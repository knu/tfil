use super::{Control, ESC, is_pane_osc};
use memchr::memchr2;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tfil::filters::tmux_wrap;

pub(super) const MAX_DCS: usize = 8 * 1024 * 1024;
pub(super) const MAX_APC: usize = 64 * 1024;
const MAX_CLIPBOARD_OSC: usize = 2 * 1024 * 1024;
const MAX_IMAGE_DATA: usize = 8 * 1024 * 1024;
const MAX_INNER_SEQUENCES: usize = 4096;
const ST: &[u8] = b"\x1b\\";

#[derive(Clone, Copy)]
pub(crate) enum Endpoint {
    Tmux { image_base: Option<u32> },
    Herdr,
}

pub(crate) struct Auto {
    endpoint: Endpoint,
    focus: Arc<AtomicBool>,
    strip_unfocused: Vec<u16>,
    wrap_pointer: bool,
    strip_titles: bool,
    image: Option<PendingImage>,
}

#[derive(Default)]
struct PendingImage {
    commands: Vec<u8>,
    remaining: usize,
}

impl PendingImage {
    fn push(&mut self, data: &[u8]) {
        self.commands.extend_from_slice(b"\x1b_");
        self.commands.extend_from_slice(data);
        self.commands.extend_from_slice(ST);
    }
}

impl Auto {
    pub(crate) fn new(
        endpoint: Endpoint,
        focus: Arc<AtomicBool>,
        strip_unfocused: Vec<u16>,
        wrap_pointer: bool,
        strip_titles: bool,
    ) -> Self {
        Self {
            endpoint,
            focus,
            strip_unfocused,
            wrap_pointer,
            strip_titles,
            image: None,
        }
    }

    pub(super) fn is_tmux(&self) -> bool {
        matches!(self.endpoint, Endpoint::Tmux { .. })
    }

    pub(super) fn write_osc(
        &self,
        data: &[u8],
        terminator: &[u8],
        wrapped: bool,
        out: &mut Vec<u8>,
    ) -> bool {
        let Some((code, payload)) = split_at(data, b';') else {
            return false;
        };
        let Some(number) = decimal::<u16>(code) else {
            return false;
        };
        let focused = self.focus.load(Ordering::Relaxed);
        if !focused && (matches!(number, 22 | 52 | 5522) || self.strip_unfocused.contains(&number))
        {
            return false;
        }
        if data.len() > osc_limit(code) {
            return false;
        }
        if !printable(payload) {
            return false;
        }

        let outer = match code {
            b"0" | b"1" | b"2" if self.strip_titles => return false,
            code if is_pane_osc(code) => false,
            b"9" if progress(payload) => true,
            b"7501" if program_status(payload) => true,
            b"22" if pointer(payload) => wrapped || self.wrap_pointer,
            b"52" if clipboard(payload) => wrapped,
            b"5522" if !payload.is_empty() => wrapped,
            _ => return false,
        };
        self.write_control(b']', data, terminator, outer, out);
        true
    }

    pub(super) fn write_window_name(&self, name: &[u8], out: &mut Vec<u8>) -> bool {
        if !self.is_tmux() || name.len() > super::MAX_OSC || !printable(name) {
            return false;
        }
        self.write_control(b'k', name, ST, false, out);
        true
    }

    pub(super) fn write_tmux(&mut self, encoded: &[u8], out: &mut Vec<u8>) -> bool {
        let Some(mut encoded) = encoded.strip_prefix(b"tmux;") else {
            return false;
        };
        let mut decoded = Vec::with_capacity(encoded.len());
        while let Some(at) = memchr::memchr(ESC, encoded) {
            if encoded.get(at + 1) != Some(&ESC) {
                return false;
            }
            decoded.extend_from_slice(&encoded[..=at]);
            encoded = &encoded[at + 2..];
        }
        decoded.extend_from_slice(encoded);

        // Validate framing for the whole packet before emitting anything from it.
        let mut remaining = decoded.as_slice();
        let mut sequences = Vec::new();
        while !remaining.is_empty() {
            if sequences.len() == MAX_INNER_SEQUENCES {
                return false;
            }
            let Some((sequence, rest)) = Inner::read(remaining) else {
                return false;
            };
            sequences.push(sequence);
            remaining = rest;
        }
        let mut accepted = false;
        for sequence in sequences {
            accepted |= match sequence {
                Inner::Bell => {
                    out.push(0x07);
                    true
                }
                Inner::Osc(data, terminator) => self.write_osc(data, terminator, true, out),
                Inner::Captured(Control::WindowName, data) => self.write_window_name(data, out),
                Inner::Captured(Control::Image, data) => self.write_image(data, out),
            };
        }
        accepted
    }

