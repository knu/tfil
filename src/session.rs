//! The coordinator owns the UI model and PTY resizing.  Readers and writers
//! exchange bounded messages; each writer owns its stream for the whole session.
//! Pending sends participate in selection so a stalled writer cannot prevent
//! processing the opposite direction or observing worker failures.

use crate::terminal_output::TerminalOutput;
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Select, Sender, TryRecvError, TrySendError, bounded, select};
use portable_pty::{Child, MasterPty};
use signal_hook::{consts::SIGWINCH, iterator::Signals};
use std::borrow::Cow;
use std::fs::File;
use std::io::{self, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread;
use tfil::codex_mouse_ui::{CodexMouseUi, MOUSE_DISABLE, POINTER_OFF};
use tfil::filters::{Filter, FilterChain, tmux_wrap};

const CHUNK_SIZE: usize = 65536;
const QUEUE_CAPACITY: usize = 8;
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";

pub(crate) struct Options {
    pub restore_cursor: bool,
    pub tmux_pointer: bool,
    pub dump: Option<File>,
}

#[derive(Debug)]
enum ReadEvent {
    Data(Vec<u8>),
    Eof,
}

#[derive(Debug)]
enum TerminalCommand {
    Output { bytes: Vec<u8>, extra: Vec<u8> },
    Controls(Vec<u8>),
    Finish(Vec<u8>),
}

#[derive(Debug)]
enum InputCommand {
    Data(Vec<u8>),
    Eof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Worker {
    OutputReader,
    InputReader,
    TerminalWriter,
    TerminalWriteFailure,
    ChildWriter,
    Resize,
}

struct Completion {
    worker: Worker,
    result: Result<()>,
}

fn spawn_worker(
    worker: Worker,
    completed: Sender<Completion>,
    run: impl FnOnce() -> Result<()> + Send + 'static,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let result = catch_unwind(AssertUnwindSafe(run))
            .map_err(|_| anyhow::anyhow!("{worker:?} panicked"))
            .and_then(|result| result);
        // Completion notices are independent of full data queues.
        let _ = completed.send(Completion { worker, result });
    })
}

pub(crate) fn run(
    child: &mut dyn Child,
    master: Box<dyn MasterPty + Send>,
    filters: FilterChain,
    mouse: Option<CodexMouseUi>,
    options: Options,
) -> Result<i32> {
    let reader = master.try_clone_reader().context("clone reader")?;
    let writer = master.take_writer().context("take writer")?;
    let (output_tx, output_rx) = bounded(QUEUE_CAPACITY);
    let (input_tx, input_rx) = bounded(QUEUE_CAPACITY);
    let (terminal_tx, terminal_rx) = bounded(QUEUE_CAPACITY);
    let (child_tx, child_rx) = bounded(QUEUE_CAPACITY);
    // Five workers plus the terminal writer's optional early failure notice.
    let (completed_tx, completed_rx) = bounded(6);
    let (stop_tx, stop_rx) = bounded::<()>(0);
    let (resize_tx, resize_rx) = bounded(1);
    let (recycle_tx, recycle_rx) = bounded(QUEUE_CAPACITY);

    let cancel = stop_rx.clone();
    spawn_worker(Worker::OutputReader, completed_tx.clone(), move || {
        read_output(reader, filters, options.dump, output_tx, cancel, recycle_rx)
    });
    let cancel = stop_rx.clone();
    spawn_worker(Worker::InputReader, completed_tx.clone(), move || {
        read_input(io::stdin(), input_tx, cancel)
    });
    let tracked = mouse.is_some() || options.restore_cursor;
    let failed = completed_tx.clone();
    let terminal_worker = spawn_worker(Worker::TerminalWriter, completed_tx.clone(), move || {
        write_terminal(io::stdout(), tracked, terminal_rx, recycle_tx, |error| {
            let _ = failed.send(Completion {
                worker: Worker::TerminalWriteFailure,
                result: Err(error),
            });
        })
    });
    spawn_worker(Worker::ChildWriter, completed_tx.clone(), move || {
        write_input(writer, child_rx)
    });

    let signals = Signals::new([SIGWINCH]);
    let signal_handle = signals.as_ref().ok().map(Signals::handle);
    if let Ok(mut signals) = signals {
        spawn_worker(Worker::Resize, completed_tx.clone(), move || {
            for _ in &mut signals {
                // Coalesce notifications, then read the latest size in the coordinator.
                if matches!(resize_tx.try_send(()), Err(TrySendError::Disconnected(_))) {
                    break;
                }
            }
            Ok(())
        });
    }
    drop(completed_tx);
    let ports = Ports {
        output: output_rx,
        input: input_rx,
        terminal: terminal_tx,
        child: child_tx,
        completed: completed_rx,
        resize: resize_rx,
    };
    let result = Coordinator::new(mouse, options.restore_cursor, options.tmux_pointer).run(
        ports,
        stop_tx,
        || {
            let size = crate::current_pty_size();
            let _ = master.resize(size);
            size
        },
        || {
            let _ = child.kill();
        },
    );
    if let Some(handle) = signal_handle {
        handle.close();
    }
    // The terminal worker has reported completion (including failure) before
    // the coordinator returns.  Blocking stdin reads never own session state.
    let _ = terminal_worker.join();
    let status = child.wait().context("wait failed");
    result?;
    Ok(status?.exit_code() as i32)
}

