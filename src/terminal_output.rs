use std::io::{self, Write};
use vtparse::{CsiParam, VTActor, VTParser};

// Normally only a few updates await the next sequence boundary.  Allow dozens
// of current 14-25 byte controls, then fail rather than accumulate indefinitely.
const MAX_PENDING_CONTROLS: usize = 1024;

/// Writes child output and synthetic controls in stream order.  Controls must
/// wait for a complete escape sequence or UTF-8 character.
/// Call `flush` before waiting for more commands.
pub(crate) struct TerminalOutput<W> {
    writer: W,
    parser: Option<SequenceTracker>,
    pending: Vec<u8>,
    finished: bool,
    write_failed: bool,
}

impl<W: Write> TerminalOutput<W> {
    pub(crate) fn new(writer: W, track_sequences: bool) -> Self {
        Self {
            writer,
            parser: track_sequences.then(SequenceTracker::new),
            pending: Vec::new(),
            finished: false,
            write_failed: false,
        }
    }

    pub(crate) fn write_child(&mut self, bytes: &[u8], extra: &[u8]) -> io::Result<()> {
        if self.finished {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "terminal output closed",
            ));
        }
        if let Some(parser) = &mut self.parser {
            parser.parse(bytes);
        }
        self.writer.write_all(bytes)?;
        self.write_extra(extra)
    }

    /// `bytes` must contain only complete terminal control sequences.
    pub(crate) fn write_extra(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        if self.parser.as_ref().is_none_or(SequenceTracker::is_ground) {
            self.writer.write_all(&self.pending)?;
            self.pending.clear();
            self.writer.write_all(bytes)?;
        } else {
            if bytes.len() > MAX_PENDING_CONTROLS - self.pending.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "pending terminal controls exceed 1 KiB",
                ));
            }
            let needed = self.pending.len() + bytes.len();
            if needed > self.pending.capacity() {
                // Keep geometric growth without allocating beyond the cap.
                self.pending
                    .reserve_exact(needed.next_power_of_two() - self.pending.len());
            }
            self.pending.extend_from_slice(bytes);
        }
        Ok(())
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }

    pub(crate) fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    pub(crate) fn mark_write_failed(&mut self) {
        // A partial write can stop inside a sequence even if the whole batch
        // was parsed through its final boundary.
        self.write_failed = true;
    }

    /// Ends the child stream and restores terminal modes.  Queued controls must
    /// not re-enable modes after cleanup.
    pub(crate) fn finish(&mut self, controls: &[u8]) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.pending.clear();
        if self.write_failed || self.parser.as_ref().is_some_and(|p| !p.is_ground()) {
            // CAN cancels ordinary sequences and incomplete UTF-8.  tmux DCS
            // treats CAN as payload, so also send ST; the leading CAN consumes
            // any dangling DCS escape, and the trailing CAN cancels its payload.
            self.writer.write_all(b"\x18\x1b\\\x18")?;
        }
        self.writer.write_all(controls)?;
        self.writer.flush()
    }
}

struct SequenceTracker {
    parser: VTParser,
    actions: BoundaryActions,
}

impl SequenceTracker {
    fn new() -> Self {
        Self {
            parser: VTParser::new(),
            actions: BoundaryActions::default(),
        }
    }

    fn parse(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.actions.tmux {
                TmuxState::Payload => {
                    if byte == 0x1b {
                        self.actions.tmux = TmuxState::Escape;
                    }
                }
                TmuxState::Escape => {
                    if byte == b'\\' {
                        self.parser = VTParser::new();
                        self.actions.tmux = TmuxState::None;
                    } else {
                        self.actions.tmux = TmuxState::Payload;
                    }
                }
                _ => self.parser.parse_byte(byte, &mut self.actions),
            }
        }
    }

    fn is_ground(&self) -> bool {
        self.parser.is_ground() && self.actions.tmux == TmuxState::None
    }
}

#[derive(Default, PartialEq, Eq)]
enum TmuxState {
    #[default]
    None,
    Prefix(usize),
    Payload,
    Escape,
}

#[derive(Default)]
struct BoundaryActions {
    tmux: TmuxState,
}

