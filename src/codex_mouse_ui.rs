//! Mouse support for Codex CLI's marker-based numbered menus.
//!
//! [`CodexMouseUi`] sits on both sides of the PTY proxy.  On the output
//! side it maintains a [`vt100`] screen model of what the terminal
//! displays and keeps SGR any-motion mouse reporting enabled.  On the
//! input side it intercepts SGR mouse reports from the terminal:
//! hovering over a `›`/`❯`-marked numbered option steers the child's
//! own selection there with arrow keys (so the marker follows the
//! mouse) and switches the terminal's mouse pointer shape via OSC 22;
//! a click sends Enter to confirm the selection.
//!
//! Events the menu logic does not consume are forwarded to the child
//! only when the child has requested a mouse protocol of its own, in
//! the encoding it asked for.

use crate::filters::tmux_wrap;
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// Enables SGR any-motion mouse reporting on the outer terminal.
pub const MOUSE_ENABLE: &[u8] = b"\x1b[?1003h\x1b[?1006h";
/// Disables the reporting enabled by [`MOUSE_ENABLE`].
pub const MOUSE_DISABLE: &[u8] = b"\x1b[?1006l\x1b[?1003l";
/// OSC 22: switch the mouse pointer to a hand/pointer shape.
pub const POINTER_ON: &[u8] = b"\x1b]22;pointer\x1b\\";
/// OSC 22: restore the default mouse pointer shape.
pub const POINTER_OFF: &[u8] = b"\x1b]22;default\x1b\\";

const ARROW_UP: &[u8] = b"\x1b[A";
const ARROW_DOWN: &[u8] = b"\x1b[B";
const APP_ARROW_UP: &[u8] = b"\x1bOA";
const APP_ARROW_DOWN: &[u8] = b"\x1bOB";

/// Any DECSET/DECRST touching modes 10xx (mouse protocols, alternate
/// screen, ...) starts with this prefix; seeing one in the child's
/// output triggers a re-assertion of our own mouse reporting.
const MODE_PREFIX: &[u8] = b"\x1b[?10";

const MAX_CONTINUATION_ROWS: usize = 3;
const MAX_PENDING: usize = 40;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum InState {
    #[default]
    Normal,
    Esc,
    Csi,
    Mouse,
}

/// Bidirectional mouse handling state for one PTY session.
pub struct CodexMouseUi {
    parser: vt100::Parser,
    state: InState,
    pending: Vec<u8>,
    last_pos: Option<(u16, u16)>,
    pointer: bool,
    swallow_release: bool,
    /// Option row hover-steering has already sent arrows toward, until
    /// the child's redraw confirms it.
    steered_row: Option<usize>,
    /// Wrap pointer-shape OSCs in a tmux DCS passthrough.
    tmux_pointer: bool,
    prev_modes: (MouseProtocolMode, MouseProtocolEncoding, bool),
    scan_carry: Vec<u8>,
}