struct Ports {
    output: Receiver<ReadEvent>,
    input: Receiver<ReadEvent>,
    terminal: Sender<TerminalCommand>,
    child: Sender<InputCommand>,
    completed: Receiver<Completion>,
    resize: Receiver<()>,
}

struct Coordinator {
    mouse: Option<CodexMouseUi>,
    restore_cursor: bool,
    tmux_pointer: bool,
    pending_terminal: Option<TerminalCommand>,
    pending_controls: Option<Vec<u8>>,
    pending_child: Option<InputCommand>,
    output_eof: bool,
    input_eof: bool,
    finish_sent: bool,
    error: Option<anyhow::Error>,
}

impl Coordinator {
    fn new(mouse: Option<CodexMouseUi>, restore_cursor: bool, tmux_pointer: bool) -> Self {
        Self {
            mouse,
            restore_cursor,
            tmux_pointer,
            pending_terminal: None,
            pending_controls: None,
            pending_child: None,
            output_eof: false,
            input_eof: false,
            finish_sent: false,
            error: None,
        }
    }

    fn cleanup(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        if self.mouse.as_ref().is_some_and(CodexMouseUi::is_active) {
            bytes.extend_from_slice(MOUSE_DISABLE);
            if self.tmux_pointer {
                bytes.extend(tmux_wrap(POINTER_OFF));
            } else {
                bytes.extend_from_slice(POINTER_OFF);
            }
        }
        if self.restore_cursor {
            bytes.extend_from_slice(CURSOR_SHOW);
        }
        bytes
    }

    fn on_output(&mut self, event: ReadEvent) {
        match event {
            ReadEvent::Data(bytes) => {
                let extra = self
                    .mouse
                    .as_mut()
                    .map(|m| m.on_output(&bytes))
                    .unwrap_or_default();
                self.pending_terminal = Some(TerminalCommand::Output { bytes, extra });
            }
            ReadEvent::Eof => self.output_eof = true,
        }
    }

    fn on_input(&mut self, event: ReadEvent) {
        match event {
            ReadEvent::Data(bytes) => {
                let (bytes, controls) = if let Some(mouse) = &mut self.mouse {
                    mouse.on_input(&bytes)
                } else {
                    (bytes, Vec::new())
                };
                if !bytes.is_empty() {
                    self.pending_child = Some(InputCommand::Data(bytes));
                }
                if !controls.is_empty() {
                    self.pending_controls = Some(controls);
                }
            }
            ReadEvent::Eof => {
                self.input_eof = true;
                let tail = self
                    .mouse
                    .as_mut()
                    .map(CodexMouseUi::finish_input)
                    .unwrap_or_default();
                if !tail.is_empty() {
                    self.pending_child = Some(InputCommand::Data(tail));
                }
            }
        }
    }

