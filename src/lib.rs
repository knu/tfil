//! Terminal output filters used by the `tfil` PTY proxy.
//!
//! The library exposes a small [`filters::Filter`] trait and a handful of
//! implementations that rewrite escape sequences in a child process's
//! output stream. Filters are independent and can be composed by running
//! them in sequence.
//!
//! # Example
//!
//! ```
//! use tfil::filters::{Filter, OscTitleFilter};
//!
//! let mut f = OscTitleFilter::new();
//! let out = f.filter(b"hello\x1b]0;ignored\x07world");
//! assert_eq!(out.as_ref(), b"helloworld");
//! ```

pub mod filters;
