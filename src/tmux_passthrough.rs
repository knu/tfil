use anyhow::{Context, Result, bail};
use std::{
    ffi::{CStr, OsStr, OsString},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, process::CommandExt},
    },
    process::{Child, Command, Stdio},
};

pub(crate) const WATCHDOG_ARG: &str = "--internal-tmux-passthrough-watchdog";

/// The 256 Kitty image IDs that Codex reserves for this tmux pane.
pub(crate) fn kitty_image_base(pane: Option<&str>) -> Option<u32> {
    let pane = pane?.strip_prefix('%')?.parse::<u32>().ok()?;
    (pane < 0x8000).then(|| 0x80_0000 + (pane << 8))
}

pub(crate) struct PassthroughGuard(Child);

impl PassthroughGuard {
    pub(crate) fn enter() -> Result<Option<Self>> {
        let pane = Pane::from_environment()?;
        let tty = pane.command(&["display-message", "-p", "-t", &pane.id, "#{pane_tty}"])?;
        if !is_foreground_on(&tty) {
            return Ok(None);
        }

        // The watchdog survives signals sent to the wrapper's process group.
        // Its stdin closes even if the wrapper is killed before its destructors run.
        let mut guard = Self(
            Command::new(std::env::current_exe()?)
                .arg(WATCHDOG_ARG)
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .context("start tmux passthrough watchdog")?,
        );
        let mut ready = [0];
        let mut stdout = guard.0.stdout.take().expect("piped stdout");
        stdout
            .read_exact(&mut ready)
            .context("tmux passthrough setup failed")?;
        Ok(Some(guard))
    }
}

impl Drop for PassthroughGuard {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        let _ = self.0.wait();
    }
}

pub(crate) fn watch() -> Result<()> {
    let pane = Pane::from_environment()?;
    let previous = pane.local_value()?;
    if previous != "all" {
        pane.set("all")?;
    }
    let result = (|| -> io::Result<()> {
        io::stdout().write_all(b"1")?;
        io::stdout().flush()?;
        io::copy(&mut io::stdin(), &mut io::sink())?;
        Ok(())
    })();
    if previous != "all" && pane.local_value()? == "all" {
        if previous.is_empty() {
            pane.command(&["set-option", "-pu", "-t", &pane.id, "allow-passthrough"])?;
        } else {
            pane.set(&previous)?;
        }
    }
    result.context("watch tmux passthrough")
}

fn is_foreground_on(tty: &[u8]) -> bool {
    let stdout = io::stdout();
    let fd = stdout.as_raw_fd();
    let mut name = [0; 1024];
    // Nested PTYs can inherit TMUX_PANE without owning the pane's tty.
    unsafe {
        libc::ttyname_r(fd, name.as_mut_ptr(), name.len()) == 0
            && CStr::from_ptr(name.as_ptr()).to_bytes() == tty.strip_suffix(b"\n").unwrap_or(tty)
            && libc::tcgetpgrp(fd) == libc::getpgrp()
    }
}

struct Pane {
    socket: OsString,
    id: String,
}

impl Pane {
    fn from_environment() -> Result<Self> {
        let tmux = std::env::var_os("TMUX").context("missing TMUX")?;
        let socket = tmux
            .as_encoded_bytes()
            .rsplitn(3, |&byte| byte == b',')
            .nth(2)
            .filter(|socket| !socket.is_empty())
            .context("invalid TMUX")?;
        let id = std::env::var("TMUX_PANE").context("missing TMUX_PANE")?;
        if !id
            .strip_prefix('%')
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        {
            bail!("invalid TMUX_PANE");
        }
        Ok(Self {
            socket: OsStr::from_bytes(socket).to_owned(),
            id,
        })
    }

    fn command(&self, args: &[&str]) -> Result<Vec<u8>> {
        let output = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .context("run tmux")?;
        if !output.status.success() {
            bail!(
                "tmux {}: {}",
                args[0],
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout)
    }

    fn local_value(&self) -> Result<String> {
        let value = self.command(&["show-options", "-pqv", "-t", &self.id, "allow-passthrough"])?;
        let value = std::str::from_utf8(&value)?.trim();
        if !matches!(value, "" | "off" | "on" | "all") {
            bail!("unexpected allow-passthrough value");
        }
        Ok(value.to_owned())
    }

    fn set(&self, value: &str) -> Result<()> {
        self.command(&[
            "set-option",
            "-p",
            "-t",
            &self.id,
            "allow-passthrough",
            value,
        ])?;
        Ok(())
    }
}
