use super::{Filter, osc::OscScanner};
use std::borrow::Cow;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Drops selected child OSCs when the session has no confirmed focus.
#[derive(Debug)]
pub struct OscFocusFilter {
    codes: Vec<u16>,
    focused: Arc<AtomicBool>,
    scanner: OscScanner,
}

impl OscFocusFilter {
    /// Uses the shared terminal focus state to gate selected OSC codes.
    pub fn new(codes: Vec<u16>, focused: Arc<AtomicBool>) -> Self {
        Self {
            codes,
            focused,
            scanner: OscScanner::default(),
        }
    }
}

impl Filter for OscFocusFilter {
    fn filter<'a>(&mut self, data: &'a [u8]) -> Cow<'a, [u8]> {
        self.scanner.filter(data, |code, sequence, out| {
            if !code.is_some_and(|code| self.codes.contains(&code))
                || self.focused.load(Ordering::Relaxed)
            {
                out.extend_from_slice(sequence);
            }
        })
    }

    fn finish(&mut self) -> Vec<u8> {
        let tail = self.scanner.finish();
        if tail.starts_with(b"\x1b]") {
            // A partial OSC cannot be classified. Do not leave it open in the
            // surrounding terminal when the child exits.
            Vec::new()
        } else {
            tail
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::{FilterChain, TmuxOscPassthroughFilter, tmux_wrap};

    #[test]
    fn selected_osc_is_checked_when_complete_and_before_tmux_wrapping() {
        let focused = Arc::new(AtomicBool::new(false));
        let mut filters = FilterChain::new(vec![
            Box::new(OscFocusFilter::new(vec![22], Arc::clone(&focused))),
            Box::new(TmuxOscPassthroughFilter::new(vec![22])),
        ]);
        let prefix = b"\x1b]22;pointer";
        let unrelated = b"\x1b]2;title\x07";

        assert!(filters.filter(prefix).is_empty());
        focused.store(true, Ordering::Relaxed);
        assert_eq!(
            filters.filter(b"\x1b\\").as_ref(),
            tmux_wrap(&[prefix.as_slice(), b"\x1b\\"].concat()),
        );
        assert!(filters.filter(prefix).is_empty());
        focused.store(false, Ordering::Relaxed);
        assert_eq!(
            filters
                .filter(&[b"\x07", unrelated.as_slice()].concat())
                .as_ref(),
            unrelated,
        );
        assert!(filters.filter(prefix).is_empty());
        assert!(filters.finish().is_empty());
    }
}
