use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

#[test]
fn generated_wrapper_runs_target_through_tfil() {
    let tfil = Path::new(env!("CARGO_BIN_EXE_tfil"));
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let real_dir = tmp.path().join("real");
    fs::create_dir(&bin_dir).unwrap();
    fs::create_dir(&real_dir).unwrap();

    let target = real_dir.join("hello");
    fs::write(&target, "#!/bin/sh\necho hello-from-target \"$@\"\n").unwrap();
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

    // The wrapper resolves its basename in PATH, skipping itself, and
    // runs the real command through tfil's PTY proxy.
    let output = Command::new(&wrapper)
        .arg("world")
        .env("PATH", &path_var)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "wrapper failed: {stdout}");
    assert!(
        stdout.contains("hello-from-target world"),
        "unexpected output: {stdout}"
    );
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
