use super::{Filter, osc::OscScanner};
use std::borrow::Cow;

/// Wraps selected OSCs in a tmux DCS passthrough.  Requires
/// allow-passthrough on in tmux (3.3+).
#[derive(Debug, Default)]
pub struct TmuxOscPassthroughFilter {
    codes: Vec<u16>,
    scanner: OscScanner,
}

impl TmuxOscPassthroughFilter {
    /// Creates a filter wrapping the given OSC codes.
    pub fn new(codes: Vec<u16>) -> Self {
        Self {
            codes,
            scanner: OscScanner::default(),
        }
    }
}

impl Filter for TmuxOscPassthroughFilter {
    fn filter<'a>(&mut self, data: &'a [u8]) -> Cow<'a, [u8]> {
        self.scanner.filter(data, |code, sequence, out| {
            if code.is_some_and(|code| self.codes.contains(&code)) {
                tmux_wrap_into(sequence, out);
            } else {
                out.extend_from_slice(sequence);
            }
        })
    }

    fn finish(&mut self) -> Vec<u8> {
        self.scanner.finish()
    }
}

/// Wraps a complete escape sequence in a tmux DCS passthrough,
/// doubling every ESC in the payload as tmux requires.
pub fn tmux_wrap(seq: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(seq.len() + 16);
    tmux_wrap_into(seq, &mut out);
    out
}

fn tmux_wrap_into(seq: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1bPtmux;");
    for &b in seq {
        if b == 0x1b {
            out.push(0x1b);
        }
        out.push(b);
    }
    out.extend_from_slice(b"\x1b\\");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f22() -> TmuxOscPassthroughFilter {
        TmuxOscPassthroughFilter::new(vec![22])
    }

    #[test]
    fn wraps_unicode_payload_without_changing_its_bytes() {
        let input = "\x1b]22;日本語;pointer\x1b\\".as_bytes();
        let expected = tmux_wrap(input);
        for split in 0..=input.len() {
            let mut filter = f22();
            let mut output = filter.filter(&input[..split]).into_owned();
            output.extend_from_slice(&filter.filter(&input[split..]));
            output.extend(filter.finish());
            assert_eq!(output, expected);
        }
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
