use anyhow::{Context, Result};
use clap::Parser;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use tfil::codex_mouse_ui::{CodexMouseUi, MOUSE_DISABLE, POINTER_OFF};
use tfil::filters::{
    CursorShapeFilter, Filter, InkFakeCursorFilter, OscTitleFilter, TmuxOscPassthroughFilter,
    tmux_wrap,
};

mod wrapper;

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")");
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";

#[derive(Parser, Debug)]
#[command(
    name = "tfil",
    version = VERSION,
    about = "Run a command through a PTY proxy that filters terminal sequences and enhances interactive TUIs"
)]
struct Cli {
    /// Drop DECSCUSR (cursor shape) sequences
    #[arg(long)]
    strip_cursor_shape: bool,

    /// Strip Ink's fake cursor sequences so the terminal's native cursor shows through
    #[arg(long)]
    strip_ink_fake_cursor: bool,

    /// Drop OSC 0/1/2 sequences (icon name and window title)
    #[arg(long)]
    strip_osc_titles: bool,

    /// Make Codex CLI's ›-marked numbered menus mouse-driven: hovering
    /// moves the selection to follow the mouse, clicking confirms it,
    /// and a pointer cursor is shown over options (OSC 22)
    #[arg(long)]
    codex_mouse_ui: bool,

    /// Wrap the given OSC sequences (comma-separated codes, e.g. "22"
    /// or "22,52") in a tmux DCS passthrough so they reach the outer
    /// terminal when running inside tmux (requires tmux 3.3+ with
    /// `allow-passthrough on`)
    #[arg(long, value_name = "CODES", value_delimiter = ',')]
    tmux_osc_passthrough: Vec<u16>,

    /// Write the pre-filter PTY output stream to FILE for debugging.
    /// Defaults to TFIL_DEBUG_DUMP when set.
    #[arg(long, value_name = "FILE")]
    debug_dump: Option<PathBuf>,

    /// Run as a wrapper: take the command name from SELF's basename,
    /// resolve it in PATH skipping SELF and other tfil wrappers, and
    /// treat all positional arguments as the command's arguments.
    /// Exits 127 when the command cannot be found.  Meant to be
    /// called from a wrapper script as: tfil --wrap="$0" [OPTIONS]
    /// -- "$@"
    #[arg(long, value_name = "SELF", conflicts_with = "create_wrapper")]
    wrap: Option<PathBuf>,

    /// Instead of running a command, write a wrapper script to PATH
    /// that runs the command named after its basename through tfil
    /// with the given options; positional arguments given after "--"
    /// are embedded as fixed leading arguments.  May be given
    /// multiple times.
    #[arg(long, value_name = "PATH")]
    create_wrapper: Vec<PathBuf>,

    /// Overwrite existing files that are not tfil wrappers
    #[arg(long, requires = "create_wrapper")]
    force: bool,

    /// Command to run, for example "claude", "gemini", or "ccmanager"
    #[arg(required_unless_present_any = ["wrap", "create_wrapper"])]
    command: Option<String>,

    /// Arguments to pass to the command
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

impl Cli {
    /// Rebuild the behavior options as command-line arguments for
    /// embedding into a wrapper script.
    fn to_wrapper_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.strip_cursor_shape {
            args.push("--strip-cursor-shape".to_string());
        }
        if self.strip_ink_fake_cursor {
            args.push("--strip-ink-fake-cursor".to_string());
        }
        if self.strip_osc_titles {
            args.push("--strip-osc-titles".to_string());
        }
        if self.codex_mouse_ui {
            args.push("--codex-mouse-ui".to_string());
        }
        if !self.tmux_osc_passthrough.is_empty() {
            let codes: Vec<String> = self
                .tmux_osc_passthrough
                .iter()
                .map(u16::to_string)
                .collect();
            args.push(format!("--tmux-osc-passthrough={}", codes.join(",")));
        }
        if let Some(path) = &self.debug_dump {
            args.push(format!("--debug-dump={}", path.display()));
        }
        args
    }

