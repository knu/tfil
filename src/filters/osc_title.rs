use super::{Filter, osc::OscScanner};
use std::borrow::Cow;

/// Drops OSC 0/1/2 (icon name and window title); other OSCs retain their bytes.
#[derive(Debug, Default)]
pub struct OscTitleFilter {
    scanner: OscScanner,
}

impl OscTitleFilter {
    /// Creates an empty filter.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Filter for OscTitleFilter {
    fn filter<'a>(&mut self, data: &'a [u8]) -> Cow<'a, [u8]> {
        self.scanner.filter(data, |code, sequence, out| {
            if !matches!(code, Some(0..=2)) {
                out.extend_from_slice(sequence);
            }
        })
    }

    fn finish(&mut self) -> Vec<u8> {
        self.scanner.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_titles_with_semicolons_and_unicode_at_every_split() {
        for code in ["0", "1", "2", "0002"] {
            for terminator in ["\x07", "\x1b\\"] {
                let input = format!("before\x1b]{code};hello;日本語{terminator}after");
                for split in 0..=input.len() {
                    let mut filter = OscTitleFilter::new();
                    let mut output = filter.filter(&input.as_bytes()[..split]).into_owned();
                    output.extend_from_slice(&filter.filter(&input.as_bytes()[split..]));
                    output.extend(filter.finish());
                    assert_eq!(output, b"beforeafter", "{code}, split {split}");
                }
            }
        }
    }

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
