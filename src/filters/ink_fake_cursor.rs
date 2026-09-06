use super::Filter;
use memchr::memchr;
use std::borrow::Cow;
use unicode_segmentation::UnicodeSegmentation;

/// Strips Ink-style fake cursor sequences and cursor-hide directives.
///
/// Ink wraps a single grapheme in inverse SGRs.  Keep the original bytes
/// until the closing SGR confirms that the candidate is a fake cursor.
#[derive(Debug, Default)]
pub struct InkFakeCursorFilter {
    pending: Vec<u8>,
    opening_len: Option<usize>,
}

impl InkFakeCursorFilter {
    /// Creates an empty filter.
    pub fn new() -> Self {
        Self::default()
    }

    fn flush_pending(&mut self, out: &mut Vec<u8>) {
        self.opening_len = None;
        if self.pending.last() == Some(&0x1b) {
            out.extend_from_slice(&self.pending[..self.pending.len() - 1]);
            self.pending.clear();
            self.pending.push(0x1b);
        } else {
            out.append(&mut self.pending);
        }
    }
}

impl Filter for InkFakeCursorFilter {
    fn filter<'a>(&mut self, data: &'a [u8]) -> Cow<'a, [u8]> {
        if self.pending.is_empty() && memchr(0x1b, data).is_none() {
            return Cow::Borrowed(data);
        }
        let mut output = Vec::with_capacity(data.len());
        for &byte in data {
            if self.pending.is_empty() {
                if byte == 0x1b {
                    self.pending.push(byte);
                } else {
                    output.push(byte);
                }
                continue;
            }

            self.pending.push(byte);
            if let Some(opening_len) = self.opening_len {
                if byte == b'm'
                    && let Some(end_start) = self.pending.iter().rposition(|&b| b == 0x1b)
                    && end_start >= opening_len
                    && let Some(end) = Sgr::parse(&self.pending[end_start..])
                    && (end.reset || end.inverse_off)
                {
                    let inner = &self.pending[opening_len..end_start];
                    if !inner.is_empty()
                        && printable_text_without_cursor_moves(inner)
                            .is_some_and(|text| text.graphemes(true).count() == 1)
                    {
                        // Only render residual SGRs after recognition succeeds.
                        Sgr::parse(&self.pending[..opening_len])
                            .expect("validated opening SGR")
                            .write_without(7, &mut output);
                        output.extend_from_slice(inner);
                        if !end.reset {
                            end.write_without(27, &mut output);
                        }
                        self.pending.clear();
                        self.opening_len = None;
                        continue;
                    }
                    self.flush_pending(&mut output);
                } else if self.pending.len() > MAX_PENDING_FAKE_CURSOR_LEN {
                    self.flush_pending(&mut output);
                }
                continue;
            }

            if self.pending == b"\x1b[?25l" {
                self.pending.clear();
            } else if is_incomplete_csi(&self.pending) || b"\x1b[?25l".starts_with(&self.pending) {
                continue;
            } else if byte == b'm'
                && self.pending.len() <= MAX_PENDING_FAKE_CURSOR_LEN
                && Sgr::parse(&self.pending).is_some_and(|sgr| sgr.inverse_on && !sgr.reset)
            {
                self.opening_len = Some(self.pending.len());
            } else {
                self.flush_pending(&mut output);
            }
        }
        Cow::Owned(output)
    }

    fn finish(&mut self) -> Vec<u8> {
        self.opening_len = None;
        std::mem::take(&mut self.pending)
    }
}

// Bound speculative buffering while allowing a full ZWJ grapheme and cursor moves.
const MAX_PENDING_FAKE_CURSOR_LEN: usize = 64;

fn csi_param_start(data: &[u8]) -> Option<usize> {
    data.starts_with(b"\x1b[").then_some(2)
}

fn is_incomplete_csi(data: &[u8]) -> bool {
    csi_param_start(data)
        .is_some_and(|start| !data[start..].iter().any(|b| (0x40..=0x7e).contains(b)))
}

