use std::process::{Command, Stdio};

#[test]
fn cursor_cleanup_recovers_incomplete_output_without_mouse_ui() {
    let output = Command::new(env!("CARGO_BIN_EXE_tfil"))
        .args([
            "--strip-ink-fake-cursor",
            "--",
            "/bin/sh",
            "-c",
            "printf '\\033[?25l\\033[?20'",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.ends_with(b"\x1b[?20\x18\x1b\\\x18\x1b[?25h"));
    let mut screen = vt100::Parser::new(24, 80, 0);
    screen.process(&output.stdout);
    assert!(!screen.screen().hide_cursor());
}