    /// Positional arguments: the command slot merged with the trailing
    /// arguments, for modes where no command name is expected.
    fn positional_args(&self) -> Vec<String> {
        self.command.iter().chain(&self.args).cloned().collect()
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if !cli.create_wrapper.is_empty() {
        return match wrapper::create_wrappers(
            &cli.create_wrapper,
            cli.force,
            &cli.to_wrapper_args(),
            &cli.positional_args(),
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("tfil: {:#}", e);
                ExitCode::from(1)
            }
        };
    }

    let bypass_pty = should_bypass_pty(
        cli.wrap.is_some(),
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
    );
    let (program, args) = if let Some(self_path) = &cli.wrap {
        match wrapper::resolve_command(self_path) {
            Ok(target) => (target, cli.positional_args()),
            Err(e) => {
                eprintln!("tfil: {:#}", e);
                return ExitCode::from(127);
            }
        }
    } else {
        let command = cli.command.clone().expect("clap enforces command");
        (PathBuf::from(command), cli.args.clone())
    };

    if bypass_pty {
        let error = std::process::Command::new(&program).args(&args).exec();
        eprintln!("tfil: {}: exec failed: {}", program.display(), error);
        return ExitCode::from(126);
    }

    match run(cli, program, args) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("tfil: {:#}", e);
            ExitCode::from(1)
        }
    }
}

fn should_bypass_pty(wrap: bool, stdin_tty: bool, stdout_tty: bool) -> bool {
    wrap && (!stdin_tty || !stdout_tty)
}