impl CodexMouseUi {
    /// Creates a handler for a terminal of the given size.
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            state: InState::default(),
            pending: Vec::new(),
            last_pos: None,
            pointer: false,
            swallow_release: false,
            steered_row: None,
            tmux_pointer: false,
            prev_modes: Default::default(),
            scan_carry: Vec::new(),
        }
    }

    /// Resizes the screen model (call on SIGWINCH).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Emits pointer-shape OSCs wrapped in a tmux DCS passthrough so
    /// they reach the outer terminal through tmux.
    pub fn set_tmux_pointer(&mut self, on: bool) {
        self.tmux_pointer = on;
    }

    /// Feeds a chunk of child output (as written to the terminal) into
    /// the screen model.  Returns bytes to append to the terminal
    /// output: mouse-mode re-assertions and pointer shape updates.
    pub fn on_output(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.parser.process(chunk);
        let mut out = Vec::new();
        let screen = self.parser.screen();
        let modes = (
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
            screen.alternate_screen(),
        );
        if self.scan_mode_prefix(chunk) || modes != self.prev_modes {
            out.extend_from_slice(MOUSE_ENABLE);
        }
        self.prev_modes = modes;
        // The content under a stationary mouse may have changed.
        self.update_pointer(&mut out);
        out
    }

    /// Feeds a chunk of terminal input.  Returns `(to_child, to_term)`:
    /// bytes to forward to the child and bytes (pointer shape updates)
    /// to write back to the terminal.
    pub fn on_input(&mut self, chunk: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut to_child = Vec::new();
        let mut to_term = Vec::new();
        for &byte in chunk {
            self.step(byte, &mut to_child, &mut to_term);
        }
        (to_child, to_term)
    }

    /// Flushes any partially buffered input sequence (call on stdin EOF).
    pub fn finish_input(&mut self) -> Vec<u8> {
        self.state = InState::Normal;
        std::mem::take(&mut self.pending)
    }

    fn step(&mut self, byte: u8, to_child: &mut Vec<u8>, to_term: &mut Vec<u8>) {
        match self.state {
            InState::Normal => {
                if byte == 0x1B {
                    self.pending.push(byte);
                    self.state = InState::Esc;
                } else {
                    to_child.push(byte);
                }
            }
            InState::Esc => {
                if byte == b'[' {
                    self.pending.push(byte);
                    self.state = InState::Csi;
                } else {
                    self.abort(byte, to_child);
                }
            }
            InState::Csi => {
                if byte == b'<' {
                    self.pending.push(byte);
                    self.state = InState::Mouse;
                } else {
                    self.abort(byte, to_child);
                }
            }
            InState::Mouse => {
                if byte.is_ascii_digit() || byte == b';' {
                    self.pending.push(byte);
                    if self.pending.len() > MAX_PENDING {
                        self.abort_flush(to_child);
                    }
                } else if byte == b'M' || byte == b'm' {
                    self.pending.push(byte);
                    let seq = std::mem::take(&mut self.pending);
                    self.state = InState::Normal;
                    match parse_sgr_mouse(&seq) {
                        Some(ev) => self.handle_event(&ev, &seq, to_child, to_term),
                        None => to_child.extend_from_slice(&seq),
                    }
                } else {
                    self.abort(byte, to_child);
                }
            }
        }
    }

    /// Flushes the pending buffer and reprocesses `byte` from scratch.
    fn abort(&mut self, byte: u8, to_child: &mut Vec<u8>) {
        self.abort_flush(to_child);
        // Not a recursive escape chase: state is Normal here, so this
        // either buffers a fresh ESC or emits the byte.
        self.step(byte, to_child, &mut Vec::new());
    }

    fn abort_flush(&mut self, to_child: &mut Vec<u8>) {
        to_child.append(&mut self.pending);
        self.state = InState::Normal;
    }

    fn handle_event(
        &mut self,
        ev: &MouseEvent,
        raw: &[u8],
        to_child: &mut Vec<u8>,
        to_term: &mut Vec<u8>,
    ) {
        self.last_pos = Some((ev.row, ev.col));
        let lookup = self.menu_lookup(ev.row);
        self.set_pointer(lookup.is_some(), to_term);

        let is_wheel = ev.code & 64 != 0;
        let is_motion = ev.code & 32 != 0 && !is_wheel;
        let button = ev.code & 3;

        // Hover: steer the child's own selection to the hovered option.
        if is_motion
            && button == 3
            && !ev.release
            && let Some(l) = &lookup
        {
            let target = l.options[l.clicked_idx];
            if l.marked_idx == l.clicked_idx {
                // The child's marker has caught up with the mouse.
                self.steered_row = None;
            } else if self.steered_row != Some(target) {
                let moves = l.clicked_idx as i32 - self.effective_marked(l) as i32;
                self.push_arrows(moves, to_child);
                self.steered_row = Some(target);
            }
            return;
        }
        // Plain left-button press on an option: confirm it.  Hover has
        // normally aligned the selection already; send any remaining
        // moves in case it has not.
        if !ev.release
            && !is_wheel
            && !is_motion
            && ev.code == 0
            && let Some(l) = &lookup
        {
            let moves = l.clicked_idx as i32 - self.effective_marked(l) as i32;
            self.push_arrows(moves, to_child);
            to_child.push(b'\r');
            self.steered_row = None;
            self.swallow_release = true;
            return;
        }
        // Swallow the release paired with a click we consumed.
        if ev.release && !is_wheel && !is_motion && button == 0 && self.swallow_release {
            self.swallow_release = false;
            return;
        }

        let screen = self.parser.screen();
        let allowed = match screen.mouse_protocol_mode() {
            MouseProtocolMode::None => false,
            MouseProtocolMode::Press => !ev.release && !is_motion,
            MouseProtocolMode::PressRelease => !is_motion,
            MouseProtocolMode::ButtonMotion => !is_motion || button != 3,
            MouseProtocolMode::AnyMotion => true,
        };
        if !allowed {
            return;
        }
        match screen.mouse_protocol_encoding() {
            MouseProtocolEncoding::Sgr => to_child.extend_from_slice(raw),
            MouseProtocolEncoding::Default | MouseProtocolEncoding::Utf8 => {
                if let Some(seq) = encode_x10(ev) {
                    to_child.extend_from_slice(&seq);
                }
            }
        }
    }

    fn update_pointer(&mut self, out: &mut Vec<u8>) {
        let Some((row, _)) = self.last_pos else {
            return;
        };
        let clickable = self.menu_lookup(row).is_some();
        self.set_pointer(clickable, out);
    }

    fn set_pointer(&mut self, clickable: bool, out: &mut Vec<u8>) {
        if clickable != self.pointer {
            self.pointer = clickable;
            let seq = if clickable { POINTER_ON } else { POINTER_OFF };
            if self.tmux_pointer {
                out.extend_from_slice(&tmux_wrap(seq));
            } else {
                out.extend_from_slice(seq);
            }
        }
    }

    /// The option index the child's marker is (or is about to be) on:
    /// arrows already sent for a hover count as applied even before the
    /// child's redraw reaches the screen model.
    fn effective_marked(&self, lookup: &MenuLookup) -> usize {
        self.steered_row
            .and_then(|row| lookup.options.iter().position(|&r| r == row))
            .unwrap_or(lookup.marked_idx)
    }

    fn push_arrows(&self, moves: i32, to_child: &mut Vec<u8>) {
        let app = self.parser.screen().application_cursor();
        let (up, down) = if app {
            (APP_ARROW_UP, APP_ARROW_DOWN)
        } else {
            (ARROW_UP, ARROW_DOWN)
        };
        let arrow = if moves < 0 { up } else { down };
        for _ in 0..moves.unsigned_abs() {
            to_child.extend_from_slice(arrow);
        }
    }

    fn menu_lookup(&self, row: u16) -> Option<MenuLookup> {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        let rows: Vec<String> = screen.rows(0, cols).collect();
        menu_lookup_in(&rows, usize::from(row))
    }

    /// Reports whether `chunk` (or its boundary with the previous
    /// chunk) contains a `CSI ? 10..` mode change.
    fn scan_mode_prefix(&mut self, chunk: &[u8]) -> bool {
        let mut boundary = std::mem::take(&mut self.scan_carry);
        boundary.extend_from_slice(&chunk[..chunk.len().min(MODE_PREFIX.len() - 1)]);
        let found = memchr::memmem::find(&boundary, MODE_PREFIX).is_some()
            || memchr::memmem::find(chunk, MODE_PREFIX).is_some();
        boundary.extend_from_slice(&chunk[chunk.len().min(MODE_PREFIX.len() - 1)..]);
        let tail = boundary.len().saturating_sub(MODE_PREFIX.len() - 1);
        self.scan_carry = boundary.split_off(tail);
        found
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MouseEvent {
    code: u16,
    /// 0-based column.
    col: u16,
    /// 0-based row.
    row: u16,
    release: bool,
}

/// Parses a complete SGR mouse report (`CSI < Pb ; Px ; Py M|m`).
fn parse_sgr_mouse(seq: &[u8]) -> Option<MouseEvent> {
    let body = seq.strip_prefix(b"\x1b[<")?;
    let (&last, params) = body.split_last()?;
    let release = match last {
        b'M' => false,
        b'm' => true,
        _ => return None,
    };
    let mut fields = params.split(|&b| b == b';').map(|f| {
        std::str::from_utf8(f)
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
    });
    let code = fields.next()??;
    let col = fields.next()??.checked_sub(1)?;
    let row = fields.next()??.checked_sub(1)?;
    if fields.next().is_some() {
        return None;
    }
    Some(MouseEvent {
        code,
        col,
        row,
        release,
    })
}

/// Re-encodes an event in the legacy X10 byte encoding for children
/// that enabled a mouse protocol without SGR encoding.  Returns `None`
/// when the coordinates do not fit.
fn encode_x10(ev: &MouseEvent) -> Option<Vec<u8>> {
    let code = if ev.release {
        (ev.code & !3) | 3
    } else {
        ev.code
    };
    let cb = u8::try_from(32 + code).ok()?;
    let cx = u8::try_from(33 + ev.col).ok()?;
    let cy = u8::try_from(33 + ev.row).ok()?;
    Some(vec![0x1B, b'[', b'M', cb, cx, cy])
}

fn is_marker(c: char) -> bool {
    matches!(c, '\u{203A}' | '\u{276F}') // › ❯
}

/// Parses a menu option line (`› 1. Yes ...` or `  2. No ...`), with
/// or without indentation, returning whether it carries the selection
/// marker.  An unmarked option must be indented so that flush-left
/// numbered lists in ordinary text do not qualify.
fn parse_option_line(line: &str) -> Option<bool> {
    let unindented = line.trim_start_matches(' ');
    let (marked, rest) = match unindented.chars().next() {
        Some(c) if is_marker(c) => (true, &unindented[c.len_utf8()..]),
        _ if unindented.len() < line.len() => (false, unindented),
        _ => return None,
    };
    let rest = rest.trim_start_matches(' ');
    let after_digits = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    if after_digits.len() == rest.len() {
        return None;
    }
    let mut chars = after_digits.chars();
    if chars.next() != Some('.') {
        return None;
    }
    match chars.next() {
        None | Some(' ') => Some(marked),
        _ => None,
    }
}

/// A wrapped tail of an option line: indented text that is neither an
/// option line nor blank.
fn is_continuation(line: &str) -> bool {
    parse_option_line(line).is_none()
        && !line.trim().is_empty()
        && line.chars().take_while(|&c| c == ' ').count() >= 3
}

/// An active menu located around a clicked or hovered row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MenuLookup {
    /// Screen rows of the block's option lines, in order.
    options: Vec<usize>,
    /// Index (into `options`) of the `›`-marked option.
    marked_idx: usize,
    /// Index (into `options`) of the clicked/hovered option.
    clicked_idx: usize,
}

