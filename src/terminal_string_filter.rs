use std::borrow::Cow;
use tfil::filters::Filter;

const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const MAX_OSC: usize = 8192;

#[derive(Default)]
pub(crate) struct TerminalStringFilter {
    state: State,
    osc: Vec<u8>,
    incomplete_utf8: Vec<u8>,
}

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Escape,
    Dcs,
    DcsEscape,
    String,
    Osc,
    OscEscape,
}

impl Filter for TerminalStringFilter {
    fn filter<'a>(&mut self, data: &'a [u8]) -> Cow<'a, [u8]> {
        let mut out = Vec::with_capacity(data.len());
        let mut continued_utf8 = std::mem::take(&mut self.incomplete_utf8);
        let mut remaining = if continued_utf8.is_empty() {
            data
        } else {
            continued_utf8.extend_from_slice(data);
            &continued_utf8
        };
        while !remaining.is_empty() {
            match std::str::from_utf8(remaining) {
                Ok(text) => {
                    self.scan_text(text, &mut out);
                    break;
                }
                Err(error) => {
                    let (valid, rest) = remaining.split_at(error.valid_up_to());
                    self.scan_text(
                        std::str::from_utf8(valid).expect("valid UTF-8 prefix"),
                        &mut out,
                    );
                    if let Some(length) = error.error_len() {
                        for &byte in &rest[..length] {
                            if (0x80..=0x9f).contains(&byte) {
                                self.scan_c1(byte, &mut out);
                            } else {
                                self.scan(&[byte], &mut out);
                            }
                        }
                        remaining = &rest[length..];
                    } else {
                        self.incomplete_utf8.extend_from_slice(rest);
                        break;
                    }
                }
            }
        }
        Cow::Owned(out)
    }
}

impl TerminalStringFilter {
    fn scan_text(&mut self, text: &str, out: &mut Vec<u8>) {
        let mut start = 0;
        for (index, ch) in text.char_indices() {
            if ('\u{80}'..='\u{9f}').contains(&ch) {
                self.scan(&text.as_bytes()[start..index], out);
                self.scan_c1(ch as u8, out);
                start = index + ch.len_utf8();
            }
        }
        self.scan(&text.as_bytes()[start..], out);
    }

    fn scan_c1(&mut self, byte: u8, out: &mut Vec<u8>) {
        self.scan(&[ESC, byte - 0x40], out);
    }

    fn scan(&mut self, data: &[u8], out: &mut Vec<u8>) {
        for &byte in data {
            if matches!(self.state, State::OscEscape) && byte != b'\\' {
                self.osc.clear();
                out.push(CAN);
                self.state = State::Escape;
            }
            self.state = match (&self.state, byte) {
                (State::Ground | State::Escape, ESC) => State::Escape,
                // tmux accepts C0 controls between ESC and a sequence introducer.
                (State::Escape, CAN | 0x1a) => {
                    out.push(byte);
                    State::Ground
                }
                (State::Escape, 0x00..=0x1f | 0x7f) => State::Escape,
                (State::Escape, b'P') => {
                    out.push(CAN);
                    State::Dcs
                }
                (State::Escape, b'_' | b'^' | b'X' | b'k') => {
                    out.push(CAN);
                    State::String
                }
                (State::Escape, b']') => State::Osc,
                (State::Escape, _) => {
                    out.extend([ESC, byte]);
                    State::Ground
                }
                (State::Dcs, ESC) => State::DcsEscape,
                (State::DcsEscape, b'\\') => State::Ground,
                // tmux treats doubled ESC and C0 as DCS payload.
                (State::Dcs | State::DcsEscape, _) => State::Dcs,
                (State::String, ESC) => State::Escape,
                (State::String, CAN | 0x1a | 0x07) => State::Ground,
                (State::String, _) => State::String,
                (State::Osc, 0x07) => {
                    self.finish_osc(b"\x07", out);
                    State::Ground
                }
                (State::Osc, ESC) => State::OscEscape,
                (State::OscEscape, b'\\') => {
                    self.finish_osc(b"\x1b\\", out);
                    State::Ground
                }
                (State::Osc, CAN | 0x1a) => {
                    self.osc.clear();
                    out.push(CAN);
                    State::Ground
                }
                (State::OscEscape, _) => unreachable!("aborted OSC handled above"),
                (State::Osc, _) if self.osc.len() < MAX_OSC && byte >= 0x20 && byte != 0x7f => {
                    self.osc.push(byte);
                    State::Osc
                }
                (State::Osc, _) => {
                    self.osc.clear();
                    out.push(CAN);
                    State::String
                }
                (State::Ground, _) => {
                    out.push(byte);
                    State::Ground
                }
            };
        }
    }

    fn finish_osc(&mut self, terminator: &[u8], out: &mut Vec<u8>) {
        if matches!(
            self.osc.split(|&byte| byte == b';').next(),
            Some(b"0" | b"1" | b"2" | b"7" | b"8" | b"10" | b"11" | b"12" | b"133")
        ) {
            out.extend_from_slice(b"\x1b]");
            out.extend_from_slice(&self.osc);
            out.extend_from_slice(terminator);
        } else {
            out.push(CAN);
        }
        self.osc.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_child_controls_across_every_chunk_boundary() {
        let input = b"a\x1bPtmux;\x1b\x1b]52;x\x1b\x1b\\\x1b\\b\x1b]5522;x\x1b\\\x1b]777;x\x07\x1b]8;;https://example.com\x1b\\c\x1b[31md";
        let expected = b"a\x18b\x18\x18\x1b]8;;https://example.com\x1b\\c\x1b[31md";
        for split in 0..=input.len() {
            let mut filter = TerminalStringFilter::default();
            let mut output = filter.filter(&input[..split]).into_owned();
            output.extend_from_slice(&filter.filter(&input[split..]));
            output.extend(filter.finish());
            assert_eq!(output, expected, "split {split}");
        }
    }

    #[test]
    fn filters_dcs_introduced_after_an_aborted_osc_or_embedded_c0() {
        let mut filter = TerminalStringFilter::default();
        assert_eq!(
            filter
                .filter(b"\x1b]0;title\x1b\nPtmux;payload\x1b\\ok")
                .as_ref(),
            b"\x18\x18ok",
        );
    }

    #[test]
    fn drops_c1_control_strings_and_preserves_utf8_at_every_split() {
        let input = [
            "Đ".as_bytes(),
            b"\x90tmux;payload\x9c",
            "\u{9d}52;clipboard\u{9c}🐐".as_bytes(),
        ]
        .concat();
        for split in 0..=input.len() {
            let mut filter = TerminalStringFilter::default();
            let mut output = filter.filter(&input[..split]).into_owned();
            output.extend_from_slice(&filter.filter(&input[split..]));
            assert_eq!(output, "Đ\x18\x18🐐".as_bytes(), "split {split}");
        }
    }
}
