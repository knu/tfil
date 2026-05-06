use super::Filter;
use memchr::memchr;
use std::borrow::Cow;

/// Drops OSC 0/1/2 sequences (icon name and window title). Other OSCs
/// (palette, hyperlinks, clipboard, ...) are passed through. Both ST
/// (`ESC \`) and BEL terminators are recognized.
#[derive(Debug, Default)]
pub struct OscTitleFilter {
    pending: Vec<u8>,
    state: State,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Normal,
    SawEsc,
    InOsc {
        drop: bool,
    },
    InOscEsc {
        drop: bool,
    },
}

impl OscTitleFilter {
    /// Creates an empty filter.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Filter for OscTitleFilter {
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
                        self.state = State::InOsc { drop: false };
                    } else {
                        out.extend_from_slice(&self.pending);
                        self.pending.clear();
                        self.state = State::Normal;
                    }
                }
                State::InOsc { drop } => {
                    let mut drop = drop;
                    self.pending.push(byte);
                    // Decide drop status as soon as we have read the parameter
                    // and its terminating ';'. Until then, the buffer is
                    // "ESC ] <digits>" with no decision yet.
                    if byte == b';' {
                        let param = &self.pending[2..self.pending.len() - 1];
                        drop = matches!(param, b"0" | b"1" | b"2");
                        self.state = State::InOsc { drop };
                    } else if byte == 0x07 {
                        if !drop {
                            out.extend_from_slice(&self.pending);
                        }
                        self.pending.clear();
                        self.state = State::Normal;
                    } else if byte == 0x1B {
                        self.state = State::InOscEsc { drop };
                    } else if !is_osc_param_byte(byte) {
                        // Unexpected byte before ';': not an OSC we recognize.
                        // Flush as-is and reset.
                        out.extend_from_slice(&self.pending);
                        self.pending.clear();
                        self.state = State::Normal;
                    } else {
                        self.state = State::InOsc { drop };
                    }
                }
                State::InOscEsc { drop } => {
                    self.pending.push(byte);
                    if byte == b'\\' {
                        if !drop {
                            out.extend_from_slice(&self.pending);
                        }
                        self.pending.clear();
                        self.state = State::Normal;
                    } else {
                        // Stray ESC inside OSC: stay in OSC, treat ESC as
                        // part of the payload and continue.
                        self.state = State::InOsc { drop };
                    }
                }
            }
        }

        Cow::Owned(out)
    }

    fn finish(&mut self) -> Vec<u8> {
        let pending = std::mem::take(&mut self.pending);
        self.state = State::Normal;
        pending
    }
}

fn is_osc_param_byte(b: u8) -> bool {
    // OSC parameters are typically digits, but we accept any printable
    // ASCII before the first ';' so we don't accidentally swallow exotic
    // OSCs. The decision to drop only fires when param is "0", "1", or "2".
    (0x20..=0x7E).contains(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_osc0_title_with_bel() {
        let mut f = OscTitleFilter::new();
        assert_eq!(f.filter(b"a\x1b]0;hello\x07b").as_ref(), b"ab");
    }

    #[test]
    fn strips_osc2_title_with_st() {
        let mut f = OscTitleFilter::new();
        assert_eq!(f.filter(b"a\x1b]2;title\x1b\\b").as_ref(), b"ab");
    }

    #[test]
    fn strips_osc1_icon_with_bel() {
        let mut f = OscTitleFilter::new();
        assert_eq!(f.filter(b"\x1b]1;icon\x07tail").as_ref(), b"tail");
    }

    #[test]
    fn preserves_osc4_palette() {
        let mut f = OscTitleFilter::new();
        let input = b"\x1b]4;5;rgb:00/00/00\x07x";
        assert_eq!(f.filter(input).as_ref(), input);
    }

    #[test]
    fn preserves_osc8_hyperlink_with_st() {
        let mut f = OscTitleFilter::new();
        let input = b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\";
        assert_eq!(f.filter(input).as_ref(), input);
    }

    #[test]
    fn passes_through_no_escape_data() {
        let mut f = OscTitleFilter::new();
        let out = f.filter(b"plain text");
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), b"plain text");
    }

    #[test]
    fn handles_split_across_chunks() {
        let mut f = OscTitleFilter::new();
        assert_eq!(f.filter(b"a\x1b]0;hel").as_ref(), b"a");
        assert_eq!(f.filter(b"lo\x07b").as_ref(), b"b");
    }

    #[test]
    fn handles_split_st_terminator() {
        let mut f = OscTitleFilter::new();
        assert_eq!(f.filter(b"a\x1b]2;title\x1b").as_ref(), b"a");
        assert_eq!(f.filter(b"\\b").as_ref(), b"b");
    }

    #[test]
    fn flushes_unfinished_osc_on_finish() {
        let mut f = OscTitleFilter::new();
        assert_eq!(f.filter(b"\x1b]0;partial").as_ref(), b"");
        assert_eq!(f.finish(), b"\x1b]0;partial");
    }

    #[test]
    fn lone_escape_then_normal_byte_is_passed_through() {
        let mut f = OscTitleFilter::new();
        // ESC followed by something that isn't ']' must not be eaten.
        assert_eq!(f.filter(b"\x1bOA").as_ref(), b"\x1bOA");
    }
}
