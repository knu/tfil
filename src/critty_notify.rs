//! Observe the visible terminal without changing its output.  Notifications are
//! best effort; neither queue pressure nor a failed helper affects the session.

use crossbeam_channel::{Receiver, Sender, bounded};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::num::NonZeroU16;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const QUEUE_CAPACITY: usize = 8;
const SEEN_CAPACITY: usize = 256;
const SEEN_TTL: Duration = Duration::from_secs(120);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const ID_LEN: usize = 32;

/// Marker prefixes and the commands they notify.  TCrit was renamed Critty;
/// its markers stay supported while older installations remain.
const HELPERS: [(&str, &str); 2] = [("CRITTY-", "critty"), ("TCRIT-", "tcrit")];

/// A request ID observed after the marker prefix of `HELPERS[helper]`.
#[derive(Clone, Debug, PartialEq)]
struct Marker {
    helper: usize,
    id: String,
}

pub(crate) struct Config {
    programs: [Option<PathBuf>; HELPERS.len()],
    environment: Vec<(OsString, OsString)>,
}

impl Config {
    /// Capture before wrapper resolution or child startup can change context.
    pub(crate) fn capture() -> Option<Self> {
        let cwd = std::env::current_dir().ok()?;
        let path = std::env::var_os("PATH")?;
        let programs = HELPERS.map(|(_, name)| {
            std::env::split_paths(&path).find_map(|dir| {
                let program = cwd.join(dir).join(name);
                program
                    .metadata()
                    .ok()
                    .filter(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)?;
                Some(program)
            })
        });
        programs.iter().any(Option::is_some).then(|| Self {
            programs,
            environment: std::env::vars_os().collect(),
        })
    }

    fn command(&self, marker: &Marker) -> Option<Command> {
        let mut command = Command::new(self.programs[marker.helper].as_ref()?);
        command
            .args(["terminal", "notify", &marker.id])
            .env_clear()
            .envs(self.environment.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Some(command)
    }
}

pub(crate) struct Observer {
    parser: vt100::Parser,
    seen: VecDeque<(Marker, Instant)>,
    sender: Option<Sender<Marker>>,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Observer {
    pub(crate) fn start(config: Config, rows: u16, cols: u16) -> Option<Self> {
        let (sender, receiver) = bounded(QUEUE_CAPACITY);
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let worker = thread::Builder::new()
            .name("critty-notify".into())
            .spawn(move || notify(config, receiver, stop))
            .ok()?;
        Some(Self {
            parser: vt100::Parser::new(size(rows, 1), size(cols, 2), 0),
            seen: VecDeque::new(),
            sender: Some(sender),
            stopping,
            worker: Some(worker),
        })
    }

    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        // Match the mouse model's minimum width for wide characters.
        self.parser.set_size(size(rows, 1), size(cols, 2));
    }

