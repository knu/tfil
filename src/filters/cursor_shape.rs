use super::Filter;
use memchr::memchr;
use std::borrow::Cow;

/// Drops DECSCUSR (`CSI Pn SP q`) so child programs cannot change the
/// terminal's cursor shape. Other CSI sequences are passed through.
#[derive(Debug, Default)]
pub struct CursorShapeFilter {
    pending: Vec<u8>,
    state: State,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Normal,
    SawEsc,
    InCsi,
}

impl CursorShapeFilter {
    /// Creates an empty filter.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Filter for CursorShapeFilter {
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
                    if byte == b'[' {
                        self.state = State::InCsi;
                    } else {
                        out.extend_from_slice(&self.pending);
                        self.pending.clear();
                        self.state = State::Normal;
                    }
                }
                State::InCsi => {
                    self.pending.push(byte);
                    // CSI = ESC [ <param 0x30-0x3F>* <inter 0x20-0x2F>* <final 0x40-0x7E>
                    if (0x40..=0x7E).contains(&byte) {
                        if is_decscusr(&self.pending) {
                            // drop
                        } else {
                            out.extend_from_slice(&self.pending);
                        }
                        self.pending.clear();
                        self.state = State::Normal;
                    }
                    // else: still in params/intermediates, keep buffering
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

fn is_decscusr(csi: &[u8]) -> bool {
    // Expect: ESC [ <digits>* SP q
    if csi.len() < 4 {
        return false;
    }
    if !csi.starts_with(b"\x1b[") {
        return false;
    }
    if !csi.ends_with(b" q") {
        return false;
    }
    let params = &csi[2..csi.len() - 2];
    params.iter().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_decscusr_default() {
        let mut f = CursorShapeFilter::new();
        assert_eq!(f.filter(b"a\x1b[ qb").as_ref(), b"ab");
    }

    #[test]
    fn strips_decscusr_with_param() {
        let mut f = CursorShapeFilter::new();
        assert_eq!(f.filter(b"a\x1b[5 qb").as_ref(), b"ab");
    }

    #[test]
    fn strips_decscusr_double_digit() {
        let mut f = CursorShapeFilter::new();
        assert_eq!(f.filter(b"\x1b[12 q").as_ref(), b"");
    }

    #[test]
    fn preserves_other_csi_with_space_intermediate() {
        // CSI 1 SP @ is SL (scroll left) — not DECSCUSR. Must not be dropped.
        let mut f = CursorShapeFilter::new();
        assert_eq!(f.filter(b"\x1b[1 @").as_ref(), b"\x1b[1 @");
    }

    #[test]
    fn preserves_sgr() {
        let mut f = CursorShapeFilter::new();
        assert_eq!(
            f.filter(b"\x1b[1;31mhi\x1b[m").as_ref(),
            b"\x1b[1;31mhi\x1b[m"
        );
    }

    #[test]
    fn preserves_cursor_move() {
        let mut f = CursorShapeFilter::new();
        assert_eq!(f.filter(b"\x1b[14;5H").as_ref(), b"\x1b[14;5H");
    }

    #[test]
    fn passes_through_no_escape_data() {
        let mut f = CursorShapeFilter::new();
        let out = f.filter(b"plain text");
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), b"plain text");
    }

    #[test]
    fn handles_split_across_chunks() {
        let mut f = CursorShapeFilter::new();
        assert_eq!(f.filter(b"a\x1b[5").as_ref(), b"a");
        assert_eq!(f.filter(b" q").as_ref(), b"");
        assert_eq!(f.filter(b"b").as_ref(), b"b");
    }

    #[test]
    fn flushes_unfinished_csi_on_finish() {
        let mut f = CursorShapeFilter::new();
        assert_eq!(f.filter(b"\x1b[5").as_ref(), b"");
        assert_eq!(f.finish(), b"\x1b[5");
    }

    #[test]
    fn lone_escape_then_normal_byte_is_passed_through() {
        let mut f = CursorShapeFilter::new();
        assert_eq!(f.filter(b"\x1bOA").as_ref(), b"\x1bOA");
    }
}