    fn run(
        mut self,
        ports: Ports,
        stop: Sender<()>,
        mut resize: impl FnMut() -> portable_pty::PtySize,
        mut abort: impl FnMut(),
    ) -> Result<()> {
        let mut stop = Some(stop);
        let mut child_closed = false;
        let mut resize_closed = false;
        loop {
            if self.pending_terminal.is_none() {
                self.pending_terminal = self.pending_controls.take().map(TerminalCommand::Controls);
            }
            if self.output_eof {
                stop.take();
                self.input_eof = true;
                if self.pending_terminal.is_none() && !self.finish_sent {
                    self.pending_terminal = Some(TerminalCommand::Finish(self.cleanup()));
                    self.finish_sent = true;
                }
            }
            if self.input_eof && self.pending_child.is_none() && !child_closed {
                self.pending_child = Some(InputCommand::Eof);
                child_closed = true;
            }

            let mut select = Select::new();
            let completed = select.recv(&ports.completed);
            let output = (!self.output_eof && self.pending_terminal.is_none())
                .then(|| select.recv(&ports.output));
            let input = (!self.input_eof
                && self.pending_controls.is_none()
                && self.pending_child.is_none())
            .then(|| select.recv(&ports.input));
            let terminal = self
                .pending_terminal
                .is_some()
                .then(|| select.send(&ports.terminal));
            let child = self
                .pending_child
                .is_some()
                .then(|| select.send(&ports.child));
            let resized = (!resize_closed && !self.output_eof).then(|| select.recv(&ports.resize));
            let operation = select.select();
            let index = operation.index();
            let outcome = if index == completed {
                let Completion { worker, result } = operation
                    .recv(&ports.completed)
                    .context("worker notifications disconnected")?;
                if worker == Worker::TerminalWriter {
                    if let Err(error) = result {
                        abort();
                        return Err(self.error.unwrap_or(error));
                    }
                    return self.error.map_or(Ok(()), Err);
                }
                if worker == Worker::ChildWriter {
                    child_closed = true;
                    self.input_eof = true;
                    self.pending_child = None;
                }
                if worker == Worker::OutputReader && result.is_err() {
                    if self.error.is_none() {
                        self.error = result.err();
                        abort();
                    }
                    // Its sender is gone, but previously queued bytes still
                    // precede cleanup.  Drain them before observing disconnect.
                    continue;
                }
                result
            } else if Some(index) == output {
                operation
                    .recv(&ports.output)
                    .map(|event| self.on_output(event))
                    .context("PTY output queue disconnected")
            } else if Some(index) == input {
                operation
                    .recv(&ports.input)
                    .map(|event| self.on_input(event))
                    .context("input queue disconnected")
            } else if Some(index) == terminal {
                operation
                    .send(&ports.terminal, self.pending_terminal.take().unwrap())
                    .map_err(|_| anyhow::anyhow!("terminal writer disconnected"))
            } else if Some(index) == child {
                if operation
                    .send(&ports.child, self.pending_child.take().unwrap())
                    .is_err()
                {
                    child_closed = true;
                    self.input_eof = true;
                }
                Ok(())
            } else if Some(index) == resized {
                if operation.recv(&ports.resize).is_ok() {
                    let size = resize();
                    if let Some(mouse) = &mut self.mouse {
                        mouse.resize(size.rows, size.cols);
                    }
                } else {
                    resize_closed = true;
                }
                Ok(())
            } else {
                unreachable!()
            };
            if let Err(error) = outcome {
                if self.error.is_none() {
                    self.error = Some(error);
                    abort();
                }
                self.output_eof = true;
            }
        }
    }
}

fn send_event(tx: &Sender<ReadEvent>, stop: &Receiver<()>, event: ReadEvent) -> bool {
    select! {
        send(tx, event) -> result => result.is_ok(),
        recv(stop) -> _ => false,
    }
}

