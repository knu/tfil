use super::Filter;
use memchr::memchr;
use std::borrow::Cow;
use unicode_segmentation::UnicodeSegmentation;

/// Strips Ink-style fake cursor sequences and the cursor-hide
/// directive (`\x1b[?25l`).
///
/// Ink draws its own block cursor by wrapping a single grapheme cluster
/// in `\x1b[7m...\x1b[27m` (or `\x1b[m`) while keeping the real cursor
/// hidden. This filter removes those wrappers so the terminal's native
/// cursor shows through.
#[derive(Debug, Default)]
pub struct InkFakeCursorFilter {
    pending: Vec<u8>,
}

impl InkFakeCursorFilter {
    /// Creates an empty filter.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Filter for InkFakeCursorFilter {
    fn filter<'a>(&mut self, data: &'a [u8]) -> Cow<'a, [u8]> {
        if self.pending.is_empty() && memchr(0x1B, data).is_none() {
            return Cow::Borrowed(data);
        }

        let mut output = Vec::with_capacity(data.len());

        for &byte in data {
            if self.pending.is_empty() {
                if byte == 0x1B {
                    self.pending.push(byte);
                } else {
                    output.push(byte);
                }
                continue;
            }

            self.pending.push(byte);

            if is_cursor_hide(&self.pending) {
                self.pending.clear();
                continue;
            }

            if is_fake_cursor(&self.pending) {
                output.extend_from_slice(fake_cursor_inner(&self.pending));
                self.pending.clear();
                continue;
            }

            if is_incomplete_csi(&self.pending)
                || is_cursor_hide_prefix(&self.pending)
                || is_fake_cursor_prefix(&self.pending)
            {
                continue;
            }

            if byte == 0x1B {
                output.extend_from_slice(&self.pending[..self.pending.len() - 1]);
                self.pending.clear();
                self.pending.push(byte);
                continue;
            }

            output.extend_from_slice(&self.pending);
            self.pending.clear();
        }

        Cow::Owned(output)
    }

    fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

// Inner payload is at most one grapheme cluster, possibly preceded by CR and
// CSI cursor-move sequences. ZWJ emoji clusters can run up to ~28 bytes (e.g.
// the 4-person family), so cap generously.
const MAX_PENDING_FAKE_CURSOR_LEN: usize = 64;

fn is_incomplete_csi(data: &[u8]) -> bool {
    let Some(param_start) = csi_param_start(data) else {
        return false;
    };

    !data[param_start..]
        .iter()
        .any(|b| (0x40..=0x7E).contains(b))
}

fn csi_param_start(data: &[u8]) -> Option<usize> {
    if data.starts_with(b"\x1b[") {
        Some(2)
    } else {
        None
    }
}

fn is_cursor_hide(data: &[u8]) -> bool {
    data == b"\x1b[?25l"
}

fn is_cursor_hide_prefix(data: &[u8]) -> bool {
    b"\x1b[?25l".starts_with(data)
}

const FAKE_CURSOR_START: &[u8] = b"\x1b[7m";

/// Length of the trailing SGR sequence that closes a fake cursor.
///
/// Ink uses several variants to leave the inverse-attribute state:
/// `\x1b[m` (full reset), `\x1b[0m`, `\x1b[27m`, `\x1b[2;27m`, etc. We
/// recognize any SGR sequence whose semicolon-separated numeric
/// parameter list contains `0` or `27`.
fn fake_cursor_end_len(data: &[u8]) -> Option<usize> {
    if !data.ends_with(b"m") {
        return None;
    }
    // Walk back to find the matching ESC '['.
    let bytes = &data[..data.len() - 1];
    let start = bytes.iter().rposition(|&b| b == 0x1B)?;
    if !bytes[start..].starts_with(b"\x1b[") {
        return None;
    }
    let params = &bytes[start + 2..];
    let mut found = params.is_empty(); // bare \x1b[m == full reset
    for part in params.split(|&b| b == b';') {
        if !part.iter().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if matches!(part, b"0" | b"27") || (part.is_empty() && !found) {
            found = true;
        }
    }
    if found {
        Some(data.len() - start)
    } else {
        None
    }
}

fn is_fake_cursor(data: &[u8]) -> bool {
    if !data.starts_with(FAKE_CURSOR_START) {
        return false;
    }
    let Some(end_len) = fake_cursor_end_len(data) else {
        return false;
    };
    is_fake_cursor_inner(&data[FAKE_CURSOR_START.len()..data.len() - end_len])
}

fn is_fake_cursor_prefix(data: &[u8]) -> bool {
    (FAKE_CURSOR_START.starts_with(data) && data.len() < FAKE_CURSOR_START.len())
        || (data.starts_with(FAKE_CURSOR_START)
            && data.len() <= MAX_PENDING_FAKE_CURSOR_LEN
            && !data[FAKE_CURSOR_START.len()..].contains(&b'\n')
            && fake_cursor_end_len(data).is_none())
}

fn is_fake_cursor_inner(data: &[u8]) -> bool {
    !data.is_empty()
        && !data.contains(&b'\n')
        && printable_text_without_cursor_moves(data)
            .is_some_and(|text| text.graphemes(true).count() == 1)
}

fn fake_cursor_inner(data: &[u8]) -> &[u8] {
    let end_len = fake_cursor_end_len(data).unwrap_or(0);
    &data[FAKE_CURSOR_START.len()..data.len() - end_len]
}

fn printable_text_without_cursor_moves(data: &[u8]) -> Option<String> {
    let mut text = String::new();
    let mut index = 0;

    while index < data.len() {
        match data[index] {
            b'\r' => {
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
        let input = b"a\x1b[7mT\x1b[2;27mry";
        let expected = b"aTry";
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
}