impl VTActor for BoundaryActions {
    fn print(&mut self, _: char) {}
    fn execute_c0_or_c1(&mut self, _: u8) {}
    fn dcs_hook(&mut self, byte: u8, params: &[i64], intermediates: &[u8], ignored: bool) {
        if byte == b't' && params.is_empty() && intermediates.is_empty() && !ignored {
            self.tmux = TmuxState::Prefix(0);
        }
    }
    fn dcs_put(&mut self, byte: u8) {
        if let TmuxState::Prefix(index) = self.tmux {
            self.tmux = if byte != b"mux;"[index] {
                TmuxState::None
            } else if index == 3 {
                // vtparse's normal DCS rules do not understand doubled ESC.
                TmuxState::Payload
            } else {
                TmuxState::Prefix(index + 1)
            };
        }
    }
    fn dcs_unhook(&mut self) {
        self.tmux = TmuxState::None;
    }
    fn esc_dispatch(&mut self, _: &[i64], _: &[u8], _: bool, _: u8) {}
    fn csi_dispatch(&mut self, _: &[CsiParam], _: bool, _: u8) {}
    fn osc_dispatch(&mut self, _: &[&[u8]]) {}
    fn apc_dispatch(&mut self, _: Vec<u8>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use tfil::codex_mouse_ui::{CodexMouseUi, MOUSE_ENABLE, POINTER_ON};
    use tfil::filters::tmux_wrap;

    #[test]
    fn pending_controls_are_capped_and_cleanup_remains_possible() {
        let mut output = TerminalOutput::new(Vec::new(), true);
        output.write_child(b"\x1b]0;unfinished", &[]).unwrap();
        let controls = b"\x1b[m".repeat(MAX_PENDING_CONTROLS / 3);
        output.write_extra(&controls).unwrap();
        let before = output.pending.len();
        let error = output.write_extra(b"\x1b[m").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(output.pending.len(), before);
        assert!(output.pending.capacity() <= MAX_PENDING_CONTROLS);
        output.finish(b"\x1b[?25h").unwrap();
        assert!(output.pending.is_empty());
        assert_eq!(output.writer, b"\x1b]0;unfinished\x18\x1b\\\x18\x1b[?25h");
    }

    #[test]
    fn complete_sequences_do_not_accumulate_controls() {
        let mut output = TerminalOutput::new(Vec::new(), true);
        let controls = b"\x1b[m".repeat(MAX_PENDING_CONTROLS);
        output.write_extra(&controls).unwrap();
        assert_eq!(output.writer, controls);
        assert_eq!(output.pending.capacity(), 0);
    }

    #[test]
    fn mouse_reassertion_does_not_leak_synchronized_update_suffix() {
        let mut mouse = CodexMouseUi::new(24, 100);
        mouse.on_output(b"\x1b[?2004h");
        let mut output = TerminalOutput::new(Vec::new(), true);
        for chunk in [b"\x1b[?1004h\x1b[?20".as_slice(), b"26l"] {
            output.write_child(chunk, &mouse.on_output(chunk)).unwrap();
        }
        let mut screen = vt100::Parser::new(24, 100, 0);
        screen.process(&output.writer);
        assert_eq!(screen.screen().contents(), "");
        assert!(output.writer.windows(8).any(|b| b == b"\x1b[?2026l"));
        assert!(output.writer.ends_with(MOUSE_ENABLE));
    }

    #[test]
    fn extra_output_waits_at_every_sequence_and_utf8_split() {
        for sequence in [
            b"\x1b[?2026l".as_slice(),
            b"\x1b]0;title\x07",
            b"\x1b]0;title\x1b\\",
            b"\x1bP1;2qpayload\x1b\\",
            b"\x1b_Gpayload\x1b\\",
            b"\x1b(B",
            "日".as_bytes(),
        ] {
            for split in 1..sequence.len() {
                for extra in [POINTER_ON.to_vec(), tmux_wrap(POINTER_ON)] {
                    let mut output = TerminalOutput::new(Vec::new(), true);
                    output.write_child(&sequence[..split], &[]).unwrap();
                    output.write_extra(&extra).unwrap();
                    output.write_child(&sequence[split..], &[]).unwrap();
                    let mut expected = sequence.to_vec();
                    expected.extend(extra);
                    assert_eq!(output.writer, expected, "{sequence:?}, split {split}");
                }
            }
        }
    }

    #[test]
    fn child_output_is_forwarded_immediately_while_controls_wait() {
        let mut output = TerminalOutput::new(Vec::new(), true);
        output.write_child(b"text\x1b[?20", MOUSE_ENABLE).unwrap();
        assert_eq!(output.writer, b"text\x1b[?20");
        output.write_extra(POINTER_ON).unwrap();
        assert_eq!(output.writer, b"text\x1b[?20");
        output.write_child(b"26l", &[]).unwrap();
        assert_eq!(
            output.writer,
            [b"text\x1b[?2026l".as_slice(), MOUSE_ENABLE, POINTER_ON].concat()
        );
        assert!(output.pending.is_empty());
    }

    #[test]
    fn controls_wait_for_the_outer_tmux_terminator() {
        for payload in [
            b"\x1b[31mred\x1b[0m".to_vec(),
            POINTER_ON.to_vec(),
            b"\x1b]0;title\x1b\\".to_vec(),
            tmux_wrap(POINTER_ON),
            b"\x18\x1a\x9c\x1b\\".to_vec(),
        ] {
            let sequence = tmux_wrap(&payload);
            for split in 1..sequence.len() {
                let mut output = TerminalOutput::new(Vec::new(), true);
                output
                    .write_child(&sequence[..split], MOUSE_ENABLE)
                    .unwrap();
                output.write_extra(POINTER_ON).unwrap();
                assert_eq!(output.writer, sequence[..split], "split {split}");
                output.write_child(&sequence[split..], &[]).unwrap();
                assert_eq!(
                    output.writer,
                    [sequence.as_slice(), MOUSE_ENABLE, POINTER_ON].concat(),
                    "split {split}"
                );
            }
            let mut output = TerminalOutput::new(Vec::new(), true);
            for &byte in &sequence {
                output.write_child(&[byte], &[]).unwrap();
                output.write_extra(POINTER_ON).unwrap();
            }
            assert_eq!(
                output.writer,
                [sequence.as_slice(), &POINTER_ON.repeat(sequence.len())].concat()
            );
        }
    }

    #[test]
    fn ordinary_sequences_do_not_enter_tmux_passthrough() {
        for sequence in [
            b"\x1b]0;tmux;title\x07".as_slice(),
            b"\x1bPtmu\x18",
            b"\x1bPtmuxx;\x18",
            b"\x1bP1tmux;\x18",
            b"\x1bPt\x18mux;",
        ] {
            let mut output = TerminalOutput::new(Vec::new(), true);
            output.write_child(sequence, POINTER_ON).unwrap();
            assert_eq!(output.writer, [sequence, POINTER_ON].concat());
        }
    }

    #[test]
    fn finish_recovers_incomplete_sequences_before_restoring_modes() {
        let cleanup = b"\x1b[?1000l\x1b[?25h";
        for sequence in [
            b"\x1b[?2026l".to_vec(),
            b"\x1b]0;title\x1b\\".to_vec(),
            b"\x1bP1;2qpayload\x1b\\".to_vec(),
            b"\x1b_Gpayload\x1b\\".to_vec(),
            "日".as_bytes().to_vec(),
            tmux_wrap(POINTER_ON),
        ] {
            for split in 1..sequence.len() {
                let mut output = TerminalOutput::new(Vec::new(), true);
                output
                    .write_child(&sequence[..split], MOUSE_ENABLE)
                    .unwrap();
                output.finish(cleanup).unwrap();
                assert_eq!(
                    output.writer,
                    [&sequence[..split], b"\x18\x1b\\\x18", cleanup].concat(),
                    "{sequence:?}, split {split}"
                );
                assert!(output.pending.is_empty());
                let mut tracker = SequenceTracker::new();
                tracker.parse(&output.writer);
                assert!(tracker.is_ground());
                output.write_extra(MOUSE_ENABLE).unwrap();
                output.finish(cleanup).unwrap();
                assert!(output.writer.ends_with(cleanup));
                assert_eq!(output.writer.len(), split + 4 + cleanup.len());
            }
        }
    }

    #[test]
    fn finish_preserves_complete_output_without_cancellation() {
        for track in [false, true] {
            let mut output = TerminalOutput::new(Vec::new(), track);
            output.write_child(b"done", &[]).unwrap();
            output.finish(b"\x1b[?25h").unwrap();
            assert_eq!(output.writer, b"done\x1b[?25h");
        }
    }

    #[test]
    fn child_output_cannot_write_after_finish() {
        let mut output = TerminalOutput::new(Vec::new(), true);
        output.finish(b"\x1b[?25h").unwrap();
        let error = output
            .write_child(b"late output", MOUSE_ENABLE)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        output.write_extra(POINTER_ON).unwrap();
        assert_eq!(output.writer, b"\x1b[?25h");
        assert!(output.pending.is_empty());
    }

    #[test]
    fn input_pointer_update_does_not_leak_synchronized_update_suffix() {
        let mut mouse = CodexMouseUi::new(24, 100);
        mouse.set_tmux_pointer(true);
        mouse.on_output("\x1b[?2004h› 1. First\r\n  2. Other\r\n".as_bytes());
        let mut output = TerminalOutput::new(Vec::new(), true);
        output.write_child(b"\x1b[?20", &[]).unwrap();
        let (_, pointer) = mouse.on_input(b"\x1b[<35;5;1M");
        assert!(!pointer.is_empty());
        output.write_extra(&pointer).unwrap();
        output.write_child(b"26l", &[]).unwrap();
        assert_eq!(
            output.writer,
            [b"\x1b[?2026l".as_slice(), &pointer].concat()
        );
        let mut screen = vt100::Parser::new(24, 100, 0);
        screen.process(&output.writer);
        assert_eq!(screen.screen().contents(), "");
    }
}
