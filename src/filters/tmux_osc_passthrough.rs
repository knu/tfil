use super::Filter;
use memchr::memchr;
use std::borrow::Cow;

/// Wraps selected OSC sequences in a tmux `DCS tmux; ... ST`
/// passthrough so they reach the outer terminal instead of being
/// swallowed by tmux.  Only OSCs whose numeric code is in the
/// configured list are wrapped; everything else is passed through
/// unchanged.  Requires `allow-passthrough on` in tmux (3.3+).
#[derive(Debug, Default)]
pub struct TmuxOscPassthroughFilter {
    codes: Vec<u16>,
    pending: Vec<u8>,
    state: State,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Normal,
    SawEsc,
    /// Inside an OSC; `wrap` is `None` while the numeric code is still
    /// being read.
    InOsc {
        wrap: Option<bool>,
    },
    InOscEsc {
        wrap: Option<bool>,
    },
}

impl TmuxOscPassthroughFilter {
    /// Creates a filter wrapping the given OSC codes.
    pub fn new(codes: Vec<u16>) -> Self {
        Self {
            codes,
            ..Self::default()
        }
    }

    fn decide(&self) -> bool {
        parse_osc_code(&self.pending).is_some_and(|code| self.codes.contains(&code))
    }

    fn complete(&mut self, out: &mut Vec<u8>, wrap: bool) {
        if wrap {
            out.extend_from_slice(&tmux_wrap(&self.pending));
        } else {
            out.extend_from_slice(&self.pending);
        }
        self.pending.clear();
        self.state = State::Normal;
    }
}

impl Filter for TmuxOscPassthroughFilter {
    fn filter<'a>(&mut self, data: &'a [u8]) -> Cow<'a, [u8]> {
        if self.state == State::Normal && memchr(0x1B, data).is_none() {
            return Cow::Borrowed(data);
        }

        let mut out = Vec::with_capacity(data.len());

        for &byte in data {
            match self.state {
                State::Normal => {
                    if byte == 0x1B {
                        self.pending.clear();
                        self.pending.push(byte);
                        self.state = State::SawEsc;
                    } else {
                        out.push(byte);
                    }
                }
                State::SawEsc => {
                    self.pending.push(byte);
                    if byte == b']' {
                        self.state = State::InOsc { wrap: None };
                    } else {
                        out.extend_from_slice(&self.pending);
                        self.pending.clear();
                        self.state = State::Normal;
                    }
                }
                State::InOsc { wrap } => {
                    if byte == 0x07 {
                        self.pending.push(byte);
                        let wrap = wrap.unwrap_or_else(|| self.decide());
                        self.complete(&mut out, wrap);
                    } else if byte == 0x1B {
                        let wrap = Some(wrap.unwrap_or_else(|| self.decide()));
                        self.pending.push(byte);
                        self.state = State::InOscEsc { wrap };
                    } else if wrap.is_some() {
                        self.pending.push(byte);
                    } else if byte == b';' {
                        let wrap = Some(self.decide());
                        self.pending.push(byte);
                        self.state = State::InOsc { wrap };
                    } else if (0x20..=0x7E).contains(&byte) {
                        self.pending.push(byte);
                        if !byte.is_ascii_digit() {
                            // Non-numeric OSC: never wrapped.
                            self.state = State::InOsc { wrap: Some(false) };
                        }
                    } else {
                        // Not a well-formed OSC: flush as-is and reset.
                        self.pending.push(byte);
                        out.extend_from_slice(&self.pending);
                        self.pending.clear();
                        self.state = State::Normal;
                    }
                }
                State::InOscEsc { wrap } => {
                    self.pending.push(byte);
                    if byte == b'\\' {
                        self.complete(&mut out, wrap.unwrap_or(false));
                    } else {
                        // Stray ESC inside OSC: treat as payload.
                        self.state = State::InOsc { wrap };
                    }
                }
            }
        }

        Cow::Owned(out)
    }

    fn finish(&mut self) -> Vec<u8> {
        self.state = State::Normal;
        std::mem::take(&mut self.pending)
    }
}

