//! Filter implementations for the tfil PTY proxy.
//!
//! Each filter rewrites a stream of bytes coming from a child process
//! before it reaches the user's terminal. Filters are stateful so they
//! can handle escape sequences that span multiple chunks.

use std::borrow::Cow;

mod cursor_shape;
mod ink_fake_cursor;
mod osc_title;

pub use cursor_shape::CursorShapeFilter;
pub use ink_fake_cursor::InkFakeCursorFilter;
pub use osc_title::OscTitleFilter;

/// Stateful byte-stream filter.
///
/// Implementations buffer partial escape sequences across calls and emit
/// them once they can be classified. [`finish`](Self::finish) is called
/// at end of stream so any unfinished pending bytes can be flushed.
pub trait Filter {
    /// Filter a chunk of bytes. The returned slice may borrow from
    /// `data` when no rewriting is needed.
    fn filter<'a>(&mut self, data: &'a [u8]) -> Cow<'a, [u8]>;

    /// Flush any bytes the filter was holding back. Called once when the
    /// upstream stream has reached EOF.
    fn finish(&mut self) -> Vec<u8> {
        Vec::new()
    }
}
