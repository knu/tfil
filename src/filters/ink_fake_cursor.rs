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
                let payload = fake_cursor_payload(&self.pending);
                if let Some(leading) = payload.leading_residual {
                    output.extend_from_slice(&leading);
                }
                output.extend_from_slice(payload.inner);
                if let Some(trailing) = payload.trailing_residual {
                    output.extend_from_slice(&trailing);
                }
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

/// Shortest possible inverse-enabling SGR. Used only for prefix tracking
/// while bytes are still arriving; classification uses [`fake_cursor_start`].
const MIN_FAKE_CURSOR_START: &[u8] = b"\x1b[7m";

/// Describes the leading SGR that enables inverse for a fake cursor.
///
/// Ink usually emits `\x1b[7m` on its own, but it sometimes folds the
/// inverse-on into a compound SGR that also resets the previous cell's
/// attributes, e.g. `\x1b[39;7m` (default foreground + inverse-on). We
/// strip the `7` and keep any remaining parameters as `residual` so the
/// surrounding attribute state still resolves correctly.
struct FakeCursorStart {
    len: usize,
    residual: Option<Vec<u8>>,
}

/// Describes the trailing SGR sequence that closes a fake cursor.
///
/// `len` is the byte length of that SGR (including the leading ESC `[`
/// and trailing `m`). `residual` is `Some(rewritten)` when the SGR
/// carried unrelated parameters that must be preserved in the output
/// after stripping the inverse-off marker — for example `\x1b[38;5;244;27m`
/// becomes `\x1b[38;5;244m`. It is `None` when the entire SGR was just a
/// reset (`\x1b[m`, `\x1b[0m`, `\x1b[27m`, `\x1b[2;27m`, etc.) and can
/// be discarded outright.
struct FakeCursorEnd {
    len: usize,
    residual: Option<Vec<u8>>,
}

/// Recognizes the leading inverse-on SGR. Returns `Some` when `data`
/// begins with an SGR whose parameter list contains `7` (and no `0`
/// reset — a reset would cancel the inverse before any text). The
/// returned `residual` re-emits the non-`7` parameters as their own SGR
/// so the surrounding attribute state survives.
fn fake_cursor_start(data: &[u8]) -> Option<FakeCursorStart> {
    if !data.starts_with(b"\x1b[") {
        return None;
    }
    let after_intro = &data[2..];
    let final_index = after_intro.iter().position(|b| (0x40..=0x7E).contains(b))?;
    if after_intro[final_index] != b'm' {
        return None;
    }
    let params = &after_intro[..final_index];
    let mut found = false;
    let mut kept: Vec<&[u8]> = Vec::new();
    for part in params.split(|&b| b == b';') {
        if !part.iter().all(|b| b.is_ascii_digit()) {
            return None;
        }
        match part {
            b"7" => {
                found = true;
            }
            // A `0` (or empty, which terminals treat as 0) inside the
            // start SGR would reset attributes after enabling inverse,
            // so it cannot mark the opening of a fake cursor.
            b"0" | b"" => return None,
            _ => kept.push(part),
        }
    }
    if !found {
        return None;
    }
    let residual = if kept.is_empty() {
        None
    } else {
        let mut out = Vec::with_capacity(3 + kept.iter().map(|p| p.len() + 1).sum::<usize>());
        out.extend_from_slice(b"\x1b[");
        for (i, part) in kept.iter().enumerate() {
            if i > 0 {
                out.push(b';');
            }
            out.extend_from_slice(part);
        }
        out.push(b'm');
        Some(out)
    };
    Some(FakeCursorStart {
        len: 2 + final_index + 1,
        residual,
    })
}

/// True when `data` could still grow into a fake-cursor opening SGR.
fn is_fake_cursor_start_prefix(data: &[u8]) -> bool {
    if !data.starts_with(b"\x1b") {
        return false;
    }
    if data == b"\x1b" {
        return true;
    }
    if !data.starts_with(b"\x1b[") {
        return false;
    }
    let params = &data[2..];
    // No final byte yet: still parameters/intermediates; must remain valid digits or ';'.
    params.iter().all(|b| b.is_ascii_digit() || *b == b';')
}

