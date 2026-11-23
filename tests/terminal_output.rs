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
    let mut child = Command::new(env!("CARGO_BIN_EXE_tfil"))
        .args([
            "--strip-ink-fake-cursor",
            "--",
            "/bin/sh",
            "-c",
            "printf '\\033[?25l\\033[?20'",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // EOF echo can complete the unfinished sequence before cleanup runs.
    let _input = child.stdin.take();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.ends_with(b"\x1b[?20\x18\x1b\\\x18\x1b[?25h"));
    let mut screen = vt100::Parser::new(24.try_into().unwrap(), 80.try_into().unwrap(), 0);
    screen.process(&output.stdout);
    assert!(!screen.screen().hide_cursor());
}

#[test]
fn queued_output_is_drained_before_cleanup_and_child_status_is_preserved() {
    for mouse_args in [&[][..], &["--codex-mouse-ui"][..]] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tfil"))
            .args(mouse_args)
            .args([
                "--strip-ink-fake-cursor",
                "--strip-cursor-shape",
                "--strip-osc-titles",
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
}

#[test]
fn exit_notification_follows_cleanup_and_preserves_child_status() {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::Read;
    use tfil::filters::tmux_wrap;

    for tmux in [false, true] {
        let pair = native_pty_system().openpty(PtySize::default()).unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_tfil"));
        command.args([
            "--notify-on-exit",
            "--strip-osc-unfocused=777",
            "--",
            "/bin/sh",
            "-c",
            "printf 'output\\033[?20'; exit 42",
        ]);
        command.env_remove("TMUX");
        if tmux {
            command.env("TMUX", "test");
        }
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut actual = Vec::new();
        reader.read_to_end(&mut actual).unwrap();
        assert_eq!(child.wait().unwrap().exit_code(), 42);
        let notification = b"\x1b]777;notify;tfil;Child exited with status 42.\x07";
        let mut expected = b"output\x1b[?20\x18\x1b\\\x18".to_vec();
        if tmux {
            expected.extend(tmux_wrap(notification));
        } else {
            expected.extend_from_slice(notification);
        }
        assert_eq!(actual, expected);
    }
}

#[test]
fn terminal_string_filter_runs_without_tmux_management() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tfil"))
        .env_remove("TMUX")
        .args([
            "--filter-terminal-strings",
            "--",
            "/bin/sh",
            "-c",
            r"printf 'before\033]52;clipboard\007after'",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let _input = child.stdin.take();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"before\x18after");
}

#[test]
fn auto_terminal_strings_selects_transport_from_the_environment() {
    use tfil::filters::tmux_wrap;

    let status = b"\x1b]7501;state=working:app=example\x1b\\";
    let auto = ["--filter-terminal-strings=auto"];
    let codex = ["--preset=codex"];
    let strict_codex = ["--preset=codex", "--filter-terminal-strings=strict"];
    for (environment, options, expected) in [
        (vec![], auto.as_slice(), b"\x18".to_vec()),
        (vec![], codex.as_slice(), status.to_vec()),
        (vec![("TMUX", "test")], &auto, tmux_wrap(status)),
        (vec![("TMUX", "test")], &codex, tmux_wrap(status)),
        (vec![("TMUX", "test")], &strict_codex, b"\x18".to_vec()),
        (
            vec![("HC_TERMINAL_ID", "t1"), ("HC_TERMINAL_SOCKET", "/test")],
            &auto,
            status.to_vec(),
        ),
        (
            vec![("HC_TERMINAL_ID", "t1"), ("HC_TERMINAL_SOCKET", "/test")],
            &codex,
            status.to_vec(),
        ),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tfil"))
            .env_remove("TMUX")
            .env_remove("HC_TERMINAL_ID")
            .env_remove("HC_TERMINAL_SOCKET")
            .envs(environment)
            .args(options)
            .args([
                "--",
                "/bin/sh",
                "-c",
                r"printf '\033]7501;state=working:app=example\033\\'",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let _input = child.stdin.take();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, expected);
    }
}

#[test]
fn codex_preset_can_start_when_tmux_passthrough_setup_fails() {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::Read;

    for explicit in [false, true] {
        let pair = native_pty_system().openpty(PtySize::default()).unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_tfil"));
        command.env("TMUX", "invalid");
        command.arg("--preset=codex");
        if explicit {
            command.arg("--tmux-managed-passthrough");
        }
        command.args(["--", "/bin/sh", "-c", "printf child-started; exit 42"]);
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut output = String::new();
        reader.read_to_string(&mut output).unwrap();

        assert_eq!(
            child.wait().unwrap().exit_code(),
            if explicit { 1 } else { 42 }
        );
        assert_eq!(output.contains("child-started"), !explicit);
        assert_eq!(output.contains("tmux passthrough setup skipped"), !explicit);
        assert!(output.contains("invalid TMUX"));
    }
}