/// A validated SGR borrows its original parameters; recognition allocates nothing.
struct Sgr<'a> {
    params: &'a [u8],
    reset: bool,
    inverse_on: bool,
    inverse_off: bool,
}

impl<'a> Sgr<'a> {
    fn parse(data: &'a [u8]) -> Option<Self> {
        let params = data.strip_prefix(b"\x1b[")?.strip_suffix(b"m")?;
        let mut sgr = Self {
            params,
            reset: false,
            inverse_on: false,
            inverse_off: false,
        };
        visit_sgr_params(params, |code, _| match code {
            Some(0) => sgr.reset = true,
            Some(7) => sgr.inverse_on = true,
            Some(27) => sgr.inverse_off = true,
            _ => {}
        })?;
        Some(sgr)
    }

    fn write_without(&self, removed: u16, out: &mut Vec<u8>) {
        let mut first = true;
        visit_sgr_params(self.params, |code, raw| {
            if code == Some(removed) {
                return;
            }
            if first {
                out.extend_from_slice(b"\x1b[");
                first = false;
            } else {
                out.push(b';');
            }
            out.extend_from_slice(raw);
        })
        .expect("validated SGR parameters");
        if !first {
            out.push(b'm');
        }
    }
}

/// Visits semantic parameters, keeping extended-color arguments together.
/// Colon-delimited subparameters are already contained in one raw field.
fn visit_sgr_params(params: &[u8], mut visit: impl FnMut(Option<u16>, &[u8])) -> Option<()> {
    let mut fields = params.split(|&b| b == b';');
    let mut offset = 0;
    while let Some(field) = fields.next() {
        let start = offset;
        offset += field.len() + 1;
        let code = if field.contains(&b':') {
            if !field.iter().all(|b| b.is_ascii_digit() || *b == b':') {
                return None;
            }
            None
        } else {
            if !field.iter().all(u8::is_ascii_digit) {
                return None;
            }
            let code = if field.is_empty() {
                Some(0)
            } else {
                std::str::from_utf8(field).ok()?.parse::<u16>().ok()
            };
            if matches!(code, Some(38 | 48 | 58)) {
                let mode = fields.next()?;
                offset += mode.len() + 1;
                let count = match mode {
                    b"5" => 1,
                    b"2" => 3,
                    _ => return None,
                };
                for _ in 0..count {
                    let value = fields.next()?;
                    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
                        return None;
                    }
                    offset += value.len() + 1;
                }
            }
            code
        };
        visit(code, &params[start..offset - 1]);
    }
    Some(())
}

fn printable_text_without_cursor_moves(data: &[u8]) -> Option<String> {
    let mut text = String::new();
    let mut index = 0;

    while index < data.len() {
        match data[index] {
            b'\r' | b'\n' => {
                index += 1;
            }
            0x1b => {
                index = skip_csi(data, index)?;
            }
            0x00..=0x1f | 0x7f => return None,
            _ => {
                let rest = std::str::from_utf8(&data[index..]).ok()?;
                let ch = rest.chars().next()?;
                text.push(ch);
                index += ch.len_utf8();
            }
        }
    }

    Some(text)
}