fn run(cli: Cli, program: PathBuf, args: Vec<String>) -> Result<i32> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(current_pty_size())
        .context("openpty failed")?;

    let mut cmd = CommandBuilder::new(&program);
    for arg in &args {
        cmd.arg(arg);
    }
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }

    let mut child = pair.slave.spawn_command(cmd).context("spawn failed")?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("clone reader")?;
    let mut writer = pair.master.take_writer().context("take writer")?;
    let master = Arc::new(Mutex::new(pair.master));

    let mut filters: Vec<Box<dyn Filter + Send>> = Vec::new();
    if cli.strip_ink_fake_cursor {
        filters.push(Box::new(InkFakeCursorFilter::new()));
    }
    if cli.strip_osc_titles {
        filters.push(Box::new(OscTitleFilter::new()));
    }
    if cli.strip_cursor_shape {
        filters.push(Box::new(CursorShapeFilter::new()));
    }
    if !cli.tmux_osc_passthrough.is_empty() {
        filters.push(Box::new(TmuxOscPassthroughFilter::new(
            cli.tmux_osc_passthrough.clone(),
        )));
    }
    let tmux_pointer = cli.tmux_osc_passthrough.contains(&22);

    let mouse = cli.codex_mouse_ui.then(|| {
        let size = current_pty_size();
        let mut m = CodexMouseUi::new(size.rows, size.cols);
        m.set_tmux_pointer(tmux_pointer);
        Arc::new(Mutex::new(m))
    });

    // Always put our stdin in raw mode: line editing is the slave PTY's
    // job (its termios is cooked by default), so the parent must forward
    // every byte without local cooking.
    let _raw_guard = RawModeGuard::enter()?;
    let done = Arc::new(AtomicBool::new(false));

    // child -> filter -> stdout
    let debug_dump = debug_dump_path(cli.debug_dump.as_deref());
    let stdout_thread = {
        let done = done.clone();
        let mut dump = debug_dump.as_deref().and_then(open_dump_file);
        let mouse = mouse.clone();
        thread::spawn(move || -> Result<()> {
            let mut filters = filters;
            let mut buf = [0u8; 65536];
            let mut owned: Vec<u8> = Vec::new();
            let stdout = io::stdout();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Some(f) = dump.as_mut() {
                            let _ = f.write_all(&buf[..n]);
                            let _ = f.flush();
                        }
                        let out = run_filters(&mut filters, &buf[..n], &mut owned);
                        let extra = mouse
                            .as_ref()
                            .map(|m| m.lock().unwrap().on_output(out))
                            .filter(|e| !e.is_empty());
                        let mut lock = stdout.lock();
                        lock.write_all(out)?;
                        if let Some(extra) = extra {
                            lock.write_all(&extra)?;
                        }
                        lock.flush()?;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let pending = flush_filters(&mut filters);
            if !pending.is_empty() {
                let mut lock = stdout.lock();
                lock.write_all(&pending)?;
                lock.flush()?;
            }
            done.store(true, Ordering::SeqCst);
            Ok(())
        })
    };

    // stdin -> child
    let stdin_done = done.clone();
    let stdin_mouse = mouse.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 65536];
        let stdin = io::stdin();
        while !stdin_done.load(Ordering::SeqCst) {
            let mut lock = stdin.lock();
            match lock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let to_child;
                    let data: &[u8] = if let Some(m) = &stdin_mouse {
                        let (child_bytes, term_bytes) = m.lock().unwrap().on_input(&buf[..n]);
                        if !term_bytes.is_empty() {
                            let mut out = io::stdout().lock();
                            let _ = out.write_all(&term_bytes);
                            let _ = out.flush();
                        }
                        to_child = child_bytes;
                        &to_child
                    } else {
                        &buf[..n]
                    };
                    if !data.is_empty() {
                        if writer.write_all(data).is_err() {
                            break;
                        }
                        let _ = writer.flush();
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        if let Some(m) = &stdin_mouse {
            let tail = m.lock().unwrap().finish_input();
            if !tail.is_empty() && writer.write_all(&tail).is_ok() {
                let _ = writer.flush();
            }
        }
    });

    // SIGWINCH -> resize pty
    let winch_master = master.clone();
    let winch_done = done.clone();
    let winch_mouse = mouse.clone();
    thread::spawn(move || {
        let Ok(mut signals) = Signals::new([SIGWINCH]) else {
            return;
        };
        for _ in &mut signals {
            if winch_done.load(Ordering::SeqCst) {
                break;
            }
            let size = current_pty_size();
            if let Ok(m) = winch_master.lock() {
                let _ = m.resize(size);
            }
            if let Some(m) = &winch_mouse {
                m.lock().unwrap().resize(size.rows, size.cols);
            }
        }
    });

    let status = child.wait().context("wait failed")?;
    done.store(true, Ordering::SeqCst);
    let _ = stdout_thread.join();

    let mouse_active = mouse
        .as_ref()
        .is_some_and(|mouse| mouse.lock().unwrap().is_active());
    if mouse_active {
        let mut lock = io::stdout().lock();
        let _ = lock.write_all(MOUSE_DISABLE);
        if tmux_pointer {
            let _ = lock.write_all(&tmux_wrap(POINTER_OFF));
        } else {
            let _ = lock.write_all(POINTER_OFF);
        }
        let _ = lock.flush();
    }
    if cli.strip_ink_fake_cursor {
        let _ = io::stdout().write_all(CURSOR_SHOW);
        let _ = io::stdout().flush();
    }

    Ok(status.exit_code() as i32)
}

fn run_filters<'a>(
    filters: &mut [Box<dyn Filter + Send>],
    data: &'a [u8],
    owned: &'a mut Vec<u8>,
) -> &'a [u8] {
    if filters.is_empty() {
        return data;
    }
    let mut current: std::borrow::Cow<'_, [u8]> = std::borrow::Cow::Borrowed(data);
    for f in filters.iter_mut() {
        let next = f.filter(current.as_ref());
        current = std::borrow::Cow::Owned(next.into_owned());
    }
    owned.clear();
    owned.extend_from_slice(current.as_ref());
    owned.as_slice()
}

fn flush_filters(filters: &mut [Box<dyn Filter + Send>]) -> Vec<u8> {
    let mut tail = Vec::new();
    for f in filters.iter_mut() {
        let pending = f.finish();
        if !pending.is_empty() {
            tail.extend_from_slice(&pending);
        }
    }
    tail
}

fn current_pty_size() -> PtySize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0
            && ws.ws_row != 0
            && ws.ws_col != 0
        {
            return PtySize {
                rows: ws.ws_row,
                cols: ws.ws_col,
                pixel_width: ws.ws_xpixel,
                pixel_height: ws.ws_ypixel,
            };
        }
    }
    PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn debug_dump_path(cli_path: Option<&Path>) -> Option<PathBuf> {
    choose_debug_dump_path(cli_path, std::env::var_os("TFIL_DEBUG_DUMP"))
}