fn send_output(tx: &Sender<ReadEvent>, stop: &Receiver<()>, bytes: Vec<u8>) -> bool {
    if bytes.len() <= CHUNK_SIZE {
        bytes.is_empty() || send_event(tx, stop, ReadEvent::Data(bytes))
    } else {
        bytes
            .chunks(CHUNK_SIZE)
            .all(|chunk| send_event(tx, stop, ReadEvent::Data(chunk.to_vec())))
    }
}

fn read_output(
    mut reader: impl Read,
    mut filters: FilterChain,
    mut dump: Option<File>,
    tx: Sender<ReadEvent>,
    stop: Receiver<()>,
    recycled: Receiver<Vec<u8>>,
) -> Result<()> {
    let mut buffer = Vec::new();
    loop {
        if stop.try_recv() != Err(TryRecvError::Empty) {
            return Ok(());
        }
        if buffer.is_empty() {
            buffer = recycled.try_recv().unwrap_or_default();
        }
        buffer.resize(CHUNK_SIZE, 0);
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(file) = &mut dump {
                    let _ = file.write_all(&buffer[..n]);
                    let _ = file.flush();
                }
                let bytes = match filters.filter(&buffer[..n]) {
                    Cow::Borrowed(bytes) if std::ptr::eq(bytes, &buffer[..n]) => {
                        buffer.truncate(n);
                        std::mem::take(&mut buffer)
                    }
                    bytes => bytes.into_owned(),
                };
                if !send_output(&tx, &stop, bytes) {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("read PTY output"),
        }
    }
    if send_output(&tx, &stop, filters.finish()) {
        send_event(&tx, &stop, ReadEvent::Eof);
    }
    Ok(())
}

