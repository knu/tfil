use crate::terminal_output::SequenceTracker;
use std::borrow::Cow;
use std::num::NonZeroU32;
use tfil::filters::Filter;

const MAX_REFERENCE_BYTES: usize = 2048;
const OSC8_OPEN: &[u8] = b"\x1b]8;;file://";
const ST: &[u8] = b"\x1b\\";
const OSC8_CLOSE: &[u8] = b"\x1b]8;;\x1b\\";

/// Links absolute `path:line` references in printable terminal output.
pub(crate) struct FileLinkFilter {
    tracker: SequenceTracker,
    reference: Vec<u8>,
    at_boundary: bool,
}

impl FileLinkFilter {
    pub(crate) fn new() -> Self {
        Self {
            tracker: SequenceTracker::new(),
            reference: Vec::new(),
            at_boundary: true,
        }
    }

    fn write_reference(&mut self, out: &mut Vec<u8>) {
        if let Some((path, line)) = file_location(&self.reference) {
            out.extend_from_slice(OSC8_OPEN);
            write_uri_path(path.as_bytes(), out);
            out.push(b'#');
            out.extend_from_slice(line.as_bytes());
            out.extend_from_slice(ST);
            out.extend_from_slice(&self.reference);
            out.extend_from_slice(OSC8_CLOSE);
        } else {
            out.extend_from_slice(&self.reference);
        }
        self.reference.clear();
    }
}

impl Filter for FileLinkFilter {
    fn filter<'a>(&mut self, data: &'a [u8]) -> Cow<'a, [u8]> {
        if data.is_empty() {
            return Cow::Borrowed(data);
        }
        let mut out = (!self.reference.is_empty()).then(|| Vec::with_capacity(data.len()));
        for (index, &byte) in data.iter().enumerate() {
            let ground = self.tracker.is_ground();
            let boundary = if ground {
                is_boundary(byte)
            } else {
                // A pending reference can be inside a UTF-8 character.
                byte.is_ascii()
            };
            if !self.reference.is_empty() {
                if boundary {
                    self.write_reference(out.as_mut().unwrap());
                } else if self.reference.len() == MAX_REFERENCE_BYTES {
                    out.as_mut().unwrap().append(&mut self.reference);
                } else {
                    self.reference.push(byte);
                    self.tracker.parse_byte(byte);
                    continue;
                }
            }
            if ground && self.at_boundary && byte == b'/' && !self.tracker.hyperlink_active() {
                out.get_or_insert_with(|| {
                    let mut out = Vec::with_capacity(data.len());
                    out.extend_from_slice(&data[..index]);
                    out
                });
                self.reference.push(byte);
            } else if let Some(out) = &mut out {
                out.push(byte);
            }
            if ground {
                self.at_boundary = boundary;
            }
            self.tracker.parse_byte(byte);
        }
        out.map_or(Cow::Borrowed(data), Cow::Owned)
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write_reference(&mut out);
        out
    }
}

fn is_boundary(byte: u8) -> bool {
    matches!(
        byte,
        0x00..=0x20 | 0x7f..=0x9f | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'<' | b'>'
            | b'"' | b'\'' | b'`'
    )
}

fn file_location(reference: &[u8]) -> Option<(&str, &str)> {
    let (path, line) = std::str::from_utf8(reference).ok()?.rsplit_once(':')?;
    if path.starts_with("//")
        || path.ends_with('/')
        || path.chars().any(|ch| ch.is_control() || ch.is_whitespace())
        || !line.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    line.parse::<NonZeroU32>().ok()?;
    Some((path, line))
}

fn write_uri_path(path: &[u8], out: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &byte in path {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(byte);
        } else {
            out.extend_from_slice(&[b'%', HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tfil::filters::tmux_wrap;

    fn assert_at_every_split(input: &[u8], expected: &[u8]) {
        for split in 0..=input.len() {
            let mut filter = FileLinkFilter::new();
            let mut actual = filter.filter(&input[..split]).into_owned();
            actual.extend_from_slice(&filter.filter(&input[split..]));
            actual.extend(filter.finish());
            assert_eq!(actual, expected, "split at byte {split}");
        }
    }

    #[test]
    fn links_absolute_paths_with_punctuation_and_utf8() {
        let input = "[source](/src/文%#?.rs:497) \x1b[36m/src/lib.rs:1";
        let expected = concat!(
            "[source](\x1b]8;;file:///src/%E6%96%87%25%23%3F.rs#497\x1b\\",
            "/src/文%#?.rs:497\x1b]8;;\x1b\\) \x1b[36m",
            "\x1b]8;;file:///src/lib.rs#1\x1b\\/src/lib.rs:1\x1b]8;;\x1b\\"
        );
        assert_at_every_split(input.as_bytes(), expected.as_bytes());
    }

    #[test]
    fn leaves_control_strings_and_existing_hyperlinks_intact() {
        let wrapped = tmux_wrap(b"\x1b]0;(/src/inside:7)\x07 /src/payload:1 ");
        let input = [
            wrapped.as_slice(),
            b"\x1b]2;(/src/title:3)\x07\x1b]8;;https://example.test\x1b\\",
            b"(/src/label:9)\x1b]8;;\x07",
            b" (/src/file:5)",
        ]
        .concat();
        let expected = [
            &input[..input.len() - b" (/src/file:5)".len()],
            b" (\x1b]8;;file:///src/file#5\x1b\\/src/file:5\x1b]8;;\x1b\\)",
        ]
        .concat();
        assert_at_every_split(&input, &expected);
    }

    #[test]
    fn preserves_nonlocations_and_overlong_references() {
        let input = b"(/src/file) (/src/file:0) file:///src/file:1 https://a.test/src:42 /a:\xff";
        assert_at_every_split(input, input);

        let input = format!("/{}:7 ", "x".repeat(MAX_REFERENCE_BYTES));
        assert_at_every_split(input.as_bytes(), input.as_bytes());
    }
}
