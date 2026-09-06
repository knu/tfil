use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn broken_output_stops_a_child_that_keeps_writing() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tfil"))
        .args([
            "--",
            "/bin/sh",
            "-c",
            "while :; do printf 'output\\n'; done",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(1));
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("tfil did not stop after stdout closed");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

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

#[test]
fn queued_output_is_drained_before_cleanup_and_child_status_is_preserved() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tfil"))
        .args([
            "--strip-ink-fake-cursor",
            "--strip-cursor-shape",
            "--strip-osc-titles",
            "--codex-mouse-ui",
            "--tmux-osc-passthrough=22,52",
            "--",
            "/bin/sh",
            "-c",
            "dd if=/dev/zero bs=65536 count=64 2>/dev/null; exit 42",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Keep stdin open so the PTY's cooked-mode EOF echo is not part of the stream.
    let _input = child.stdin.take();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(42));
    let (data, cleanup) = output.stdout.split_at(65536 * 64);
    assert!(data.iter().all(|&byte| byte == 0));
    assert_eq!(cleanup, b"\x1b[?25h");
}