/// Ink uses several variants to leave the inverse-attribute state:
/// `\x1b[m` (full reset), `\x1b[0m`, `\x1b[27m`, `\x1b[2;27m`, or even
/// SGRs that bundle inverse-off with the next cell's attributes such as
/// `\x1b[38;5;244;27m`. We recognize any SGR whose parameter list
/// contains `0` or `27` (or is empty, i.e. a bare reset).
fn fake_cursor_end(data: &[u8]) -> Option<FakeCursorEnd> {
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
    if params.is_empty() {
        return Some(FakeCursorEnd {
            len: data.len() - start,
            residual: None,
        });
    }
    let mut found = false;
    let mut kept: Vec<&[u8]> = Vec::new();
    let mut full_reset = false;
    for part in params.split(|&b| b == b';') {
        if !part.iter().all(|b| b.is_ascii_digit()) {
            return None;
        }
        match part {
            b"0" => {
                found = true;
                full_reset = true;
            }
            b"27" => {
                found = true;
            }
            b"" => {
                // Empty parameter inside a list (e.g. `;;`) is treated as 0
                // by terminals; keep parity with the discard path.
                found = true;
                full_reset = true;
            }
            _ => kept.push(part),
        }
    }
    if !found {
        return None;
    }
    let residual = if full_reset || kept.is_empty() {
        None
    } else {
        let mut out = Vec::with_capacity(3 + kept.iter().map(|p| p.len() + 1).sum::<usize>());
        out.extend_from_slice(b"\x1b[");
        for (i, part) in kept.iter().enumerate() {
            if i > 0 {
                out.push(b';');
            }
            out.extend_from_slice(part);
        }
        out.push(b'm');
        Some(out)
    };
    Some(FakeCursorEnd {
        len: data.len() - start,
        residual,
    })
}

fn is_fake_cursor(data: &[u8]) -> bool {
    let Some(start) = fake_cursor_start(data) else {
        return false;
    };
    let Some(end) = fake_cursor_end(data) else {
        return false;
    };
    if start.len + end.len > data.len() {
        return false;
    }
    is_fake_cursor_inner(&data[start.len..data.len() - end.len])
}

fn is_fake_cursor_prefix(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    if is_fake_cursor_start_prefix(data) && fake_cursor_start(data).is_none() {
        return true;
    }
    if fake_cursor_start(data).is_some()
        && data.len() <= MAX_PENDING_FAKE_CURSOR_LEN
        && fake_cursor_end(data).is_none()
    {
        return true;
    }
    // Also accept the legacy minimum prefix (`\x1b`, `\x1b[`, `\x1b[7`)
    // so we keep buffering until enough bytes arrive to classify.
    MIN_FAKE_CURSOR_START.starts_with(data) && data.len() < MIN_FAKE_CURSOR_START.len()
}

fn is_fake_cursor_inner(data: &[u8]) -> bool {
    !data.is_empty()
        && printable_text_without_cursor_moves(data)
            .is_some_and(|text| text.graphemes(true).count() == 1)
}

struct FakeCursorPayload<'a> {
    leading_residual: Option<Vec<u8>>,
    inner: &'a [u8],
    trailing_residual: Option<Vec<u8>>,
}

fn fake_cursor_payload(data: &[u8]) -> FakeCursorPayload<'_> {
    let (start_len, leading_residual) = match fake_cursor_start(data) {
        Some(start) => (start.len, start.residual),
        None => (0, None),
    };
    let (end_len, trailing_residual) = match fake_cursor_end(data) {
        Some(end) => (end.len, end.residual),
        None => (0, None),
    };
    FakeCursorPayload {
        leading_residual,
        inner: &data[start_len..data.len() - end_len],
        trailing_residual,
    }
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