    pub(crate) fn observe(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        let now = Instant::now();
        while self
            .seen
            .front()
            .is_some_and(|(_, when)| now.duration_since(*when) >= SEEN_TTL)
        {
            self.seen.pop_front();
        }
        for marker in visible_markers(self.parser.screen()) {
            if self.seen.iter().any(|(seen, _)| seen == &marker) {
                continue;
            }
            if self.seen.len() == SEEN_CAPACITY {
                self.seen.pop_front();
            }
            self.seen.push_back((marker.clone(), now));
            // Remember dropped attempts too: a full queue must not cause a
            // redraw to retry indefinitely.
            if let Some(sender) = &self.sender {
                let _ = sender.try_send(marker);
            }
        }
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn size(value: u16, minimum: u16) -> NonZeroU16 {
    NonZeroU16::new(value.max(minimum)).unwrap()
}

fn visible_markers(screen: &vt100::Screen) -> Vec<Marker> {
    let (rows, cols) = screen.size();
    let mut markers = Vec::new();
    // Use cells, not contents(): contents joins soft-wrapped rows and loses
    // the coordinates needed to distinguish an unfinished token at the cursor.
    for row in 0..rows.get() {
        for col in 0..cols.get() {
            for (helper, (prefix, _)) in HELPERS.iter().enumerate() {
                if let Some(id) = marker_id(screen, row, col, prefix) {
                    markers.push(Marker { helper, id });
                }
            }
        }
    }
    markers
}

/// Return the request ID of a complete marker starting at the given cell.
fn marker_id(screen: &vt100::Screen, row: u16, col: u16, prefix: &str) -> Option<String> {
    let end = col + (prefix.len() + ID_LEN) as u16;
    // A token at the right edge might continue on the next row.  Stay
    // conservative on narrow terminals rather than stitch rows.
    if end >= screen.size().1.get() {
        return None;
    }
    let cell = |offset: usize| screen.cell(row, col + offset as u16).unwrap().contents();
    if !prefix
        .bytes()
        .enumerate()
        .all(|(i, byte)| cell(i).as_bytes() == [byte])
    {
        return None;
    }
    let mut id = String::with_capacity(ID_LEN);
    for offset in prefix.len()..prefix.len() + ID_LEN {
        let contents = cell(offset);
        if contents.len() != 1 || !matches!(contents.as_bytes()[0], b'0'..=b'9' | b'a'..=b'f') {
            return None;
        }
        id.push_str(contents);
    }
    let next = screen.cell(row, end).unwrap().contents();
    if next.as_bytes().first().is_some_and(u8::is_ascii_hexdigit) {
        return None;
    }
    // A read boundary is not a token boundary.  Wait for a delimiter
    // or for the cursor to leave, even if a blank cell follows the ID.
    if next.is_empty() && screen.cursor_position() == (row, end) {
        return None;
    }
    Some(id)
}

fn notify(config: Config, receiver: Receiver<Marker>, stopping: Arc<AtomicBool>) {
    while let Ok(marker) = receiver.recv() {
        if stopping.load(Ordering::Relaxed) {
            break;
        }
        let Some(Ok(mut child)) = config.command(&marker).map(|mut c| c.spawn()) else {
            continue;
        };
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline && !stopping.load(Ordering::Relaxed) => {
                    thread::sleep(POLL_INTERVAL);
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn marker(helper: usize, id: &str) -> Marker {
        Marker {
            helper,
            id: id.into(),
        }
    }

    fn detector() -> (Observer, Receiver<Marker>) {
        let (sender, receiver) = bounded(QUEUE_CAPACITY);
        (
            Observer {
                parser: vt100::Parser::new(size(24, 1), size(100, 2), 0),
                seen: VecDeque::new(),
                sender: Some(sender),
                stopping: Arc::new(AtomicBool::new(false)),
                worker: None,
            },
            receiver,
        )
    }

    #[test]
    fn visible_markers_survive_every_read_boundary_and_redraw() {
        for (helper, (prefix, _)) in HELPERS.iter().enumerate() {
            for text in [
                format!("{prefix}{ID}\r\n"),
                format!("{prefix}\x1b[31m{ID}\x1b[m\r\n"),
                format!("old text\r{prefix}{ID}\x1b[K\r\n"),
                format!("\x1b[4;8H{prefix}{ID}\x1b[1;1H"),
                format!("\x1b]8;;https://example.test\x1b\\{prefix}{ID}\x1b]8;;\x1b\\\r\n"),
            ] {
                for split in 0..=text.len() {
                    let (mut observer, receiver) = detector();
                    observer.observe(&text.as_bytes()[..split]);
                    observer.observe(&text.as_bytes()[split..]);
                    assert_eq!(
                        receiver.try_recv().unwrap(),
                        marker(helper, ID),
                        "split {split}: {text:?}"
                    );
                    observer.observe(format!("\x1b[H{prefix}{ID}\r\n").as_bytes());
                    assert!(receiver.is_empty());
                }
                let (mut observer, receiver) = detector();
                for byte in text.bytes() {
                    observer.observe(&[byte]);
                }
                assert_eq!(receiver.try_recv().unwrap(), marker(helper, ID));
                assert!(receiver.is_empty());
            }
        }
    }

    #[test]
    fn both_marker_kinds_are_reported_separately() {
        let (mut observer, receiver) = detector();
        observer.observe(format!("CRITTY-{ID}\r\nTCRIT-{ID}\r\n").as_bytes());
        assert_eq!(receiver.try_recv().unwrap(), marker(0, ID));
        assert_eq!(receiver.try_recv().unwrap(), marker(1, ID));
        assert!(receiver.is_empty());
    }

    #[test]
    fn incomplete_invalid_and_metadata_tokens_are_not_notifications() {
        for (prefix, _) in HELPERS {
            for text in [
                format!("{prefix}{ID}"),
                format!("{prefix}{ID}0\r\n"),
                format!("{prefix}{ID}A\r\n"),
                format!("{prefix}{}\r\n", ID.to_uppercase()),
                format!("{prefix}{}\r\n", &ID[..31]),
                format!("{prefix}{}\r\n{}\r\n", &ID[..16], &ID[16..]),
                format!("{}{ID}\r\n", prefix.to_lowercase()),
                format!("\x1b]0;{prefix}{ID}\x07\r\n"),
                format!("\x1b]8;;{prefix}{ID}\x1b\\link\x1b]8;;\x1b\\\r\n"),
                format!("\x1bPq{prefix}{ID}\x1b\\\r\n"),
                format!("\x1bPtmux;\x1b\x1b]0;{prefix}{ID}\x07\x1b\\\r\n"),
            ] {
                for split in 0..=text.len() {
                    let (mut observer, receiver) = detector();
                    observer.observe(&text.as_bytes()[..split]);
                    observer.observe(&text.as_bytes()[split..]);
                    assert!(receiver.is_empty(), "split {split}: {text:?}");
                }
            }
        }
    }

    #[test]
    fn erased_scrolled_and_alternate_screen_content_is_not_visible() {
        for suffix in ["\r\x1b[2K", "\x1b[2J", "\x1b[?1049h", &"\r\n".repeat(25)] {
            let (mut observer, receiver) = detector();
            observer.observe(format!("CRITTY-{ID}{suffix}").as_bytes());
            assert!(receiver.is_empty(), "{suffix:?}");
        }
    }

    #[test]
    fn cursor_overwrites_and_resize_preserve_parser_state() {
        let (mut observer, receiver) = detector();
        observer.observe(format!("CRITTY-{}x\r\n", &ID[..31]).as_bytes());
        assert!(receiver.is_empty());
        observer.observe(b"\x1b[1;39Hf\r\n");
        assert_eq!(receiver.try_recv().unwrap(), marker(0, ID));
        observer.observe(b"\x1b[?1049h\x1b[31");
        for cols in [1, 4, 53, 100] {
            observer.resize(6, cols);
            observer.observe("m日".as_bytes());
        }
        observer.observe(b"\x1b[?1049l\x1b[2J\x1b[H");
        let next = "f".repeat(32);
        observer.observe(format!("CRITTY-{next}\r\n").as_bytes());
        assert_eq!(receiver.try_recv().unwrap(), marker(0, &next));
    }

    #[test]
    fn queue_and_deduplication_have_fixed_bounds_and_expire() {
        let (mut observer, receiver) = detector();
        for index in 0..SEEN_CAPACITY + 10 {
            observer.observe(format!("\x1b[HCRITTY-{index:032x}\r\n").as_bytes());
        }
        assert_eq!(receiver.len(), QUEUE_CAPACITY);
        assert_eq!(observer.seen.len(), SEEN_CAPACITY);
        while receiver.try_recv().is_ok() {}
        observer.observe(format!("\x1b[HCRITTY-{:032x}\r\n", SEEN_CAPACITY + 9).as_bytes());
        assert!(receiver.is_empty());
        let expired = Instant::now() - SEEN_TTL;
        for (_, when) in &mut observer.seen {
            *when = expired;
        }
        observer.observe(b"");
        assert_eq!(receiver.len(), 1);
        assert_eq!(observer.seen.len(), 1);
    }

    fn script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let program = dir.join(name);
        std::fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    fn config(critty: Option<PathBuf>, tcrit: Option<PathBuf>) -> Config {
        Config {
            programs: [critty, tcrit],
            environment: vec![],
        }
    }

    #[test]
    fn helper_gets_exact_arguments_and_captured_environment() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            "critty",
            "printf '%s\\n' \"$@\" \"$TMUX\" \"$TMUX_PANE\" \"$HERDR_SOCKET_PATH\" \"$HERDR_WORKSPACE_ID\" \"$HERDR_TAB_ID\" \"$HERDR_PANE_ID\" \"$XDG_STATE_HOME\" \"$PATH\" > \"$REPORT\"",
        );
        let mut config = config(Some(program), None);
        let report = dir.path().join("report");
        config.environment = [
            ("REPORT", report.to_str().unwrap()),
            ("TMUX", "/custom/socket,42,0"),
            ("TMUX_PANE", "%7"),
            ("HERDR_SOCKET_PATH", "/custom/herdr"),
            ("HERDR_WORKSPACE_ID", "workspace"),
            ("HERDR_TAB_ID", "tab"),
            ("HERDR_PANE_ID", "pane"),
            ("XDG_STATE_HOME", "/custom/state"),
            ("PATH", "/original/path"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let (sender, receiver) = bounded(1);
        sender.send(marker(0, ID)).unwrap();
        drop(sender);
        notify(config, receiver, Arc::new(AtomicBool::new(false)));
        assert_eq!(
            std::fs::read_to_string(report).unwrap(),
            format!(
                "terminal\nnotify\n{ID}\n/custom/socket,42,0\n%7\n/custom/herdr\nworkspace\ntab\npane\n/custom/state\n/original/path\n"
            )
        );
    }

    #[test]
    fn each_marker_kind_notifies_its_own_helper() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("report");
        let program = |name: &str| {
            Some(script(
                dir.path(),
                name,
                &format!("echo {name} \"$3\" >> \"$REPORT\""),
            ))
        };
        for (critty, tcrit, expected) in [
            (program("critty"), program("tcrit"), "critty a\ntcrit b\n"),
            (program("critty"), None, "critty a\n"),
            (None, program("tcrit"), "tcrit b\n"),
        ] {
            let _ = std::fs::remove_file(&report);
            let mut config = config(critty, tcrit);
            config.environment = vec![("REPORT".into(), report.clone().into_os_string())];
            let (sender, receiver) = bounded(2);
            sender.send(marker(0, "a")).unwrap();
            sender.send(marker(1, "b")).unwrap();
            drop(sender);
            notify(config, receiver, Arc::new(AtomicBool::new(false)));
            assert_eq!(std::fs::read_to_string(&report).unwrap(), expected);
        }
    }

    #[test]
    fn missing_and_unsuccessful_helpers_do_not_stop_the_worker() {
        let dir = tempfile::tempdir().unwrap();
        for config in [
            config(
                Some(script(
                    dir.path(),
                    "critty",
                    "echo ignored; echo ignored >&2; exit 17",
                )),
                None,
            ),
            config(Some(dir.path().join("missing")), None),
        ] {
            let (sender, receiver) = bounded(2);
            sender.send(marker(0, ID)).unwrap();
            sender.send(marker(0, ID)).unwrap();
            drop(sender);
            notify(config, receiver, Arc::new(AtomicBool::new(false)));
        }
    }

    #[test]
    fn timeout_kills_and_reaps_helper_and_shutdown_interrupts_it() {
        for shutdown in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("pid");
            let mut config = config(
                Some(script(
                    dir.path(),
                    "critty",
                    "echo $$ > \"$PID_FILE\"; exec /bin/sleep 30",
                )),
                None,
            );
            config
                .environment
                .push(("PID_FILE".into(), pid_file.clone().into_os_string()));
            let mut observer = Observer::start(config, 24, 100).unwrap();
            observer.observe(format!("CRITTY-{ID}\r\n").as_bytes());
            let deadline = Instant::now() + Duration::from_secs(5);
            while !pid_file.exists() {
                assert!(Instant::now() < deadline);
                thread::sleep(POLL_INTERVAL);
            }
            let pid: libc::pid_t = std::fs::read_to_string(pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            if !shutdown {
                while unsafe { libc::kill(pid, 0) } == 0 {
                    assert!(Instant::now() < deadline, "helper was not reaped");
                    thread::sleep(POLL_INTERVAL);
                }
            }
            let before = Instant::now();
            drop(observer);
            assert!(before.elapsed() < Duration::from_secs(1));
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(
                unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
    }
}
