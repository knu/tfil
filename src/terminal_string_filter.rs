use std::borrow::Cow;
use tfil::filters::Filter;

mod auto;
pub(crate) use auto::{Auto, Endpoint};

const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const MAX_OSC: usize = 8192;

#[derive(Default)]
pub(crate) struct TerminalStringFilter {
    state: State,
    osc: Vec<u8>,
    incomplete_utf8: Vec<u8>,
    auto: Option<Auto>,
    captured: Option<Vec<u8>>,
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
    Captured(Control),
    CapturedEscape(Control),
}

#[derive(Clone, Copy)]
enum Control {
    Image,
    WindowName,
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
    pub(crate) fn auto(auto: Auto) -> Self {
        Self {
            auto: Some(auto),
            ..Self::default()
        }
    }

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
                    if self.auto.as_ref().is_some_and(Auto::is_tmux) {
                        self.captured = Some(Vec::new());
                    } else {
                        out.push(CAN);
                    }
                    State::Dcs
                }
                (State::Escape, b'_' | b'k') if self.auto.is_some() => {
                    self.captured = Some(Vec::new());
                    State::Captured(if byte == b'_' {
                        Control::Image
                    } else {
                        Control::WindowName
                    })
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
                (State::DcsEscape, b'\\') => {
                    if let Some(auto) = &mut self.auto
                        && auto.is_tmux()
                        && !self
                            .captured
                            .take()
                            .is_some_and(|data| auto.write_tmux(&data, out))
                    {
                        out.push(CAN);
                    }
                    State::Ground
                }
                // tmux treats doubled ESC and C0 as DCS payload.
                (State::Dcs | State::DcsEscape, _) => {
                    if matches!(self.state, State::DcsEscape) {
                        self.capture(ESC, auto::MAX_DCS);
                    }
                    self.capture(byte, auto::MAX_DCS);
                    State::Dcs
                }
                (State::Captured(control), ESC) => State::CapturedEscape(*control),
                (State::CapturedEscape(control), b'\\') => {
                    self.finish_capture(*control, out);
                    State::Ground
                }
                (State::Captured(Control::WindowName), 0x07) => {
                    self.finish_capture(Control::WindowName, out);
                    State::Ground
                }
                (State::Captured(_) | State::CapturedEscape(_), CAN | 0x1a) => {
                    self.captured = None;
                    out.push(CAN);
                    State::Ground
                }
                (State::CapturedEscape(_), _) => {
                    self.captured = None;
                    out.push(CAN);
                    // ESC starts a fresh sequence when it is not ST.
                    self.state = State::Escape;
                    self.scan(&[byte], out);
                    continue;
                }
                (State::Captured(control), _) => {
                    let control = *control;
                    self.capture(byte, auto::MAX_APC);
                    State::Captured(control)
                }
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
                (State::Osc, _)
                    if byte >= 0x20 && byte != 0x7f && self.osc.len() < self.osc_limit() =>
                {
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

    fn osc_limit(&self) -> usize {
        if self.auto.is_some() && self.osc.len() >= MAX_OSC {
            auto::osc_limit(
                self.osc[..5]
                    .split(|&byte| byte == b';')
                    .next()
                    .unwrap_or_default(),
            )
        } else {
            MAX_OSC
        }
    }

    fn capture(&mut self, byte: u8, limit: usize) {
        if let Some(payload) = &mut self.captured {
            if payload.len() < limit {
                payload.push(byte);
            } else {
                self.captured = None;
            }
        }
    }

    fn finish_capture(&mut self, control: Control, out: &mut Vec<u8>) {
        if let (Some(auto), Some(payload)) = (&mut self.auto, self.captured.take()) {
            let accepted = match control {
                Control::Image => auto.write_image(&payload, out),
                Control::WindowName => auto.write_window_name(&payload, out),
            };
            if accepted {
                return;
            }
        }
        out.push(CAN);
    }

    fn finish_osc(&mut self, terminator: &[u8], out: &mut Vec<u8>) {
        if let Some(auto) = &self.auto {
            if !auto.write_osc(&self.osc, terminator, false, out) {
                out.push(CAN);
            }
        } else if is_pane_osc(
            self.osc
                .split(|&byte| byte == b';')
                .next()
                .unwrap_or_default(),
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

fn is_pane_osc(code: &[u8]) -> bool {
    matches!(
        code,
        b"0" | b"1" | b"2" | b"7" | b"8" | b"10" | b"11" | b"12" | b"133"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tfil::filters::tmux_wrap;

    const IMAGE: &[u8] = b"\x1b_Ga=T,t=d,f=100,c=2,r=1,q=2,i=8388608,U=1,m=0,C=1;AAAA\x1b\\";
    const CLIPBOARD: &[u8] = b"\x1b]52;c;YQ==\x07";
    const STATUS: &[u8] = b"\x1b]7501;state=working:app=codex\x1b\\";

    fn auto(endpoint: Endpoint, focus: &Arc<AtomicBool>) -> TerminalStringFilter {
        TerminalStringFilter::auto(Auto::new(
            endpoint,
            Arc::clone(focus),
            vec![9],
            false,
            false,
        ))
    }

    #[test]
    fn tmux_auto_filters_inside_one_dcs_at_every_read_boundary() {
        let focus = Arc::new(AtomicBool::new(false));
        let bare_title = b"\x1b]0;Title\x07";
        let input =
            tmux_wrap(&[IMAGE, CLIPBOARD, bare_title, STATUS, b"\x1b]9;alert\x07"].concat());
        let mut expected = tmux_wrap(IMAGE);
        expected.extend_from_slice(bare_title);
        expected.extend(tmux_wrap(STATUS));
        for split in 0..=input.len() {
            let mut filter = auto(
                Endpoint::Tmux {
                    image_base: Some(0x80_0000),
                },
                &focus,
            );
            let mut actual = filter.filter(&input[..split]).into_owned();
            actual.extend_from_slice(&filter.filter(&input[split..]));
            assert_eq!(actual, expected, "split {split}");
        }
    }

    #[test]
    fn focus_changes_apply_to_bare_and_wrapped_clipboard() {
        let focus = Arc::new(AtomicBool::new(false));
        let mut filter = auto(
            Endpoint::Tmux {
                image_base: Some(0x80_0000),
            },
            &focus,
        );
        let wrapped = tmux_wrap(CLIPBOARD);
        let source = [CLIPBOARD, wrapped.as_slice()].concat();
        assert_eq!(filter.filter(&source).as_ref(), b"\x18\x18");
        let midpoint = wrapped.len() - 1;
        assert!(filter.filter(&wrapped[..midpoint]).is_empty());
        focus.store(true, Ordering::Relaxed);
        assert_eq!(filter.filter(&wrapped[midpoint..]).as_ref(), wrapped);
        assert_eq!(filter.filter(&source).as_ref(), source);
        focus.store(false, Ordering::Relaxed);
        assert_eq!(filter.filter(&source).as_ref(), b"\x18\x18");
    }

    #[test]
    fn herdr_auto_passes_native_images_and_status() {
        let focus = Arc::new(AtomicBool::new(false));
        let probe = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";
        let mut filter = auto(Endpoint::Herdr, &focus);
        let native = [STATUS, IMAGE, probe].concat();
        assert_eq!(filter.filter(&native).as_ref(), native);
        assert_eq!(filter.filter(&tmux_wrap(IMAGE)).as_ref(), b"\x18");
        assert_eq!(filter.filter(CLIPBOARD).as_ref(), b"\x18");
        focus.store(true, Ordering::Relaxed);
        assert_eq!(filter.filter(CLIPBOARD).as_ref(), CLIPBOARD);
    }

    #[test]
    fn tmux_auto_preserves_safe_image_continuations_and_rejects_unsafe_commands() {
        let focus = Arc::new(AtomicBool::new(true));
        let mut filter = auto(
            Endpoint::Tmux {
                image_base: Some(0x80_0000),
            },
            &focus,
        );
        let first = b"\x1b_Ga=T,t=d,f=100,c=2,r=1,q=2,i=8388608,U=1,m=1,C=1;AAAA\x1b\\";
        let last = b"\x1b_Gm=0;AAAA\x1b\\";
        assert!(filter.filter(&tmux_wrap(first)).is_empty());
        assert_eq!(
            filter.filter(&tmux_wrap(last)).as_ref(),
            tmux_wrap(&[first.as_slice(), last.as_slice()].concat())
        );
        assert!(filter.filter(&tmux_wrap(first)).is_empty());
        assert_eq!(
            filter.filter(&tmux_wrap(b"\x1b_Gm=0;!\x1b\\")).as_ref(),
            b"\x18"
        );
        for command in [
            last.as_slice(),
            b"\x1b_Ga=d,d=A,q=2\x1b\\",
            b"\x1b_Ga=d,d=I,q=2,i=8388864\x1b\\",
            b"\x1b_Ga=T,t=f,f=100,c=2,r=1,q=2,i=8388608,U=1,C=1;AAAA\x1b\\",
            b"\x1b_Ga=T,t=d,f=100,c=2,r=1,q=2,i=8388608,U=1,U=0,C=1;AAAA\x1b\\",
            b"\x1b]9;4;1;101\x07",
            b"\x1b]7501;state=oops\x1b\\",
        ] {
            assert_eq!(filter.filter(&tmux_wrap(command)).as_ref(), b"\x18");
        }
        let nested = tmux_wrap(&[STATUS, tmux_wrap(CLIPBOARD).as_slice()].concat());
        assert_eq!(filter.filter(&nested).as_ref(), b"\x18");
    }

    #[test]
    fn tmux_auto_routes_progress_and_window_name_separately() {
        let focus = Arc::new(AtomicBool::new(false));
        let mut filter = TerminalStringFilter::auto(Auto::new(
            Endpoint::Tmux { image_base: None },
            focus,
            vec![],
            true,
            true,
        ));
        let progress = b"\x1b]9;4;1;50\x07";
        let name = b"\x1bkexample\x1b\\";
        let input = [
            progress.as_slice(),
            b"\x1b]0;title\x07",
            name.as_slice(),
            b"\x07",
        ]
        .concat();
        let expected = [tmux_wrap(progress).as_slice(), b"\x18", name, b"\x07"].concat();
        assert_eq!(filter.filter(&input).as_ref(), expected);
    }

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