    pub(super) fn write_image(&mut self, data: &[u8], out: &mut Vec<u8>) -> bool {
        // Codex uses this one-pixel query to detect native Kitty support in Herdr.
        if matches!(self.endpoint, Endpoint::Herdr) && data == b"Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA" {
            self.write_control(b'_', data, ST, false, out);
            return true;
        }
        let Some(transfer) = self.image_transfer(data) else {
            self.image = None;
            return false;
        };
        // Complete the whole upload before sending it to the terminal. A rejected
        // continuation must not leave a partial upload in the outer terminal.
        if let Some(remaining) = transfer {
            let pending = self.image.get_or_insert_with(PendingImage::default);
            pending.remaining = remaining;
            pending.push(data);
        } else if let Some(mut pending) = self.image.take() {
            pending.push(data);
            if self.is_tmux() {
                out.extend(tmux_wrap(&pending.commands));
            } else {
                out.extend(pending.commands);
            }
        } else {
            self.write_control(b'_', data, ST, true, out);
        }
        true
    }

    fn image_transfer(&self, data: &[u8]) -> Option<Option<usize>> {
        if data.len() > MAX_APC {
            return None;
        }
        let data = data.strip_prefix(b"G")?;
        let (header, payload) = split_at(data, b';').unwrap_or((data, b""));
        let mut params = [None; 11];
        for field in header.split(|&byte| byte == b',') {
            let ([key], value) = split_at(field, b'=')? else {
                return None;
            };
            let index = b"aitfcrqmCUd".iter().position(|k| k == key)?;
            if params[index].replace(value).is_some() {
                return None;
            }
        }
        let [
            action,
            id,
            transport,
            format,
            cols,
            rows,
            quiet,
            more,
            cursor,
            virtual_image,
            delete,
        ] = params;
        if payload.is_empty() {
            if self.image.is_some() || quiet != Some(b"2") || !self.image_id_allowed(id) {
                return None;
            }
            return match action? {
                b"d" if matches!(delete?, b"i" | b"I")
                    && [transport, format, cols, rows, more, cursor, virtual_image]
                        .iter()
                        .all(Option::is_none) =>
                {
                    Some(None)
                }
                b"p" if matches!(self.endpoint, Endpoint::Herdr)
                    && bounded_dimension(cols)
                    && bounded_dimension(rows)
                    && cursor == Some(b"1")
                    && [transport, format, more, virtual_image, delete]
                        .iter()
                        .all(Option::is_none) =>
                {
                    Some(None)
                }
                _ => None,
            };
        }
        if !base64_text(payload) {
            return None;
        }
        let more = match more {
            Some(b"1") => true,
            Some(b"0") | None => false,
            _ => return None,
        };
        let remaining = if [
            action,
            id,
            transport,
            format,
            cols,
            rows,
            quiet,
            cursor,
            virtual_image,
            delete,
        ]
        .iter()
        .all(Option::is_none)
        {
            self.image.as_ref()?.remaining
        } else {
            if self.image.is_some()
                || action != Some(b"T")
                || transport != Some(b"d")
                || format != Some(b"100")
                || quiet != Some(b"2")
                || cursor != Some(b"1")
                || delete.is_some()
                || !bounded_dimension(cols)
                || !bounded_dimension(rows)
                || !self.image_id_allowed(id)
                || !matches!(virtual_image, Some(b"1") | None)
                || (self.is_tmux() && virtual_image != Some(b"1"))
            {
                return None;
            }
            MAX_IMAGE_DATA
        };
        let remaining = remaining.checked_sub(data.len() + 5)?;
        Some(more.then_some(remaining))
    }

    fn image_id_allowed(&self, id: Option<&[u8]>) -> bool {
        let Some(id) = id.and_then(decimal::<u32>).filter(|&id| id != 0) else {
            return false;
        };
        match self.endpoint {
            Endpoint::Tmux {
                image_base: Some(base),
            } => (base..base + 256).contains(&id),
            Endpoint::Tmux { image_base: None } => false,
            Endpoint::Herdr => true,
        }
    }

