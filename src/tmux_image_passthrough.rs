use std::borrow::Cow;
use tfil::filters::{Filter, tmux_wrap};

const ESC: u8 = 0x1b;
// Leave room for the DCS framing within tmux's default 1 MiB input buffer.
const MAX_IMAGE_SEQUENCE: usize = 1024 * 1024 - 16;

#[derive(Default)]
pub(crate) struct ImagePassthrough {
    state: State,
    pending: Vec<u8>,
}

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Escape,
    String {
        kind: u8,
        escaped: bool,
    },
}

impl Filter for ImagePassthrough {
    fn filter<'a>(&mut self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        if matches!(self.state, State::Ground) && !bytes.contains(&ESC) {
            return Cow::Borrowed(bytes);
        }
        let mut out = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            self.push(byte, &mut out);
        }
        Cow::Owned(out)
    }

    fn finish(&mut self) -> Vec<u8> {
        if matches!(self.state, State::Escape) {
            self.pending.push(ESC);
        }
        self.state = State::Ground;
        std::mem::take(&mut self.pending)
    }
}

impl ImagePassthrough {
    fn push(&mut self, byte: u8, out: &mut Vec<u8>) {
        match self.state {
            State::Ground if byte == ESC => self.state = State::Escape,
            State::Ground => out.push(byte),
            State::Escape => {
                self.state = match byte {
                    b']' | b'_' => {
                        self.pending.extend([ESC, byte]);
                        State::String {
                            kind: byte,
                            escaped: false,
                        }
                    }
                    b'P' | b'^' | b'X' | b'k' => {
                        out.extend([ESC, byte]);
                        State::String {
                            kind: byte,
                            escaped: false,
                        }
                    }
                    ESC => {
                        out.push(ESC);
                        State::Escape
                    }
                    _ => {
                        out.extend([ESC, byte]);
                        State::Ground
                    }
                };
            }
            State::String { kind, escaped } => {
                // A tmux DCS uses doubled ESC within its payload. Keep the
                // whole DCS opaque so existing passthrough is left alone.
                let ended =
                    (escaped && byte == b'\\') || (matches!(kind, b']' | b'k') && byte == 0x07);
                let cancelled = kind != b'P' && matches!(byte, 0x18 | 0x1a);
                if escaped && !ended && !cancelled && !self.pending.is_empty() {
                    self.pending.pop();
                    out.append(&mut self.pending);
                    self.state = State::Escape;
                    self.push(byte, out);
                    return;
                }
                if self.pending.is_empty() {
                    out.push(byte);
                } else if self.pending.len() == MAX_IMAGE_SEQUENCE {
                    out.append(&mut self.pending);
                    out.push(byte);
                } else {
                    self.pending.push(byte);
                    if ended && is_image(&self.pending) {
                        out.extend(tmux_wrap(&self.pending));
                        self.pending.clear();
                    }
                }
                self.state = if ended || cancelled {
                    out.append(&mut self.pending);
                    State::Ground
                } else {
                    State::String {
                        kind,
                        escaped: byte == ESC && !escaped,
                    }
                };
            }
        }
    }
}

fn is_image(sequence: &[u8]) -> bool {
    if sequence.starts_with(b"\x1b_G") {
        return true;
    }
    let Some(body) = sequence.strip_prefix(b"\x1b]1337;") else {
        return false;
    };
    let body = body
        .strip_suffix(b"\x07")
        .or_else(|| body.strip_suffix(b"\x1b\\"))
        .expect("complete OSC");
    if body.starts_with(b"FilePart=") || body == b"FileEnd" {
        return true;
    }
    body.strip_prefix(b"File=")
        .or_else(|| body.strip_prefix(b"MultipartFile="))
        .and_then(|fields| fields.split(|&byte| byte == b':').next())
        .is_some_and(|fields| {
            fields
                .split(|&byte| byte == b';')
                .any(|field| field == b"inline=1")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_only_bare_graphics_at_every_split() {
        let kitty = b"\x1b_Ga=T;AAAA\x1b\\";
        let file = b"\x1b]1337;File=inline=1:AAAA\x07";
        let start = b"\x1b]1337;MultipartFile=inline=1\x1b\\";
        let part = b"\x1b]1337;FilePart=AAAA\x07";
        let end = b"\x1b]1337;FileEnd\x07";
        let other =
            b"text\x1b[2m\x1b]1337;File=inline=0:AAAA\x07\x1b]1337;FileEndLater\x07\x1bPq?\x1b\\";
        let already_wrapped = tmux_wrap(kitty);
        let unfinished = b"\x1b]1337;File=inline=1:AAAA";
        let input = [
            other.as_slice(),
            kitty,
            file,
            start,
            part,
            end,
            &already_wrapped,
            unfinished,
        ]
        .concat();
        let mut expected = other.to_vec();
        for sequence in [kitty.as_slice(), file, start, part, end] {
            expected.extend(tmux_wrap(sequence));
        }
        expected.extend(&already_wrapped);
        expected.extend(unfinished);
        for split in 0..=input.len() {
            let mut filter = ImagePassthrough::default();
            let mut actual = filter.filter(&input[..split]).into_owned();
            actual.extend_from_slice(&filter.filter(&input[split..]));
            actual.extend(filter.finish());
            assert_eq!(actual, expected, "split {split}");
        }
    }

    #[test]
    fn oversized_image_does_not_hold_up_the_remaining_stream() {
        let mut filter = ImagePassthrough::default();
        let mut large = b"\x1b]1337;File=inline=1:".to_vec();
        large.resize(MAX_IMAGE_SEQUENCE + 1, b'A');
        assert_eq!(filter.filter(&large).as_ref(), large);
        assert_eq!(filter.filter(b"\x07text\x1b").as_ref(), b"\x07text");
        assert_eq!(filter.finish(), b"\x1b");
    }
}
