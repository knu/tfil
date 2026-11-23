use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, atomic::AtomicBool};
use tfil::codex_mouse_ui::CodexMouseUi;
use tfil::filters::{
    CursorShapeFilter, Filter, FilterChain, InkFakeCursorFilter, OscFocusFilter, OscTitleFilter,
    TmuxOscPassthroughFilter, tmux_wrap,
};

mod critty_notify;
mod session;
mod terminal_output;
mod terminal_string_filter;
mod tmux_passthrough;
mod wrapper;

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")");

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Preset {
    /// Enable Codex mouse UI and Critty integration
    Codex,
    /// Strip Ink's fake cursor sequences
    Claude,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum TerminalStringMode {
    /// Block child passthrough, graphics, and clipboard controls
    Strict,
    /// Allow selected graphics and status controls in tmux and herdr-core
    Auto,
}

#[derive(Parser, Debug)]
#[command(
    name = "tfil",
    version = VERSION,
    about = "Run a command through a PTY proxy that filters terminal sequences and enhances interactive TUIs"
)]
struct Cli {
    /// Apply recommended options for an application
    #[arg(long, value_name = "NAME")]
    preset: Option<Preset>,

    /// Drop DECSCUSR (cursor shape) sequences
    #[arg(long)]
    strip_cursor_shape: bool,

    /// Strip Ink's fake cursor sequences so the terminal's native cursor shows through
    #[arg(long)]
    strip_ink_fake_cursor: bool,

    /// Drop OSC 0/1/2 sequences (icon name and window title)
    #[arg(long)]
    strip_osc_titles: bool,

    /// Drop the given OSC codes until the terminal reports focus, and while unfocused.
    /// The child must enable terminal focus reporting.
    #[arg(long, value_name = "CODES", value_delimiter = ',')]
    strip_osc_unfocused: Vec<u16>,

    /// Make Codex CLI's ›-marked numbered menus mouse-driven: hovering
    /// moves the selection to follow the mouse, clicking confirms it,
    /// and a pointer cursor is shown over options (OSC 22)
    #[arg(long)]
    codex_mouse_ui: bool,

    /// Notify Critty of visible terminal request markers using the launch
    /// environment; also notifies TCrit of its older markers
    #[arg(long, alias = "tcrit-notify")]
    critty_notify: bool,

    /// Send a desktop notification with the child's exit status
    #[arg(long)]
    notify_on_exit: bool,

    /// Wrap the given OSC sequences (comma-separated codes, e.g. "22"
    /// or "22,52") in a tmux DCS passthrough so they reach the outer
    /// terminal when running inside tmux (requires tmux 3.3+ with
    /// `allow-passthrough on`)
    #[arg(long, value_name = "CODES", value_delimiter = ',')]
    tmux_osc_passthrough: Vec<u16>,

    /// Filter child terminal strings (default mode when specified without a value: strict)
    #[arg(long, value_name = "MODE", num_args = 0..=1, require_equals = true, default_missing_value = "strict")]
    filter_terminal_strings: Option<TerminalStringMode>,

