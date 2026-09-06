use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn commands_resolve_relative_paths_and_path_names() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("path/to");
    fs::create_dir_all(&bin_dir).unwrap();
    let target = bin_dir.join("cmd");
    fs::write(&target, "#!/bin/sh\nprintf 'target: %s' \"$1\"\nexit 42\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

    let local = tmp.path().join("cmd");
    fs::write(&local, "#!/bin/sh\nexit 99\n").unwrap();
    fs::set_permissions(&local, fs::Permissions::from_mode(0o755)).unwrap();

    for program in [
        "path/to/cmd",
        "./path/to/cmd",
        "../to/cmd",
        target.to_str().unwrap(),
        "cmd",
    ] {
        let cwd = if program.starts_with("../") {
            &bin_dir
        } else {
            tmp.path()
        };
        let output = Command::new(env!("CARGO_BIN_EXE_tfil"))
            .args(["--", program, "hello world"])
            .current_dir(cwd)
            .env("PATH", &bin_dir)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(42),
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // The PTY may echo control characters when tfil forwards stdin EOF.
        assert!(output.stdout.ends_with(b"target: hello world"), "{program}");
    }
}