    fn write_control(
        &self,
        kind: u8,
        data: &[u8],
        terminator: &[u8],
        outer: bool,
        out: &mut Vec<u8>,
    ) {
        if outer && self.is_tmux() {
            let mut control = Vec::with_capacity(2 + data.len() + terminator.len());
            control.extend_from_slice(&[ESC, kind]);
            control.extend_from_slice(data);
            control.extend_from_slice(terminator);
            out.extend(tmux_wrap(&control));
        } else {
            out.extend_from_slice(&[ESC, kind]);
            out.extend_from_slice(data);
            out.extend_from_slice(terminator);
        }
    }
}

pub(super) fn osc_limit(code: &[u8]) -> usize {
    if matches!(code, b"52" | b"5522") {
        MAX_CLIPBOARD_OSC
    } else {
        super::MAX_OSC
    }
}

enum Inner<'a> {
    Bell,
    Osc(&'a [u8], &'a [u8]),
    Captured(Control, &'a [u8]),
}

impl<'a> Inner<'a> {
    fn read(input: &'a [u8]) -> Option<(Self, &'a [u8])> {
        if let Some(rest) = input.strip_prefix(b"\x07") {
            return Some((Self::Bell, rest));
        }
        let [ESC, kind @ (b']' | b'_' | b'k'), payload @ ..] = input else {
            return None;
        };
        let at = memchr2(ESC, 0x07, payload)?;
        let terminator = if payload[at] == ESC && payload[at..].starts_with(ST) {
            ST
        } else if payload[at] == 0x07 && *kind != b'_' {
            b"\x07"
        } else {
            return None;
        };
        let rest = &payload[at + terminator.len()..];
        let body = &payload[..at];
        let sequence = match kind {
            b']' => Self::Osc(body, terminator),
            b'_' => Self::Captured(Control::Image, body),
            b'k' => Self::Captured(Control::WindowName, body),
            _ => unreachable!("matched string introducers"),
        };
        Some((sequence, rest))
    }
}

fn split_at(input: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let at = memchr::memchr(delimiter, input)?;
    Some((&input[..at], &input[at + 1..]))
}

fn decimal<T: std::str::FromStr>(input: &[u8]) -> Option<T> {
    if !input.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(input).ok()?.parse().ok()
}

fn bounded_dimension(value: Option<&[u8]>) -> bool {
    value
        .and_then(decimal::<u16>)
        .is_some_and(|v| (1..=4096).contains(&v))
}

fn printable(input: &[u8]) -> bool {
    std::str::from_utf8(input).is_ok_and(|s| !s.chars().any(char::is_control))
}

fn pointer(payload: &[u8]) -> bool {
    matches!(
        payload,
        b"" | b"default" | b"pointer" | b"text" | b"ew-resize"
    )
}

fn clipboard(payload: &[u8]) -> bool {
    split_at(payload, b';').is_some_and(|(selection, data)| {
        selection
            .iter()
            .all(|&byte| matches!(byte, b'c' | b'p' | b'q' | b's' | b'0'..=b'7'))
            && (data == b"?" || base64_text(data))
    })
}

fn base64_text(data: &[u8]) -> bool {
    let unpadded = data
        .strip_suffix(b"==")
        .or_else(|| data.strip_suffix(b"="))
        .unwrap_or(data);
    unpadded
        .iter()
        .all(|&byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
}

fn progress(payload: &[u8]) -> bool {
    let Some(state) = payload.strip_prefix(b"4;") else {
        return false;
    };
    let (state, value) = split_at(state, b';').unwrap_or((state, b""));
    matches!(state, b"0" | b"1" | b"2" | b"3" | b"4")
        && (value.is_empty() || decimal::<u8>(value).is_some_and(|v| v <= 100))
}

fn program_status(payload: &[u8]) -> bool {
    if payload == b"?" {
        return true;
    }
    payload
        .split(|&byte| byte == b':')
        .try_fold(false, |seen, field| {
            let (key, value) = split_at(field, b'=')?;
            match key {
                b"state"
                    if !seen
                        && matches!(
                            value,
                            b"idle" | b"working" | b"blocked" | b"done" | b"error" | b"clear"
                        ) =>
                {
                    Some(true)
                }
                b"app" | b"id" | b"parent" | b"kind"
                    if !value.is_empty()
                        && value.iter().all(|&byte| {
                            byte.is_ascii_alphanumeric()
                                || matches!(byte, b'-' | b'_' | b'.' | b'/')
                        }) =>
                {
                    Some(seen)
                }
                b"title" | b"msg" if base64_text(value) => Some(seen),
                _ => None,
            }
        })
        == Some(true)
}