    /// Temporarily allow tmux passthrough from this pane, including when hidden.
    #[arg(long)]
    tmux_managed_passthrough: bool,

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
    fn apply_preset(&mut self) {
        match self.preset {
            Some(Preset::Codex) => {
                self.codex_mouse_ui = true;
                self.critty_notify = true;
            }
            Some(Preset::Claude) => self.strip_ink_fake_cursor = true,
            None => {}
        }
    }

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
        if !self.strip_osc_unfocused.is_empty() {
            let codes: Vec<String> = self
                .strip_osc_unfocused
                .iter()
                .map(u16::to_string)
                .collect();
            args.push(format!("--strip-osc-unfocused={}", codes.join(",")));
        }
        if self.codex_mouse_ui {
            args.push("--codex-mouse-ui".to_string());
        }
        if self.critty_notify {
            args.push("--critty-notify".to_string());
        }
        if self.notify_on_exit {
            args.push("--notify-on-exit".to_string());
        }
        if let Some(mode) = self.filter_terminal_strings {
            args.push(match mode {
                TerminalStringMode::Strict => "--filter-terminal-strings".to_string(),
                TerminalStringMode::Auto => "--filter-terminal-strings=auto".to_string(),
            });
        }
        if self.tmux_managed_passthrough {
            args.push("--tmux-managed-passthrough".to_string());
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
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == tmux_passthrough::WATCHDOG_ARG)
    {
        return match tmux_passthrough::watch() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("tfil: tmux passthrough watchdog: {error:#}");
                ExitCode::FAILURE
            }
        };
    }
    let mut cli = Cli::parse();
    cli.apply_preset();

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

    let notify = cli
        .critty_notify
        .then(critty_notify::Config::capture)
        .flatten();
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

    match run(cli, program, args, notify) {
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

fn run(
    cli: Cli,
    program: PathBuf,
    args: Vec<String>,
    notify: Option<critty_notify::Config>,
) -> Result<i32> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(current_pty_size())
        .context("openpty failed")?;

    // portable-pty otherwise searches PATH for relative paths like "bin/cmd".
    let program = if program.is_relative() && program.as_os_str().as_encoded_bytes().contains(&b'/')
    {
        std::env::current_dir()
            .context("get current directory")?
            .join(program)
    } else {
        program
    };
    let mut cmd = CommandBuilder::new(&program);
    for arg in &args {
        cmd.arg(arg);
    }
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }

    let in_tmux = std::env::var_os("TMUX").is_some_and(|value| !value.is_empty());
    let auto_endpoint = if cli.filter_terminal_strings == Some(TerminalStringMode::Auto) {
        if in_tmux {
            Some(terminal_string_filter::Endpoint::Tmux {
                image_base: tmux_passthrough::kitty_image_base(
                    std::env::var("TMUX_PANE").ok().as_deref(),
                ),
            })
        } else if ["HC_TERMINAL_ID", "HC_TERMINAL_SOCKET"]
            .iter()
            .all(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
        {
            Some(terminal_string_filter::Endpoint::Herdr)
        } else {
            None
        }
    } else {
        None
    };
    let _passthrough = if cli.tmux_managed_passthrough && in_tmux {
        tmux_passthrough::PassthroughGuard::enter()?
    } else {
        None
    };
    let mut child = pair.slave.spawn_command(cmd).context("spawn failed")?;
    drop(pair.slave);

    let mut filters: Vec<Box<dyn Filter + Send>> = Vec::new();
    if cli.strip_ink_fake_cursor {
        filters.push(Box::new(InkFakeCursorFilter::new()));
    }
    if cli.strip_osc_titles && auto_endpoint.is_none() {
        filters.push(Box::new(OscTitleFilter::new()));
    }
    if cli.strip_cursor_shape {
        filters.push(Box::new(CursorShapeFilter::new()));
    }
    let filter_strings = cli.filter_terminal_strings.is_some();
    let focus_pointer = filter_strings && cli.codex_mouse_ui;
    let focus = (auto_endpoint.is_some() || !cli.strip_osc_unfocused.is_empty() || focus_pointer)
        .then(|| Arc::new(AtomicBool::new(false)));
    if let Some(endpoint) = auto_endpoint {
        filters.push(Box::new(
            terminal_string_filter::TerminalStringFilter::auto(terminal_string_filter::Auto::new(
                endpoint,
                Arc::clone(focus.as_ref().expect("auto filtering tracks focus")),
                cli.strip_osc_unfocused,
                cli.tmux_osc_passthrough.contains(&22),
                cli.strip_osc_titles,
            )),
        ));
    } else if !cli.strip_osc_unfocused.is_empty() {
        filters.push(Box::new(OscFocusFilter::new(
            cli.strip_osc_unfocused,
            Arc::clone(focus.as_ref().expect("OSC focus tracking enabled")),
        )));
    }
    if filter_strings && auto_endpoint.is_none() {
        filters.push(Box::new(
            terminal_string_filter::TerminalStringFilter::default(),
        ));
    }
    let wrap_osc = auto_endpoint.is_none()
        && !cli.tmux_osc_passthrough.is_empty()
        && (in_tmux || !filter_strings);
    if wrap_osc {
        filters.push(Box::new(TmuxOscPassthroughFilter::new(
            cli.tmux_osc_passthrough.clone(),
        )));
    }
    let tmux_pointer = (auto_endpoint.is_some() && in_tmux)
        || (wrap_osc && cli.tmux_osc_passthrough.contains(&22));
    let filters = FilterChain::new(filters);

    let mouse = cli.codex_mouse_ui.then(|| {
        let size = current_pty_size();
        let mut m = CodexMouseUi::new(size.rows, size.cols);
        m.set_tmux_pointer(tmux_pointer);
        if focus_pointer && let Some(focused) = &focus {
            m.set_pointer_focus(Arc::clone(focused));
        }
        m
    });

    // Line editing belongs to the slave PTY; forward every input byte unchanged.
    let _raw_guard = RawModeGuard::enter()?;
    let debug_dump = debug_dump_path(cli.debug_dump.as_deref());
    let notify_on_exit = cli.notify_on_exit && io::stdout().is_terminal();
    let status = session::run(
        child.as_mut(),
        pair.master,
        filters,
        mouse,
        session::Options {
            restore_cursor: cli.strip_ink_fake_cursor,
            tmux_pointer,
            focus,
            dump: debug_dump.as_deref().and_then(open_dump_file),
            notify,
            notify_on_exit,
        },
    )?;
    if notify_on_exit {
        let message = format!("\x1b]777;notify;tfil;Child exited with status {status}.\x07");
        let mut stdout = io::stdout().lock();
        // Reporting the exit is optional. Preserve the child's status even
        // if the terminal closes after the session has finished.
        let _ = if in_tmux {
            stdout.write_all(&tmux_wrap(message.as_bytes()))
        } else {
            stdout.write_all(message.as_bytes())
        }
        .and_then(|()| stdout.flush());
    }
    Ok(status)
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

        // Exclude options that are not serialized, including the expanded preset.
        const EXCLUDED: &[&str] = &[
            "preset",
            "wrap",
            "create-wrapper",
            "force",
            "help",
            "version",
        ];

        let cli = Cli::parse_from([
            "tfil",
            "--strip-cursor-shape",
            "--strip-ink-fake-cursor",
            "--strip-osc-titles",
            "--strip-osc-unfocused=22,52",
            "--codex-mouse-ui",
            "--critty-notify",
            "--notify-on-exit",
            "--filter-terminal-strings",
            "--tmux-managed-passthrough",
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
    fn wrapper_args_expand_presets_and_keep_explicit_options() {
        let mut codex = Cli::parse_from(["tfil", "--preset=codex", "--create-wrapper=codex"]);
        codex.apply_preset();
        assert_eq!(
            codex.to_wrapper_args(),
            ["--codex-mouse-ui", "--critty-notify"]
        );

        let mut codex = Cli::parse_from([
            "tfil",
            "--tmux-osc-passthrough=133",
            "--preset=codex",
            "--notify-on-exit",
            "--create-wrapper=codex",
        ]);
        codex.apply_preset();
        assert_eq!(codex.tmux_osc_passthrough, [133]);
        assert!(codex.notify_on_exit);

        let mut claude = Cli::parse_from(["tfil", "--preset=claude", "cmd", "--resume"]);
        claude.apply_preset();
        assert_eq!(claude.to_wrapper_args(), ["--strip-ink-fake-cursor"]);
        assert_eq!(claude.args, ["--resume"]);
    }

    #[test]
    fn terminal_string_mode_preserves_legacy_positionals_and_wrapper_arguments() {
        let strict = Cli::parse_from(["tfil", "--filter-terminal-strings", "strict"]);
        assert_eq!(strict.command.as_deref(), Some("strict"));
        assert_eq!(
            strict.filter_terminal_strings,
            Some(TerminalStringMode::Strict)
        );
        assert_eq!(strict.to_wrapper_args(), ["--filter-terminal-strings"]);

        let auto = Cli::parse_from(["tfil", "--filter-terminal-strings=auto", "cmd"]);
        assert_eq!(auto.filter_terminal_strings, Some(TerminalStringMode::Auto));
        assert_eq!(auto.to_wrapper_args(), ["--filter-terminal-strings=auto"]);
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