fn choose_debug_dump_path(cli_path: Option<&Path>, env_path: Option<OsString>) -> Option<PathBuf> {
    cli_path
        .map(PathBuf::from)
        .or_else(|| env_path.map(PathBuf::from))
}

fn open_dump_file(path: &Path) -> Option<std::fs::File> {
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("tfil: --debug-dump {}: {}", path.display(), e);
            None
        }
    }
}

struct RawModeGuard {
    saved: Option<libc::termios>,
}

impl RawModeGuard {
    fn enter() -> Result<Self> {
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) == 0 {
                return Ok(Self { saved: None });
            }
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return Err(io::Error::last_os_error()).context("tcgetattr failed");
            }
            let mut raw = original;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return Err(io::Error::last_os_error()).context("tcsetattr failed");
            }
            Ok(Self {
                saved: Some(original),
            })
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Some(saved) = self.saved {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn debug_dump_path_uses_env_when_cli_is_absent() {
        let path = choose_debug_dump_path(None, Some(OsString::from("env.dump")));

        assert_eq!(path.as_deref(), Some(Path::new("env.dump")));
    }

    #[test]
    fn debug_dump_path_prefers_cli_over_env() {
        let path = choose_debug_dump_path(
            Some(Path::new("cli.dump")),
            Some(OsString::from("env.dump")),
        );

        assert_eq!(path.as_deref(), Some(Path::new("cli.dump")));
    }

    #[test]
    fn wrapper_args_cover_all_behavior_options() {
        use clap::CommandFactory;

        // Options that configure wrapper handling itself, not child
        // behavior, and thus are never embedded into a script.
        const EXCLUDED: &[&str] = &["wrap", "create-wrapper", "force", "help", "version"];

        let cli = Cli::parse_from([
            "tfil",
            "--strip-cursor-shape",
            "--strip-ink-fake-cursor",
            "--strip-osc-titles",
            "--codex-mouse-ui",
            "--tmux-osc-passthrough=22,52",
            "--debug-dump=dump.log",
            "cmd",
        ]);
        let args = cli.to_wrapper_args();

        for arg in Cli::command().get_arguments() {
            if arg.is_positional() {
                continue;
            }
            let long = arg.get_long().expect("all options have long names");
            if EXCLUDED.contains(&long) {
                continue;
            }
            assert!(
                args.iter()
                    .any(|a| *a == format!("--{long}") || a.starts_with(&format!("--{long}="))),
                "--{long} is not covered; update to_wrapper_args() and this test's argv"
            );
        }
    }

    #[test]
    fn wrap_mode_treats_all_positionals_as_child_args() {
        let cli = Cli::parse_from(["tfil", "--wrap=/home/u/bin/claude", "--", "--resume", "x"]);

        assert_eq!(cli.command.as_deref(), Some("--resume"));
        assert_eq!(cli.args, ["x"]);
        assert_eq!(cli.positional_args(), ["--resume", "x"]);
    }

    #[test]
    fn wrap_conflicts_with_create_wrapper() {
        let result = Cli::try_parse_from([
            "tfil",
            "--wrap=/home/u/bin/claude",
            "--create-wrapper=/home/u/bin/claude",
        ]);

        assert_eq!(
            result.unwrap_err().kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
    }

    #[test]
    fn wrap_mode_accepts_no_positionals() {
        let cli = Cli::parse_from(["tfil", "--wrap=/home/u/bin/claude", "--"]);

        assert_eq!(cli.command, None);
        assert!(cli.positional_args().is_empty());
    }

    #[test]
    fn wrapper_bypasses_pty_when_standard_io_is_not_a_terminal() {
        assert!(should_bypass_pty(true, false, true));
        assert!(should_bypass_pty(true, true, false));
        assert!(!should_bypass_pty(true, true, true));
        assert!(!should_bypass_pty(false, false, false));
    }
}