fn skip_csi(data: &[u8], index: usize) -> Option<usize> {
    let rest = data.get(index..)?;
    let param_start = csi_param_start(rest)?;
    let final_index = rest[param_start..]
        .iter()
        .position(|b| (0x40..=0x7E).contains(b))?;

    Some(index + param_start + final_index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouped_colors_survive_fake_cursor_recognition_at_every_split() {
        for (input, expected) in [
            ("\x1b[38;5;7mX\x1b[27m", "\x1b[38;5;7mX\x1b[27m"),
            ("\x1b[7mX\x1b[38;5;27m", "\x1b[7mX\x1b[38;5;27m"),
            (
                "\x1b[38;5;7;7mX\x1b[48;5;27;27m",
                "\x1b[38;5;7mX\x1b[48;5;27m",
            ),
            (
                "\x1b[38;2;0;7;27;7mX\x1b[58;2;27;0;7;27m",
                "\x1b[38;2;0;7;27mX\x1b[58;2;27;0;7m",
            ),
            (
                "\x1b[38:2::0:7:27;7m日\x1b[48:5:27;27m",
                "\x1b[38:2::0:7:27m日\x1b[48:5:27m",
            ),
            ("\x1b[38;2;7mX\x1b[27m", "\x1b[38;2;7mX\x1b[27m"),
        ] {
            for split in 0..=input.len() {
                let mut filter = InkFakeCursorFilter::new();
                let bytes = input.as_bytes();
                let mut output = filter.filter(&bytes[..split]).into_owned();
                output.extend_from_slice(&filter.filter(&bytes[split..]));
                output.extend(filter.finish());
                assert_eq!(output, expected.as_bytes(), "{input:?}, split {split}");
            }
        }
    }

    #[test]
    fn test_cursor_filter_removes_cursor_hide() {
        let mut filter = InkFakeCursorFilter::new();

        assert_eq!(filter.filter(b"a\x1b[?25lb").as_ref(), b"ab");
    }

    #[test]
    fn test_cursor_filter_removes_split_cursor_hide() {
        let mut filter = InkFakeCursorFilter::new();

        assert_eq!(filter.filter(b"a\x1b[").as_ref(), b"a");
        assert_eq!(filter.filter(b"?25l").as_ref(), b"");
        assert_eq!(filter.filter(b"b").as_ref(), b"b");
    }

    #[test]
    fn test_cursor_filter_preserves_cursor_show_when_hiding_is_suppressed() {
        let mut filter = InkFakeCursorFilter::new();

        assert_eq!(filter.filter(b"a\x1b[?25hb").as_ref(), b"a\x1b[?25hb");
    }

    #[test]
    fn test_cursor_filter_removes_fake_cursor_space() {
        let mut filter = InkFakeCursorFilter::new();

        assert_eq!(filter.filter(b"a\x1b[7m \x1b[27mb").as_ref(), b"a b");
    }

    #[test]
    fn test_cursor_filter_removes_split_fake_cursor_space() {
        let mut filter = InkFakeCursorFilter::new();

        assert_eq!(filter.filter(b"a\x1b[7m").as_ref(), b"a");
        assert_eq!(filter.filter(b" ").as_ref(), b"");
        assert_eq!(filter.filter(b"\x1b[27m").as_ref(), b" ");
        assert_eq!(filter.filter(b"b").as_ref(), b"b");
    }

    #[test]
    fn test_cursor_filter_strips_single_cell_fake_cursor_attributes() {
        let mut filter = InkFakeCursorFilter::new();

        assert_eq!(filter.filter(b"a\x1b[7mf\x1b[27mb").as_ref(), b"afb");
    }

    #[test]
    fn test_cursor_filter_preserves_inverse_keycap_text() {
        let mut filter = InkFakeCursorFilter::new();

        assert_eq!(
            filter.filter(b"a\x1b[7mEnter\x1b[27mb").as_ref(),
            b"a\x1b[7mEnter\x1b[27mb"
        );
    }

    #[test]
    fn test_cursor_filter_strips_fake_cursor_with_compound_sgr_reset_terminator() {
        let mut filter = InkFakeCursorFilter::new();
        // Ink uses \x1b[2;27m (faint + invert-off) in the very first frame.
        // The 27 is consumed; the 2 is preserved so the next cell's faint
        // attribute survives.
        let input = b"a\x1b[7mT\x1b[2;27mry";
        let expected = b"aT\x1b[2mry";
        assert_eq!(filter.filter(input).as_ref(), expected);
    }

    #[test]
    fn test_cursor_filter_strips_fake_cursor_with_color_bundled_terminator() {
        let mut filter = InkFakeCursorFilter::new();
        // Observed in the wild: Ink draws a single inverse cell, advances the
        // cursor with a newline, and then opens the next paint with an SGR
        // that bundles the inverse-off marker with a foreground color.
        let input = b"a\x1b[7m \r\n\x1b[38;5;244;27m\xe2\x94\x80b";
        let expected = b"a \r\n\x1b[38;5;244m\xe2\x94\x80b";
        assert_eq!(filter.filter(input).as_ref(), expected);
    }

    #[test]
    fn test_cursor_filter_strips_fake_cursor_with_zero_sgr_reset_terminator() {
        let mut filter = InkFakeCursorFilter::new();
        let input = b"a\x1b[7mT\x1b[0mb";
        let expected = b"aTb";
        assert_eq!(filter.filter(input).as_ref(), expected);
    }

    #[test]
    fn test_cursor_filter_preserves_unrelated_trailing_sgr() {
        // \x1b[31m doesn't include 0 or 27; not a fake cursor terminator.
        let mut filter = InkFakeCursorFilter::new();
        let input = b"a\x1b[7mEnter\x1b[27m\x1b[31mb";
        assert_eq!(filter.filter(input).as_ref(), input);
    }

    #[test]
    fn test_cursor_filter_strips_fake_cursor_with_full_sgr_reset_terminator() {
        let mut filter = InkFakeCursorFilter::new();
        // Ink uses \x1b[m (full SGR reset) as terminator in some paths.
        let input = "a\x1b[7m\u{672C}\x1b[14;5H\x1b[mb";
        let expected = "a\u{672C}\x1b[14;5Hb";
        assert_eq!(
            filter.filter(input.as_bytes()).as_ref(),
            expected.as_bytes()
        );
    }

    #[test]
    fn test_cursor_filter_strips_fake_cursor_on_fullwidth_char() {
        let mut filter = InkFakeCursorFilter::new();

        assert_eq!(
            filter
                .filter("a\x1b[7m\u{3042}\x1b[27mb".as_bytes())
                .as_ref(),
            "a\u{3042}b".as_bytes()
        );
    }

    #[test]
    fn test_cursor_filter_strips_fake_cursor_on_flag_emoji() {
        let mut filter = InkFakeCursorFilter::new();
        // \u{1F1EF}\u{1F1F5} = 🇯🇵, a single grapheme made of two code points
        let input = "a\x1b[7m\u{1F1EF}\u{1F1F5}\x1b[27mb";
        let expected = "a\u{1F1EF}\u{1F1F5}b";
        assert_eq!(
            filter.filter(input.as_bytes()).as_ref(),
            expected.as_bytes()
        );
    }

    #[test]
    fn test_cursor_filter_strips_fake_cursor_on_zwj_emoji() {
        let mut filter = InkFakeCursorFilter::new();
        // 👨‍👩‍👧‍👦 = man + ZWJ + woman + ZWJ + girl + ZWJ + boy, one grapheme
        let input = "a\x1b[7m\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}\x1b[27mb";
        let expected = "a\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}b";
        assert_eq!(
            filter.filter(input.as_bytes()).as_ref(),
            expected.as_bytes()
        );
    }

    #[test]
    fn test_cursor_filter_preserves_two_fullwidth_chars() {
        let mut filter = InkFakeCursorFilter::new();
        // Two graphemes — should not be treated as a virtual cursor
        let input = "a\x1b[7m\u{3042}\u{3044}\x1b[27mb";
        assert_eq!(filter.filter(input.as_bytes()).as_ref(), input.as_bytes());
    }

    #[test]
    fn test_cursor_filter_removes_fake_cursor_attributes_around_cursor_moves() {
        let mut filter = InkFakeCursorFilter::new();

        assert_eq!(
            filter.filter(b"a\x1b[7mf\x1b[6C\x1b[27m b").as_ref(),
            b"af\x1b[6C b"
        );
    }

    #[test]
    fn test_cursor_filter_removes_fake_cursor_attributes_across_newline() {
        let mut filter = InkFakeCursorFilter::new();

        assert_eq!(
            filter.filter(b"a\x1b[7m \r\n\x1b[27mb").as_ref(),
            b"a \r\nb"
        );
    }

    #[test]
    fn test_cursor_filter_removes_fake_cursor_attributes_across_carriage_return() {
        let mut filter = InkFakeCursorFilter::new();

        assert_eq!(
            filter.filter(b"a\x1b[7m \r\x1b[1B\x1b[27mb").as_ref(),
            b"a \r\x1b[1Bb"
        );
    }

    #[test]
    fn test_cursor_filter_preserves_multiline_inverse_text() {
        let mut filter = InkFakeCursorFilter::new();

        assert_eq!(
            filter.filter(b"a\x1b[7mline1\r\nline2\x1b[27mb").as_ref(),
            b"a\x1b[7mline1\r\nline2\x1b[27mb"
        );
    }

    #[test]
    fn test_cursor_filter_preserves_long_inverse_text() {
        let mut filter = InkFakeCursorFilter::new();

        assert_eq!(
            filter
                .filter(b"a\x1b[7m012345678901234567890123456789012\x1b[27mb")
                .as_ref(),
            b"a\x1b[7m012345678901234567890123456789012\x1b[27mb"
        );
    }

    #[test]
    fn test_cursor_filter_finish_flushes_pending_sequence() {
        let mut filter = InkFakeCursorFilter::new();

        assert_eq!(filter.filter(b"a\x1b[7m").as_ref(), b"a");
        assert_eq!(filter.finish(), b"\x1b[7m");
        assert_eq!(filter.filter(b"b").as_ref(), b"b");
    }

    #[test]
    fn test_cursor_filter_strips_fake_cursor_with_color_bundled_starter() {
        // Observed in claude (Ink) selection arrows: the inverse-on is
        // folded into an SGR that also resets the previous cell's
        // foreground (`\x1b[39;7m`). The `39` must survive so the trailing
        // text reverts to default foreground correctly.
        let mut filter = InkFakeCursorFilter::new();
        let input = b"a\x1b[38;5;246m>\x1b[39;7m \x1b[mb";
        let expected = b"a\x1b[38;5;246m>\x1b[39m b";
        assert_eq!(filter.filter(input).as_ref(), expected);
    }

    #[test]
    fn test_cursor_filter_strips_fake_cursor_with_color_bundled_starter_and_cursor_move() {
        let mut filter = InkFakeCursorFilter::new();
        let input = b"a\x1b[39;7m \x1b[78C\x1b[mb";
        let expected = b"a\x1b[39m \x1b[78Cb";
        assert_eq!(filter.filter(input).as_ref(), expected);
    }

    #[test]
    fn test_cursor_filter_strips_fake_cursor_with_color_bundled_starter_and_terminator() {
        // Both ends bundled: `\x1b[39;7m` opens, `\x1b[38;5;244;27m` closes.
        let mut filter = InkFakeCursorFilter::new();
        let input = b"a\x1b[39;7m \r\n\x1b[38;5;244;27m\xe2\x94\x80b";
        let expected = b"a\x1b[39m \r\n\x1b[38;5;244m\xe2\x94\x80b";
        assert_eq!(filter.filter(input).as_ref(), expected);
    }

    #[test]
    fn test_cursor_filter_preserves_sgr_without_inverse() {
        // An SGR that doesn't enable inverse must pass through unchanged.
        let mut filter = InkFakeCursorFilter::new();
        let input = b"a\x1b[39m b";
        assert_eq!(filter.filter(input).as_ref(), input);
    }

    #[test]
    fn test_cursor_filter_preserves_sgr_with_reset_then_inverse() {
        // `\x1b[0;7m` resets attributes first, then enables inverse — this
        // is a plain inverse opening, not a candidate for cursor stripping
        // because the leading `0` would cancel everything before it.
        let mut filter = InkFakeCursorFilter::new();
        let input = b"a\x1b[0;7mEnter\x1b[27mb";
        assert_eq!(filter.filter(input).as_ref(), input);
    }
}
