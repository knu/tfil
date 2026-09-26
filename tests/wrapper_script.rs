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

#[test]
fn existing_wrapper_chains_with_another_wrapper_in_either_order() {
    let tfil = Path::new(env!("CARGO_BIN_EXE_tfil"));
    for tfil_first in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let tfil_dir = tmp.path().join("tfil-wrapper");
        let other_dir = tmp.path().join("other-wrapper");
        let real_dir = tmp.path().join("real");
        for dir in [&tfil_dir, &other_dir, &real_dir] {
            fs::create_dir(dir).unwrap();
        }
        let wrapper = tfil_dir.join("wrapped");
        fs::write(
            &wrapper,
            "#!/bin/sh\n# tfil-wrapper\nexec tfil --wrap=\"$0\" -- \"$@\"\n",
        )
        .unwrap();
        let other = other_dir.join("wrapped");
        fs::write(
            &other,
            r#"#!/bin/sh
if [ "${OTHER_WRAPPER_ACTIVE-}" = 1 ]; then
    echo 'wrapper cycle' >&2
    exit 99
fi
export OTHER_WRAPPER_ACTIVE=1
printf 'other wrapper\n' >&2
after_self=false
old_ifs=$IFS
IFS=:
set -f
for dir in $PATH; do
    candidate=${dir:-.}/wrapped
    if [ "$candidate" = "$0" ]; then
        after_self=true
    elif [ "$after_self" = true ] && [ -f "$candidate" ] && [ -x "$candidate" ]; then
        IFS=$old_ifs
        exec "$candidate" "$@"
    fi
done
exit 127
"#,
        )
        .unwrap();
        let real = real_dir.join("wrapped");
        fs::write(&real, "#!/bin/sh\nprintf '<%s>\\n' \"$@\"\nexit 23\n").unwrap();
        for path in [&wrapper, &other, &real] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let dirs = if tfil_first {
            [&tfil_dir, &other_dir]
        } else {
            [&other_dir, &tfil_dir]
        };
        let path_var = std::env::join_paths([
            dirs[0].as_path(),
            dirs[1].as_path(),
            real_dir.as_path(),
            tfil.parent().unwrap(),
            Path::new("/usr/bin"),
            Path::new("/bin"),
        ])
        .unwrap();
        let output = Command::new(dirs[0].join("wrapped"))
            .args(["space in argument", "", "--flag"])
            .env("PATH", path_var)
            .env_remove("OTHER_WRAPPER_ACTIVE")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(23), "{output:?}");
        assert_eq!(output.stdout, b"<space in argument>\n<>\n<--flag>\n");
        assert_eq!(output.stderr, b"other wrapper\n");
    }
}