/// Locates the menu around the clicked row.  `None` when the click is
/// not on an active menu: the block must contain at least two numbered
/// options with exactly one selection marker.
fn menu_lookup_in(rows: &[String], clicked: usize) -> Option<MenuLookup> {
    if clicked >= rows.len() {
        return None;
    }
    // Resolve a click on a wrapped continuation to its option line.
    let mut target = clicked;
    let mut steps = 0;
    while parse_option_line(&rows[target]).is_none() {
        if !is_continuation(&rows[target]) || target == 0 || steps == MAX_CONTINUATION_ROWS {
            return None;
        }
        target -= 1;
        steps += 1;
    }
    // Find the first option line of the contiguous block.
    let mut first = target;
    let mut cursor = target;
    let mut run = 0;
    while cursor > 0 {
        let prev = cursor - 1;
        if parse_option_line(&rows[prev]).is_some() {
            first = prev;
            cursor = prev;
            run = 0;
        } else if is_continuation(&rows[prev]) && run < MAX_CONTINUATION_ROWS {
            cursor = prev;
            run += 1;
        } else {
            break;
        }
    }
    // Collect the block's option lines downward.
    let mut options: Vec<(usize, bool)> = Vec::new();
    let mut row = first;
    let mut run = 0;
    while row < rows.len() {
        if let Some(marked) = parse_option_line(&rows[row]) {
            options.push((row, marked));
            run = 0;
        } else if is_continuation(&rows[row]) && run < MAX_CONTINUATION_ROWS {
            run += 1;
        } else {
            break;
        }
        row += 1;
    }
    if options.len() < 2 {
        return None;
    }
    let mut marked_iter = options.iter().enumerate().filter(|(_, (_, m))| *m);
    let (marked_idx, _) = marked_iter.next()?;
    if marked_iter.next().is_some() {
        return None;
    }
    let clicked_idx = options.iter().position(|(r, _)| *r == target)?;
    Some(MenuLookup {
        options: options.into_iter().map(|(r, _)| r).collect(),
        marked_idx,
        clicked_idx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEX_MENU: &str = concat!(
        "  Would you like to run the following command?\r\n",
        "\r\n",
        "  $ pnpm add --dir .tmp/converter node-html-markdown\r\n",
        "\r\n",
        "\u{203A} 1. Yes, proceed (y)\r\n",
        "  2. Yes, and don't ask again for commands that start with `pnpm add --dir .tmp/converter node-\r\n",
        "     html-markdown` (p)\r\n",
        "  3. No, and tell Codex what to do differently (esc)\r\n",
        "\r\n",
        "  Press enter to confirm or esc to cancel\r\n",
        "\r\n",
        "\u{203A} Summarize recent commits\r\n",
    );

    fn menu() -> CodexMouseUi {
        let mut m = CodexMouseUi::new(24, 100);
        m.on_output(CODEX_MENU.as_bytes());
        m
    }

    fn sgr(code: u16, col: u16, row: u16, release: bool) -> Vec<u8> {
        format!(
            "\x1b[<{};{};{}{}",
            code,
            col + 1,
            row + 1,
            if release { 'm' } else { 'M' }
        )
        .into_bytes()
    }

    fn menu_moves_in(rows: &[String], clicked: usize) -> Option<i32> {
        menu_lookup_in(rows, clicked).map(|l| l.clicked_idx as i32 - l.marked_idx as i32)
    }

    #[test]
    fn parses_option_lines() {
        assert_eq!(
            parse_option_line("\u{203A} 1. Yes, proceed (y)"),
            Some(true)
        );
        assert_eq!(parse_option_line("\u{276F} 2. No"), Some(true));
        assert_eq!(
            parse_option_line("  3. No, and tell Codex (esc)"),
            Some(false)
        );
        assert_eq!(parse_option_line("  12."), Some(false));
        assert_eq!(
            parse_option_line("  \u{203A} 1. Option A (Recommended)  Select the first menu item."),
            Some(true)
        );
        assert_eq!(
            parse_option_line("    2. Option B                Select the second menu item."),
            Some(false)
        );
        assert_eq!(parse_option_line("\u{203A} Summarize recent commits"), None);
        assert_eq!(parse_option_line("  Question 1/1 (1 unanswered)"), None);
        assert_eq!(parse_option_line("  1.5 released"), None);
        assert_eq!(parse_option_line("• 1. bullet"), None);
        assert_eq!(parse_option_line("1. top-level list"), None);
        assert_eq!(parse_option_line(""), None);
        assert_eq!(parse_option_line("   "), None);
    }

    #[test]
    fn resolves_indented_menu() {
        let rows: Vec<String> = [
            "",
            "  Question 1/1 (1 unanswered)",
            "  Click an option to test mouse selection.",
            "",
            "  \u{203A} 1. Option A (Recommended)  Select the first menu item.",
            "    2. Option B                Select the second menu item.",
            "    3. Option C                Select the third menu item.",
            "    4. None of the above       Optionally, add details in notes (tab).",
            "",
            "  tab to add notes | enter to submit answer | esc to interrupt",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(menu_moves_in(&rows, 4), Some(0));
        assert_eq!(menu_moves_in(&rows, 6), Some(2));
        assert_eq!(menu_moves_in(&rows, 7), Some(3));
        assert_eq!(menu_moves_in(&rows, 2), None);
        assert_eq!(menu_moves_in(&rows, 9), None);
    }

    #[test]
    fn resolves_menu_moves() {
        let rows: Vec<String> = CODEX_MENU.split("\r\n").map(|s| s.to_string()).collect();
        assert_eq!(menu_moves_in(&rows, 4), Some(0)); // marked option 1
        assert_eq!(menu_moves_in(&rows, 5), Some(1)); // option 2
        assert_eq!(menu_moves_in(&rows, 6), Some(1)); // wrapped tail of option 2
        assert_eq!(menu_moves_in(&rows, 7), Some(2)); // option 3
        assert_eq!(menu_moves_in(&rows, 9), None); // "Press enter to confirm"
        assert_eq!(menu_moves_in(&rows, 11), None); // composer prompt
        assert_eq!(menu_moves_in(&rows, 0), None); // question text
        assert_eq!(menu_moves_in(&rows, 99), None); // out of range
    }

    #[test]
    fn numbered_list_without_marker_is_not_a_menu() {
        let rows: Vec<String> = ["  1. first", "  2. second", "  3. third"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(menu_moves_in(&rows, 1), None);
    }

    #[test]
    fn upward_selection_yields_negative_moves() {
        let rows: Vec<String> = ["  1. first", "\u{203A} 2. second"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(menu_moves_in(&rows, 0), Some(-1));
    }

    #[test]
    fn click_on_option_sends_arrows_and_enter() {
        let mut m = menu();
        let (to_child, _) = m.on_input(&sgr(0, 10, 7, false));
        assert_eq!(to_child, b"\x1b[B\x1b[B\r");
        // The paired release is swallowed.
        let (to_child, _) = m.on_input(&sgr(0, 10, 7, true));
        assert_eq!(to_child, b"");
    }

    #[test]
    fn click_on_marked_option_sends_enter_only() {
        let mut m = menu();
        let (to_child, _) = m.on_input(&sgr(0, 3, 4, false));
        assert_eq!(to_child, b"\r");
    }

    #[test]
    fn click_uses_application_cursor_arrows_when_set() {
        let mut m = menu();
        m.on_output(b"\x1b[?1h");
        let (to_child, _) = m.on_input(&sgr(0, 3, 7, false));
        assert_eq!(to_child, b"\x1bOB\x1bOB\r");
    }

    #[test]
    fn click_outside_menu_is_dropped_when_child_has_no_mouse() {
        let mut m = menu();
        let (to_child, _) = m.on_input(&sgr(0, 0, 0, false));
        assert_eq!(to_child, b"");
    }

    #[test]
    fn click_outside_menu_is_forwarded_when_child_uses_sgr() {
        let mut m = menu();
        m.on_output(b"\x1b[?1000h\x1b[?1006h");
        let seq = sgr(0, 0, 0, false);
        let (to_child, _) = m.on_input(&seq);
        assert_eq!(to_child, seq);
    }

    #[test]
    fn wheel_is_forwarded_in_x10_encoding_when_child_lacks_sgr() {
        let mut m = menu();
        m.on_output(b"\x1b[?1000h");
        let (to_child, _) = m.on_input(&sgr(64, 5, 5, false));
        assert_eq!(to_child, b"\x1b[M\x60\x26\x26");
    }

    #[test]
    fn motion_outside_menu_is_not_forwarded_to_press_only_child() {
        let mut m = menu();
        m.on_output(b"\x1b[?1000h\x1b[?1006h");
        let (to_child, _) = m.on_input(&sgr(35, 5, 0, false));
        assert_eq!(to_child, b"");
    }

    #[test]
    fn hover_steers_selection_and_click_confirms() {
        let mut m = menu();
        // Hover option 3: two steps down.
        let (to_child, _) = m.on_input(&sgr(35, 10, 7, false));
        assert_eq!(to_child, b"\x1b[B\x1b[B");
        // Motion on the same option resends nothing while the child's
        // redraw is still in flight.
        let (to_child, _) = m.on_input(&sgr(35, 12, 7, false));
        assert_eq!(to_child, b"");
        // Click confirms without extra arrows.
        let (to_child, _) = m.on_input(&sgr(0, 12, 7, false));
        assert_eq!(to_child, b"\r");
    }

    #[test]
    fn quick_hover_across_options_sends_incremental_arrows() {
        let mut m = menu();
        let (to_child, _) = m.on_input(&sgr(35, 10, 5, false)); // option 2
        assert_eq!(to_child, b"\x1b[B");
        // No redraw has landed yet; hovering option 3 adds one step.
        let (to_child, _) = m.on_input(&sgr(35, 10, 7, false));
        assert_eq!(to_child, b"\x1b[B");
    }

    #[test]
    fn steering_resumes_from_redrawn_marker() {
        let mut m = menu();
        let (to_child, _) = m.on_input(&sgr(35, 10, 7, false)); // steer to option 3
        assert_eq!(to_child, b"\x1b[B\x1b[B");
        // The child redraws with the marker on option 3.
        let redraw = CODEX_MENU
            .replace("\u{203A} 1.", "  1.")
            .replace("  3.", "\u{203A} 3.");
        m.on_output(format!("\x1b[H{redraw}").as_bytes());
        // The model has caught up: hovering option 3 clears steering.
        let (to_child, _) = m.on_input(&sgr(35, 10, 7, false));
        assert_eq!(to_child, b"");
        // Hover option 1: two steps up from the redrawn marker.
        let (to_child, _) = m.on_input(&sgr(35, 10, 4, false));
        assert_eq!(to_child, b"\x1b[A\x1b[A");
    }

    #[test]
    fn hover_toggles_pointer_shape() {
        let mut m = menu();
        let (_, to_term) = m.on_input(&sgr(35, 10, 5, false));
        assert_eq!(to_term, POINTER_ON);
        let (_, to_term) = m.on_input(&sgr(35, 10, 5, false));
        assert_eq!(to_term, b"");
        let (_, to_term) = m.on_input(&sgr(35, 10, 9, false));
        assert_eq!(to_term, POINTER_OFF);
    }

    #[test]
    fn tmux_pointer_mode_wraps_pointer_oscs() {
        let mut m = menu();
        m.set_tmux_pointer(true);
        let (_, to_term) = m.on_input(&sgr(35, 10, 4, false));
        assert_eq!(to_term, tmux_wrap(POINTER_ON));
    }

    #[test]
    fn pointer_resets_when_menu_disappears_under_mouse() {
        let mut m = menu();
        let (_, to_term) = m.on_input(&sgr(35, 10, 5, false));
        assert_eq!(to_term, POINTER_ON);
        let out = m.on_output(b"\x1b[2J");
        assert!(out.windows(POINTER_OFF.len()).any(|w| w == POINTER_OFF));
    }

    #[test]
    fn keyboard_input_passes_through() {
        let mut m = menu();
        let (to_child, to_term) = m.on_input(b"hello\x1b[A\x1bOP\x1b\x1b");
        assert_eq!(to_child, b"hello\x1b[A\x1bOP\x1b");
        assert_eq!(to_term, b"");
        assert_eq!(m.finish_input(), b"\x1b");
    }

    #[test]
    fn mouse_sequence_split_across_chunks() {
        let mut m = menu();
        let seq = sgr(0, 3, 7, false);
        let (a, b) = seq.split_at(4);
        let (to_child, _) = m.on_input(a);
        assert_eq!(to_child, b"");
        let (to_child, _) = m.on_input(b);
        assert_eq!(to_child, b"\x1b[B\x1b[B\r");
    }

    #[test]
    fn overlong_candidate_is_flushed() {
        let mut m = menu();
        let mut seq = b"\x1b[<".to_vec();
        seq.extend_from_slice(&[b'1'; MAX_PENDING + 8]);
        let (to_child, _) = m.on_input(&seq);
        assert_eq!(&to_child, &seq[..seq.len().min(to_child.len())]);
        assert!(!to_child.is_empty());
    }

    #[test]
    fn child_mode_change_triggers_reassert() {
        let mut m = menu();
        let out = m.on_output(b"\x1b[?1000h");
        assert!(out.windows(MOUSE_ENABLE.len()).any(|w| w == MOUSE_ENABLE));
        // Steady-state output does not re-assert.
        let out = m.on_output(b"plain text");
        assert!(!out.windows(MOUSE_ENABLE.len()).any(|w| w == MOUSE_ENABLE));
    }

    #[test]
    fn mode_sequence_split_across_chunks_triggers_reassert() {
        let mut m = menu();
        let out = m.on_output(b"text\x1b[?1");
        let hit1 = out.windows(MOUSE_ENABLE.len()).any(|w| w == MOUSE_ENABLE);
        let out = m.on_output(b"004h more");
        let hit2 = out.windows(MOUSE_ENABLE.len()).any(|w| w == MOUSE_ENABLE);
        assert!(hit1 || hit2);
    }

    #[test]
    fn parses_sgr_reports() {
        assert_eq!(
            parse_sgr_mouse(b"\x1b[<0;4;8M"),
            Some(MouseEvent {
                code: 0,
                col: 3,
                row: 7,
                release: false
            })
        );
        assert_eq!(
            parse_sgr_mouse(b"\x1b[<35;1;1m"),
            Some(MouseEvent {
                code: 35,
                col: 0,
                row: 0,
                release: true
            })
        );
        assert_eq!(parse_sgr_mouse(b"\x1b[<0;0;0M"), None); // 1-based coords
        assert_eq!(parse_sgr_mouse(b"\x1b[<0;1M"), None);
        assert_eq!(parse_sgr_mouse(b"\x1b[<0;1;1;1M"), None);
    }
}
