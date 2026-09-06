use memchr::memchr;
use std::borrow::Cow;

/// Collects complete OSCs without decoding or regenerating their payloads.
#[derive(Debug, Default)]
pub(super) struct OscScanner {
    pending: Vec<u8>,
    state: State,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Normal,
    Escape,
    Osc,
    OscEscape,
}

impl OscScanner {
    pub(super) fn filter<'a>(
        &mut self,
        data: &'a [u8],
        mut complete: impl FnMut(Option<u16>, &[u8], &mut Vec<u8>),
    ) -> Cow<'a, [u8]> {
        if self.state == State::Normal && memchr(0x1b, data).is_none() {
            return Cow::Borrowed(data);
        }
        let mut out = Vec::with_capacity(data.len());
        for &byte in data {
            if self.state == State::Normal {
                if byte == 0x1b {
                    self.pending.push(byte);
                    self.state = State::Escape;
                } else {
                    out.push(byte);
                }
                continue;
            }
            self.pending.push(byte);
            match self.state {
                State::Escape => {
                    if byte == b']' {
                        self.state = State::Osc;
                    } else {
                        out.append(&mut self.pending);
                        self.state = State::Normal;
                    }
                }
                State::Osc if byte == 0x07 => self.complete(1, &mut out, &mut complete),
                State::Osc if byte == 0x1b => self.state = State::OscEscape,
                State::OscEscape if byte == b'\\' => self.complete(2, &mut out, &mut complete),
                State::OscEscape => self.state = State::Osc,
                _ => {}
            }
        }
        Cow::Owned(out)
    }

    fn complete(
        &mut self,
        terminator_len: usize,
        out: &mut Vec<u8>,
        complete: &mut impl FnMut(Option<u16>, &[u8], &mut Vec<u8>),
    ) {
        let body = &self.pending[2..self.pending.len() - terminator_len];
        let field = &body[..memchr(b';', body).unwrap_or(body.len())];
        let code = if !field.is_empty() && field.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(field).ok().and_then(|s| s.parse().ok())
        } else {
            None
        };
        complete(code, &self.pending, out);
        self.pending.clear();
        self.state = State::Normal;
    }

    pub(super) fn finish(&mut self) -> Vec<u8> {
        self.state = State::Normal;
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_independent_of_payload_and_chunk_boundaries() {
        for (input, code) in [
            ("\x1b]0;hello;世界\x07", Some(0)),
            ("\x1b]22;pointer\x1b\\", Some(22)),
            ("\x1b]0022;pointer\x07", Some(22)),
            ("\x1b]22\x07", Some(22)),
            ("\x1b]22x;pointer\x07", None),
            ("\x1b]65536;payload\x07", None),
        ] {
            let input = input.as_bytes();
            for split in 0..=input.len() {
                let mut scanner = OscScanner::default();
                let mut seen = Vec::new();
                let mut complete = |actual, bytes: &[u8], out: &mut Vec<u8>| {
                    seen.push(actual);
                    out.extend_from_slice(bytes);
                };
                let mut output = scanner.filter(&input[..split], &mut complete).into_owned();
                output.extend_from_slice(&scanner.filter(&input[split..], &mut complete));
                output.extend(scanner.finish());
                assert_eq!(output, input);
                assert_eq!(seen, [code]);
            }
        }
    }

    #[test]
    fn incomplete_sequences_are_returned_unchanged() {
        let input = "\x1b]0;hello;世界\x1b\\".as_bytes();
        for end in 1..input.len() {
            let mut scanner = OscScanner::default();
            let output = scanner.filter(&input[..end], |_, _, _| panic!("incomplete OSC"));
            assert!(output.is_empty());
            assert_eq!(scanner.finish(), input[..end]);
            assert_eq!(
                scanner.filter(b"text", |_, _, _| unreachable!()).as_ref(),
                b"text"
            );
        }
    }
}
