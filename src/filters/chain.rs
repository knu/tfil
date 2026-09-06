use super::Filter;
use std::borrow::Cow;

/// Runs filters in order, including their buffered output at EOF.
#[derive(Default)]
pub struct FilterChain {
    filters: Vec<Box<dyn Filter + Send>>,
}

impl FilterChain {
    /// Creates a chain in the given processing order.
    pub fn new(filters: Vec<Box<dyn Filter + Send>>) -> Self {
        Self { filters }
    }
}

impl Filter for FilterChain {
    fn filter<'a>(&mut self, data: &'a [u8]) -> Cow<'a, [u8]> {
        self.filters
            .iter_mut()
            .fold(Cow::Borrowed(data), |data, filter| {
                apply_filter(filter.as_mut(), data)
            })
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut tail = Vec::new();
        for filter in &mut self.filters {
            // Upstream tails follow any earlier bytes held by this filter.
            tail = apply_filter(filter.as_mut(), Cow::Owned(tail)).into_owned();
            tail.extend(filter.finish());
        }
        tail
    }
}

fn apply_filter<'a>(filter: &mut dyn Filter, data: Cow<'a, [u8]>) -> Cow<'a, [u8]> {
    match data {
        Cow::Borrowed(bytes) => filter.filter(bytes),
        Cow::Owned(bytes) => match filter.filter(&bytes) {
            Cow::Owned(next) => Cow::Owned(next),
            Cow::Borrowed(next) if std::ptr::eq(next, bytes.as_slice()) => Cow::Owned(bytes),
            // A filter may borrow a subslice or static replacement instead.
            Cow::Borrowed(next) => Cow::Owned(next.to_vec()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::{
        CursorShapeFilter, InkFakeCursorFilter, OscTitleFilter, TmuxOscPassthroughFilter, tmux_wrap,
    };

    fn all_filters() -> FilterChain {
        FilterChain::new(vec![
            Box::new(InkFakeCursorFilter::new()),
            Box::new(OscTitleFilter::new()),
            Box::new(CursorShapeFilter::new()),
            Box::new(TmuxOscPassthroughFilter::new(vec![22])),
        ])
    }

    #[test]
    fn unchanged_input_remains_borrowed() {
        let input = b"plain text";
        for mut chain in [FilterChain::default(), all_filters()] {
            let result = chain.filter(input);
            assert!(matches!(result, Cow::Borrowed(_)));
            assert!(std::ptr::eq(result.as_ref(), input.as_slice()));
        }
    }

    struct BorrowSuffix;
    impl Filter for BorrowSuffix {
        fn filter<'a>(&mut self, data: &'a [u8]) -> Cow<'a, [u8]> {
            Cow::Borrowed(&data[1..])
        }
    }

    #[test]
    fn borrowed_subslices_of_owned_output_are_preserved() {
        let mut chain = FilterChain::new(vec![
            Box::new(CursorShapeFilter::new()),
            Box::new(BorrowSuffix),
        ]);
        assert_eq!(chain.filter(b"a\x1b[5 qbc").as_ref(), b"bc");
    }

    struct BorrowStatic;
    impl Filter for BorrowStatic {
        fn filter<'a>(&mut self, _: &'a [u8]) -> Cow<'a, [u8]> {
            Cow::Borrowed(b"replacement")
        }
    }

    #[test]
    fn borrowed_static_replacements_of_owned_output_are_preserved() {
        let mut chain = FilterChain::new(vec![
            Box::new(CursorShapeFilter::new()),
            Box::new(BorrowStatic),
        ]);
        assert_eq!(chain.filter(b"a\x1b[5 qbc").as_ref(), b"replacement");
    }

    #[test]
    fn filter_chain_preserves_order_and_transforms_at_every_split() {
        let input = b"\x1b[1;7m \x1b[27m\x1b[5 q\x1b]0;title\x07\x1b]22;pointer\x07";
        let expected = [b"\x1b[1m ".as_slice(), &tmux_wrap(b"\x1b]22;pointer\x07")].concat();
        for split in 0..=input.len() {
            let mut filters = all_filters();
            let mut output = filters.filter(&input[..split]).into_owned();
            output.extend_from_slice(&filters.filter(&input[split..]));
            output.extend(filters.finish());
            assert_eq!(output, expected, "split {split}");
        }
    }

    #[test]
    fn filter_chain_flushes_upstream_tails_through_downstream_filters() {
        for input in [
            b"\x1b]22;pointer\x1b".as_slice(),
            b"\x1b[7m ",
            b"\x1b[7m \x1b]22;pointer\x07",
        ] {
            let mut filters = all_filters();
            let mut output = filters.filter(input).into_owned();
            output.extend(filters.finish());
            let expected = if input.ends_with(b"\x07") {
                [b"\x1b[7m ".as_slice(), &tmux_wrap(b"\x1b]22;pointer\x07")].concat()
            } else {
                input.to_vec()
            };
            assert_eq!(output, expected, "{input:?}");
        }
    }
}
