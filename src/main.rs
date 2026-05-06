use anyhow::{Context, Result};
use clap::Parser;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use tfil::filters::{CursorShapeFilter, Filter, InkFakeCursorFilter, OscTitleFilter};

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")");
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";

#[derive(Parser, Debug)]
#[command(
    name = "tfil",
    version = VERSION,
    about = "Run a command through a configurable terminal output filter"
)]
struct Cli {
    /// Strip Ink's fake cursor sequences so the terminal's native cursor shows through
    #[arg(long)]
    strip_ink_fake_cursor: bool,

    /// Drop OSC 0/1/2 sequences (icon name and window title)
    #[arg(long)]
    strip_osc_titles: bool,

    /// Drop DECSCUSR (cursor shape) sequences
    #[arg(long)]
    strip_cursor_shape: bool,

    /// Write the pre-filter PTY output stream to FILE for debugging
    #[arg(long, value_name = "FILE")]
    debug_dump: Option<std::path::PathBuf>,

    /// Command to run, for example "claude", "gemini", or "ccmanager"
    command: String,

    /// Arguments to pass to the command
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("tfil: {:#}", e);
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(current_pty_size())
        .context("openpty failed")?;

    let mut cmd = CommandBuilder::new(&cli.command);
    for arg in &cli.args {
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

    // Always put our stdin in raw mode: line editing is the slave PTY's
    // job (its termios is cooked by default), so the parent must forward
    // every byte without local cooking.
    let _raw_guard = RawModeGuard::enter()?;
    let done = Arc::new(AtomicBool::new(false));

    // child -> filter -> stdout
    let stdout_thread = {
        let done = done.clone();
        let mut dump = cli.debug_dump.as_deref().and_then(open_dump_file);
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
                        let mut lock = stdout.lock();
                        lock.write_all(out)?;
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
    thread::spawn(move || {
        let mut buf = [0u8; 65536];
        let stdin = io::stdin();
        while !stdin_done.load(Ordering::SeqCst) {
            let mut lock = stdin.lock();
            match lock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = writer.flush();
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    // SIGWINCH -> resize pty
    let winch_master = master.clone();
    let winch_done = done.clone();
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
        }
    });

    let status = child.wait().context("wait failed")?;
    done.store(true, Ordering::SeqCst);
    let _ = stdout_thread.join();

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

fn open_dump_file(path: &std::path::Path) -> Option<std::fs::File> {
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
