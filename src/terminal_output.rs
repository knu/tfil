use std::io::{self, Write};
use vtparse::{CsiParam, VTActor, VTParser};

/// Serializes child output and synthetic terminal controls across input/output
/// threads.  Controls must wait for a complete escape sequence or UTF-8 character.
pub(crate) struct TerminalOutput<W> {
    writer: W,
    parser: Option<VTParser>,
    pending: Vec<u8>,
}

impl<W: Write> TerminalOutput<W> {
    pub(crate) fn new(writer: W, track_sequences: bool) -> Self {
        Self {
            writer,
            parser: track_sequences.then(VTParser::new),
            pending: Vec::new(),
        }
    }

    pub(crate) fn write_child(&mut self, bytes: &[u8], extra: &[u8]) -> io::Result<()> {
        if let Some(parser) = &mut self.parser {
            parser.parse(bytes, &mut IgnoreActions);
        }
        self.writer.write_all(bytes)?;
        self.write_extra(extra)
    }

    /// `bytes` must contain only complete terminal control sequences.
    pub(crate) fn write_extra(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.pending.extend_from_slice(bytes);
        if self.parser.as_ref().is_none_or(VTParser::is_ground) {
            self.writer.write_all(&self.pending)?;
            self.pending.clear();
        }
        self.writer.flush()
    }
}

struct IgnoreActions;

impl VTActor for IgnoreActions {
    fn print(&mut self, _: char) {}
    fn execute_c0_or_c1(&mut self, _: u8) {}
    fn dcs_hook(&mut self, _: u8, _: &[i64], _: &[u8], _: bool) {}
    fn dcs_put(&mut self, _: u8) {}
    fn dcs_unhook(&mut self) {}
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
