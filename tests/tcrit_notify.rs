use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const ID: &str = "0123456789abcdef0123456789abcdef";

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn notifications_preserve_output_status_and_launch_environment_with_or_without_mouse() {
    for mouse in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("report");
        executable(
            &dir.path().join("tcrit"),
            "#!/bin/sh\nprintf '%s\\n' \"$@\" \"$TMUX_PANE\" \"$XDG_STATE_HOME\" >> \"$REPORT\"\necho hidden; echo hidden >&2\n",
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_tfil"));
        command.arg("--tcrit-notify");
        if mouse {
            command.arg("--codex-mouse-ui");
        }
        let mut child = command
            .args([
                "--",
                "/bin/sh",
                "-c",
                r#"
export TMUX_PANE='%wrong-child'
export XDG_STATE_HOME='/wrong-child-state'
printf 'TCRIT-%s\n' "$ID"
i=0
while [ ! -f "$REPORT" ] && [ "$i" -lt 200 ]; do
    /bin/sleep 0.01
    i=$((i + 1))
done
printf '\033[HTCRIT-%s\n' "$ID"
exit 23
"#,
            ])
            .env("PATH", dir.path())
            .env("ID", ID)
            .env("REPORT", &report)
            .env("TMUX_PANE", "%original")
            .env("XDG_STATE_HOME", "/original-state")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let _input = child.stdin.take();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(23));
        assert_eq!(
            output.stdout,
            format!("TCRIT-{ID}\r\n\x1b[HTCRIT-{ID}\r\n").as_bytes()
        );
        assert!(output.stderr.is_empty(), "{output:?}");
        assert_eq!(
            fs::read_to_string(report).unwrap(),
            format!("terminal\nnotify\n{ID}\n%original\n/original-state\n")
        );
    }
}

#[test]
fn blocked_notification_does_not_delay_pty_input_or_output() {
    let dir = tempfile::tempdir().unwrap();
    let started = dir.path().join("started");
    executable(
        &dir.path().join("tcrit"),
        "#!/bin/sh\necho started > \"$STARTED\"\nexec /bin/sleep 30\n",
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_tfil"))
        .args([
            "--tcrit-notify",
            "--",
            "/bin/sh",
            "-c",
            "printf 'TCRIT-%s\\n' \"$ID\"; read reply; printf 'response:%s\\n' \"$reply\"",
        ])
        .env("PATH", dir.path())
        .env("ID", ID)
        .env("STARTED", &started)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !started.exists() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("notification did not start");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let before = Instant::now();
    let mut input = child.stdin.take().unwrap();
    input.write_all(b"ping\n").unwrap();
    while child.try_wait().unwrap().is_none() {
        if before.elapsed() >= Duration::from_millis(800) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("notification blocked PTY forwarding or shutdown");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("response:ping")
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn generated_wrapper_notifies_through_another_wrapper_in_both_orders() {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    for tfil_first in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let real = dir.path().join("real");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(&real).unwrap();
        let report = dir.path().join("report");
        executable(
            &bin.join("tcrit"),
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$REPORT\"\n",
        );
        let tfil = Path::new(env!("CARGO_BIN_EXE_tfil"));
        let path = std::env::join_paths([&bin, &real, tfil.parent().unwrap()]).unwrap();
        let created = Command::new(tfil)
            .arg("--tcrit-notify")
            .arg(format!("--create-wrapper={}", bin.join("agent").display()))
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(created.status.success());
        let proxy = dir.path().join("proxy");
        executable(&proxy, "#!/bin/sh\nexec \"$NEXT\"\n");
        executable(
            &real.join("agent"),
            if tfil_first {
                "#!/bin/sh\nexec \"$PROXY\"\n"
            } else {
                "#!/bin/sh\nexec \"$PAYLOAD\"\n"
            },
        );
        let payload = dir.path().join("payload");
        executable(
            &payload,
            "#!/bin/sh\nprintf 'TCRIT-%s\\n' \"$ID\"\ni=0\nwhile [ ! -f \"$REPORT\" ] && [ \"$i\" -lt 200 ]; do /bin/sleep 0.01; i=$((i + 1)); done\n",
        );
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(if tfil_first {
            bin.join("agent")
        } else {
            proxy.clone()
        });
        command.env("PATH", &path);
        command.env("ID", ID);
        command.env("REPORT", &report);
        command.env("PROXY", &proxy);
        command.env("PAYLOAD", &payload);
        command.env(
            "NEXT",
            if tfil_first {
                payload
            } else {
                bin.join("agent")
            },
        );
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let output = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("wrapper did not finish");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(child.wait().unwrap().success());
        assert!(
            String::from_utf8(output.join().unwrap())
                .unwrap()
                .contains(&format!("TCRIT-{ID}"))
        );
        assert_eq!(
            fs::read_to_string(report).unwrap(),
            format!("terminal\nnotify\n{ID}\n")
        );
    }
}