fn read_input(mut reader: impl Read, tx: Sender<ReadEvent>, stop: Receiver<()>) -> Result<()> {
    let mut buffer = [0u8; 4096];
    loop {
        if stop.try_recv() != Err(TryRecvError::Empty) {
            return Ok(());
        }
        match reader.read(&mut buffer) {
            Ok(0) => {
                send_event(&tx, &stop, ReadEvent::Eof);
                return Ok(());
            }
            Ok(n) => {
                if !send_event(&tx, &stop, ReadEvent::Data(buffer[..n].to_vec())) {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("read terminal input"),
        }
    }
}

fn write_terminal(
    writer: impl Write,
    track_sequences: bool,
    rx: Receiver<TerminalCommand>,
    recycle: Sender<Vec<u8>>,
    mut report_failure: impl FnMut(anyhow::Error),
) -> Result<()> {
    let mut terminal = TerminalOutput::new(writer, track_sequences);
    let mut failed = false;
    for command in rx {
        let result = match command {
            TerminalCommand::Finish(bytes) => {
                return terminal.finish(&bytes).context("restore terminal modes");
            }
            _ if failed => continue,
            TerminalCommand::Output { bytes, extra } => {
                let result = terminal
                    .write_child(&bytes, &extra)
                    .context("write terminal output");
                let _ = recycle.try_send(bytes);
                result
            }
            TerminalCommand::Controls(bytes) => terminal
                .write_extra(&bytes)
                .context("write terminal controls"),
        };
        if let Err(error) = result {
            failed = true;
            // Abort the child promptly, but keep ownership until the coordinator
            // supplies cleanup.  Even a failed stream gets a restoration attempt.
            report_failure(error);
        }
    }
    Err(anyhow::anyhow!("terminal writer closed without cleanup"))
}

fn write_input(mut writer: impl Write, rx: Receiver<InputCommand>) -> Result<()> {
    for command in rx {
        let InputCommand::Data(bytes) = command else {
            return Ok(());
        };
        if let Err(error) = writer.write_all(&bytes).and_then(|_| writer.flush()) {
            if error.kind() == io::ErrorKind::BrokenPipe || error.raw_os_error() == Some(libc::EIO)
            {
                return Ok(());
            }
            return Err(error).context("write child input");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tfil::filters::InkFakeCursorFilter;

    const TIMEOUT: Duration = Duration::from_secs(5);

    struct Harness {
        output: Sender<ReadEvent>,
        input: Sender<ReadEvent>,
        terminal: Receiver<TerminalCommand>,
        child: Receiver<InputCommand>,
        completed: Sender<Completion>,
        result: Receiver<Result<()>>,
        aborted: Receiver<()>,
        stopped: Receiver<()>,
    }

    impl Harness {
        fn start(coordinator: Coordinator) -> Self {
            let (output, output_rx) = bounded(1);
            let (input, input_rx) = bounded(1);
            // Rendezvous writers make backpressure deterministic.
            let (terminal_tx, terminal) = bounded(0);
            let (child_tx, child) = bounded(0);
            let (completed, completed_rx) = bounded(5);
            let (stop, stopped) = bounded(0);
            let (resize_tx, resize) = bounded(1);
            drop(resize_tx);
            let (result_tx, result) = bounded(1);
            let (abort_tx, aborted) = bounded(1);
            thread::spawn(move || {
                let ports = Ports {
                    output: output_rx,
                    input: input_rx,
                    terminal: terminal_tx,
                    child: child_tx,
                    completed: completed_rx,
                    resize,
                };
                let result = coordinator.run(
                    ports,
                    stop,
                    || unreachable!(),
                    || {
                        let _ = abort_tx.try_send(());
                    },
                );
                result_tx.send(result).unwrap();
            });
            Self {
                output,
                input,
                terminal,
                child,
                completed,
                result,
                aborted,
                stopped,
            }
        }

        fn complete(&self, worker: Worker, result: Result<()>) {
            self.completed.send(Completion { worker, result }).unwrap();
        }

        fn finish(&self) {
            self.output.send_timeout(ReadEvent::Eof, TIMEOUT).unwrap();
            assert!(matches!(
                self.terminal.recv_timeout(TIMEOUT).unwrap(),
                TerminalCommand::Finish(_)
            ));
            // Finish must be acknowledged before session completion.
            assert!(self.result.try_recv().is_err());
            self.complete(Worker::TerminalWriter, Ok(()));
            self.result.recv_timeout(TIMEOUT).unwrap().unwrap();
        }
    }

    #[test]
    fn blocked_child_writer_does_not_stop_output_or_cleanup() {
        let mut coordinator = Coordinator::new(None, true, false);
        coordinator.on_input(ReadEvent::Data(b"blocked input".to_vec()));
        let h = Harness::start(coordinator);
        h.output.send(ReadEvent::Data(b"output".to_vec())).unwrap();
        assert!(
            matches!(h.terminal.recv_timeout(TIMEOUT).unwrap(), TerminalCommand::Output { bytes, .. } if bytes == b"output")
        );
        // Keep the child writer blocked throughout shutdown.
        h.finish();
        assert!(h.stopped.recv_timeout(TIMEOUT).is_err());
    }

    #[test]
    fn blocked_terminal_writer_does_not_stop_keyboard_input() {
        let mut coordinator = Coordinator::new(None, false, false);
        coordinator.on_output(ReadEvent::Data(b"blocked output".to_vec()));
        let h = Harness::start(coordinator);
        h.input.send(ReadEvent::Data(b"input".to_vec())).unwrap();
        assert!(
            matches!(h.child.recv_timeout(TIMEOUT).unwrap(), InputCommand::Data(bytes) if bytes == b"input")
        );
        assert!(matches!(
            h.terminal.recv_timeout(TIMEOUT).unwrap(),
            TerminalCommand::Output { .. }
        ));
        h.finish();
    }

    #[test]
    fn writer_failure_is_observed_while_both_writers_are_blocked() {
        let mut coordinator = Coordinator::new(None, false, false);
        coordinator.on_input(ReadEvent::Data(vec![1]));
        coordinator.on_output(ReadEvent::Data(vec![2]));
        let h = Harness::start(coordinator);
        h.complete(
            Worker::TerminalWriter,
            Err(anyhow::anyhow!("failed writer")),
        );
        assert_eq!(
            h.result
                .recv_timeout(TIMEOUT)
                .unwrap()
                .unwrap_err()
                .to_string(),
            "failed writer"
        );
        h.aborted.recv_timeout(TIMEOUT).unwrap();
    }

    #[test]
    fn reader_failure_keeps_pending_output_before_cleanup() {
        let mut coordinator = Coordinator::new(None, true, false);
        coordinator.on_output(ReadEvent::Data(b"\x1b[?20".to_vec()));
        let h = Harness::start(coordinator);
        h.output.send(ReadEvent::Data(b"0".to_vec())).unwrap();
        h.complete(Worker::OutputReader, Err(anyhow::anyhow!("failed reader")));
        h.aborted.recv_timeout(TIMEOUT).unwrap();
        let mut bytes = Vec::new();
        let mut terminal = TerminalOutput::new(&mut bytes, true);
        match h.terminal.recv_timeout(TIMEOUT).unwrap() {
            TerminalCommand::Output { bytes, extra } => {
                terminal.write_child(&bytes, &extra).unwrap()
            }
            _ => panic!("pending output lost"),
        }
        match h.terminal.recv_timeout(TIMEOUT).unwrap() {
            TerminalCommand::Output { bytes, extra } => {
                terminal.write_child(&bytes, &extra).unwrap()
            }
            _ => panic!("queued output lost"),
        }
        h.output.send(ReadEvent::Eof).unwrap();
        match h.terminal.recv_timeout(TIMEOUT).unwrap() {
            TerminalCommand::Finish(bytes) => terminal.finish(&bytes).unwrap(),
            _ => panic!("missing cleanup"),
        }
        h.complete(Worker::TerminalWriter, Ok(()));
        assert_eq!(
            h.result
                .recv_timeout(TIMEOUT)
                .unwrap()
                .unwrap_err()
                .to_string(),
            "failed reader"
        );
        assert_eq!(bytes, b"\x1b[?200\x18\x1b\\\x18\x1b[?25h");
    }

    #[test]
    fn input_eof_flushes_partial_report_before_closing_writer() {
        let mut mouse = CodexMouseUi::new(24, 80);
        mouse.on_output(b"\x1b[?2004h");
        let h = Harness::start(Coordinator::new(Some(mouse), false, false));
        h.input.send(ReadEvent::Data(b"a\x1b[<".to_vec())).unwrap();
        assert!(
            matches!(h.child.recv_timeout(TIMEOUT).unwrap(), InputCommand::Data(bytes) if bytes == b"a")
        );
        h.input.send(ReadEvent::Eof).unwrap();
        assert!(
            matches!(h.child.recv_timeout(TIMEOUT).unwrap(), InputCommand::Data(bytes) if bytes == b"\x1b[<")
        );
        assert!(matches!(
            h.child.recv_timeout(TIMEOUT).unwrap(),
            InputCommand::Eof
        ));
        h.finish();
    }

    #[test]
    fn cancellation_releases_a_full_reader_queue() {
        let (tx, rx) = bounded(1);
        tx.send(ReadEvent::Data(vec![1])).unwrap();
        let (stop, cancel) = bounded(0);
        let (done_tx, done) = bounded(1);
        thread::spawn(move || {
            done_tx
                .send(send_event(&tx, &cancel, ReadEvent::Eof))
                .unwrap();
        });
        drop(stop);
        assert!(!done.recv_timeout(TIMEOUT).unwrap());
        assert_eq!(rx.len(), 1);
    }

    #[test]
    fn filter_eof_updates_ui_before_finish() {
        let (tx, rx) = bounded(8);
        let (_stop, cancel) = bounded(0);
        let (_recycle, recycled) = bounded(1);
        let input = b"\x1b[7m \x1b[?2004h";
        read_output(
            input.as_slice(),
            FilterChain::new(vec![Box::new(InkFakeCursorFilter::new())]),
            None,
            tx,
            cancel,
            recycled,
        )
        .unwrap();
        let mut coordinator = Coordinator::new(Some(CodexMouseUi::new(24, 80)), false, false);
        let mut output = Vec::new();
        for event in rx {
            coordinator.on_output(event);
            if let Some(TerminalCommand::Output { bytes, .. }) = coordinator.pending_terminal.take()
            {
                output.extend(bytes);
            }
        }
        assert_eq!(output, input);
        assert!(coordinator.output_eof);
        assert!(coordinator.cleanup().starts_with(MOUSE_DISABLE));
    }

    #[test]
    fn terminal_commands_preserve_sequence_boundaries_and_finish_order() {
        let (tx, rx) = bounded(8);
        tx.send(TerminalCommand::Output {
            bytes: b"\x1b[3".to_vec(),
            extra: vec![],
        })
        .unwrap();
        tx.send(TerminalCommand::Controls(b"\x1b[?25l".to_vec()))
            .unwrap();
        tx.send(TerminalCommand::Output {
            bytes: b"1mX".to_vec(),
            extra: vec![],
        })
        .unwrap();
        tx.send(TerminalCommand::Finish(CURSOR_SHOW.to_vec()))
            .unwrap();
        tx.send(TerminalCommand::Controls(b"must not appear".to_vec()))
            .unwrap();
        let (recycle, _) = bounded(1);
        let mut bytes = Vec::new();
        write_terminal(&mut bytes, true, rx, recycle, |_| unreachable!()).unwrap();
        assert_eq!(bytes, b"\x1b[31mX\x1b[?25l\x1b[?25h");
    }

    struct FailedIo;
    impl Read for FailedIo {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("read failed"))
        }
    }
    impl Write for FailedIo {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("write failed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn failed_terminal_write_still_attempts_cleanup() {
        struct FailOnce {
            failed: bool,
            bytes: Vec<u8>,
        }
        impl Write for FailOnce {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if !self.failed {
                    self.failed = true;
                    return Err(io::Error::other("transient failure"));
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (tx, rx) = bounded(3);
        tx.send(TerminalCommand::Output {
            bytes: b"\x1b[?20".to_vec(),
            extra: vec![],
        })
        .unwrap();
        tx.send(TerminalCommand::Controls(b"must not appear".to_vec()))
            .unwrap();
        tx.send(TerminalCommand::Finish(CURSOR_SHOW.to_vec()))
            .unwrap();
        let (recycle, _) = bounded(1);
        let mut writer = FailOnce {
            failed: false,
            bytes: Vec::new(),
        };
        let mut errors = Vec::new();
        write_terminal(&mut writer, true, rx, recycle, |error| {
            errors.push(error.to_string())
        })
        .unwrap();
        assert_eq!(errors, ["write terminal output"]);
        assert_eq!(writer.bytes, b"\x18\x1b\\\x18\x1b[?25h");
    }

    #[test]
    fn worker_errors_and_panics_are_reported() {
        let (tx, rx) = bounded(1);
        let (_stop, cancel) = bounded(0);
        let (_recycle, recycled) = bounded(1);
        assert_eq!(
            read_output(FailedIo, FilterChain::default(), None, tx, cancel, recycled)
                .unwrap_err()
                .to_string(),
            "read PTY output"
        );
        drop(rx);
        let (tx, rx) = bounded(2);
        tx.send(TerminalCommand::Output {
            bytes: vec![1],
            extra: vec![],
        })
        .unwrap();
        tx.send(TerminalCommand::Finish(CURSOR_SHOW.to_vec()))
            .unwrap();
        let (recycle, _) = bounded(1);
        let mut failures = Vec::new();
        assert_eq!(
            write_terminal(FailedIo, true, rx, recycle, |error| failures
                .push(error.to_string()))
            .unwrap_err()
            .to_string(),
            "restore terminal modes"
        );
        assert_eq!(failures, ["write terminal output"]);
        let (tx, rx) = bounded(1);
        tx.send(InputCommand::Data(vec![1])).unwrap();
        assert_eq!(
            write_input(FailedIo, rx).unwrap_err().to_string(),
            "write child input"
        );
        let (tx, rx) = bounded(1);
        let worker = spawn_worker(Worker::OutputReader, tx, || panic!("test panic"));
        let notice = rx.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(notice.worker, Worker::OutputReader);
        assert!(notice.result.unwrap_err().to_string().contains("panicked"));
        worker.join().unwrap();
    }
}