/// Numeric code of a buffered OSC (`ESC ] <digits> ...`), if any.
fn parse_osc_code(pending: &[u8]) -> Option<u16> {
    let digits: Vec<u8> = pending
        .get(2..)?
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .copied()
        .collect();
    std::str::from_utf8(&digits).ok()?.parse().ok()
}

/// Wraps a complete escape sequence in a tmux DCS passthrough,
/// doubling every ESC in the payload as tmux requires.
pub fn tmux_wrap(seq: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(seq.len() + 16);
    out.extend_from_slice(b"\x1bPtmux;");
    for &b in seq {
        if b == 0x1B {
            out.push(0x1B);
        }
        out.push(b);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f22() -> TmuxOscPassthroughFilter {
        TmuxOscPassthroughFilter::new(vec![22])
    }

    #[test]
    fn wraps_listed_osc_with_bel() {
        let mut f = f22();
        assert_eq!(
            f.filter(b"a\x1b]22;pointer\x07b").as_ref(),
            b"a\x1bPtmux;\x1b\x1b]22;pointer\x07\x1b\\b"
        );
    }

    #[test]
    fn wraps_listed_osc_with_st() {
        let mut f = f22();
        assert_eq!(
            f.filter(b"\x1b]22;pointer\x1b\\").as_ref(),
            b"\x1bPtmux;\x1b\x1b]22;pointer\x1b\x1b\\\x1b\\"
        );
    }

    #[test]
    fn wraps_osc_without_semicolon() {
        let mut f = f22();
        assert_eq!(
            f.filter(b"\x1b]22\x07").as_ref(),
            b"\x1bPtmux;\x1b\x1b]22\x07\x1b\\"
        );
    }

    #[test]
    fn passes_unlisted_osc() {
        let mut f = f22();
        let input = b"\x1b]8;;https://example.com\x1b\\link";
        assert_eq!(f.filter(input).as_ref(), input);
    }

    #[test]
    fn does_not_wrap_code_with_matching_prefix() {
        let mut f = TmuxOscPassthroughFilter::new(vec![2]);
        let input = b"\x1b]22;pointer\x07";
        assert_eq!(f.filter(input).as_ref(), input);
    }

    #[test]
    fn passes_non_numeric_osc() {
        let mut f = f22();
        let input = b"\x1b]P1ff0000\x07";
        assert_eq!(f.filter(input).as_ref(), input);
    }

    #[test]
    fn passes_csi_and_plain_text() {
        let mut f = f22();
        let input = b"hi\x1b[1;31mred\x1b[m";
        assert_eq!(f.filter(input).as_ref(), input);
    }

    #[test]
    fn handles_split_across_chunks() {
        let mut f = f22();
        assert_eq!(f.filter(b"a\x1b]22;po").as_ref(), b"a");
        assert_eq!(
            f.filter(b"inter\x07b").as_ref(),
            b"\x1bPtmux;\x1b\x1b]22;pointer\x07\x1b\\b"
        );
    }

    #[test]
    fn handles_split_st_terminator() {
        let mut f = f22();
        assert_eq!(f.filter(b"\x1b]22;default\x1b").as_ref(), b"");
        assert_eq!(
            f.filter(b"\\x").as_ref(),
            b"\x1bPtmux;\x1b\x1b]22;default\x1b\x1b\\\x1b\\x"
        );
    }

    #[test]
    fn flushes_unfinished_osc_on_finish() {
        let mut f = f22();
        assert_eq!(f.filter(b"\x1b]22;par").as_ref(), b"");
        assert_eq!(f.finish(), b"\x1b]22;par");
    }

    #[test]
    fn tmux_wrap_doubles_escapes() {
        assert_eq!(
            tmux_wrap(b"\x1b]22;pointer\x1b\\"),
            b"\x1bPtmux;\x1b\x1b]22;pointer\x1b\x1b\\\x1b\\"
        );
    }
}
