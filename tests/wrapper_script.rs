use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

#[test]
fn generated_wrapper_bypasses_pty_for_non_terminal_io() {
    let tfil = Path::new(env!("CARGO_BIN_EXE_tfil"));
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let real_dir = tmp.path().join("real");
    fs::create_dir(&bin_dir).unwrap();
    fs::create_dir(&real_dir).unwrap();

    let target = real_dir.join("hello");
    fs::write(
        &target,
        "#!/bin/sh\nprintf 'hello-from-target %s\\n' \"$1\"\nprintf 'target warning\\n' >&2\n",
    )
    .unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

    let path_var = std::env::join_paths([
        tfil.parent().unwrap(),
        &bin_dir,
        &real_dir,
        Path::new("/usr/bin"),
        Path::new("/bin"),
    ])
    .unwrap();

    let wrapper = bin_dir.join("hello");
    let output = Command::new(tfil)
        .arg(format!("--create-wrapper={}", wrapper.display()))
        .env("PATH", &path_var)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Piped standard I/O bypasses the PTY so newlines and the separation
    // between stdout and stderr remain intact.
    let output = Command::new(&wrapper)
        .arg("world")
        .env("PATH", &path_var)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "wrapper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"hello-from-target world\n");
    assert_eq!(output.stderr, b"target warning\n");
}

#[test]
fn wrapper_exits_127_when_target_is_missing() {
    let tfil = Path::new(env!("CARGO_BIN_EXE_tfil"));
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    fs::create_dir(&bin_dir).unwrap();

    let path_var = std::env::join_paths([
        tfil.parent().unwrap(),
        &bin_dir,
        Path::new("/usr/bin"),
        Path::new("/bin"),
    ])
    .unwrap();

    let wrapper = bin_dir.join("no-such-command");
    let output = Command::new(tfil)
        .arg(format!("--create-wrapper={}", wrapper.display()))
        .env("PATH", &path_var)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());

    let output = Command::new(&wrapper)
        .env("PATH", &path_var)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(127));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not found in PATH"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
