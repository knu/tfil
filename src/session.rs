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
const MAX_OUTPUT_BATCH: usize = 8;
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

impl TerminalCommand {
    fn byte_len(&self) -> usize {
        match self {
            Self::Output { bytes, extra } => bytes.len() + extra.len(),
            Self::Controls(bytes) | Self::Finish(bytes) => bytes.len(),
        }
    }
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

    // Without a UI model, data needs no coordination.  EOF still goes through
    // the coordinator, which appends closing commands to the same writer queues.
    let direct_output = mouse.is_none().then(|| terminal_tx.clone());
    let cancel = stop_rx.clone();
    spawn_worker(Worker::OutputReader, completed_tx.clone(), move || {
        read_output(
            reader,
            filters,
            options.dump,
            output_tx,
            cancel,
            recycle_rx,
            direct_output,
        )
    });
    let direct_input = mouse.is_none().then(|| child_tx.clone());
    let cancel = stop_rx.clone();
    spawn_worker(Worker::InputReader, completed_tx.clone(), move || {
        read_input(io::stdin(), input_tx, cancel, direct_input)
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputState {
    Reading,
    Draining,
    FinishQueued,
}

impl OutputState {
    fn close(&mut self) {
        if *self == Self::Reading {
            *self = Self::Draining;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputState {
    Reading,
    Draining,
    CloseQueued,
    Closed,
}

impl InputState {
    fn close(&mut self) {
        if *self == Self::Reading {
            *self = Self::Draining;
        }
    }
}

#[derive(Default)]
struct Workers {
    disconnected: u8,
    completed: u8,
    terminal_result: Option<Result<()>>,
}

impl Workers {
    fn disconnected(&self, worker: Worker) -> bool {
        self.disconnected & (1 << worker as u8) != 0
    }

    fn completed(&self, worker: Worker) -> bool {
        self.completed & (1 << worker as u8) != 0
    }

    fn disconnect(&mut self, worker: Worker) {
        self.disconnected |= 1 << worker as u8;
    }

    fn complete(&mut self, worker: Worker) {
        self.completed |= 1 << worker as u8;
    }

    fn take_result(&mut self) -> Option<Result<()>> {
        if self.disconnected & !self.completed == 0 {
            self.terminal_result.take()
        } else {
            None
        }
    }
}

struct Coordinator {
    mouse: Option<CodexMouseUi>,
    restore_cursor: bool,
    tmux_pointer: bool,
    pending_terminal: Option<TerminalCommand>,
    pending_controls: Option<Vec<u8>>,
    pending_child: Option<InputCommand>,
    output: OutputState,
    input: InputState,
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
            output: OutputState::Reading,
            input: InputState::Reading,
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
            ReadEvent::Eof => self.output.close(),
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
                self.input.close();
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

    fn prepare_commands(&mut self, stop: &mut Option<Sender<()>>) {
        if self.pending_terminal.is_none() {
            self.pending_terminal = self.pending_controls.take().map(TerminalCommand::Controls);
        }
        if self.output != OutputState::Reading {
            stop.take();
            self.input.close();
            if self.pending_terminal.is_none() && self.output == OutputState::Draining {
                self.pending_terminal = Some(TerminalCommand::Finish(self.cleanup()));
                self.output = OutputState::FinishQueued;
            }
        }
        if self.input == InputState::Draining && self.pending_child.is_none() {
            self.pending_child = Some(InputCommand::Eof);
            self.input = InputState::CloseQueued;
        }
    }

    fn record_error(&mut self, error: anyhow::Error, abort: &mut impl FnMut()) {
        if self.error.is_none() {
            self.error = Some(error);
            abort();
        }
    }

    fn on_completion(
        &mut self,
        Completion { worker, result }: Completion,
        workers: &mut Workers,
        abort: &mut impl FnMut(),
    ) -> Result<()> {
        workers.complete(worker);
        if worker == Worker::TerminalWriter {
            if result.is_err() {
                abort();
            }
            workers.terminal_result = Some(result);
            return Ok(());
        }
        if worker == Worker::ChildWriter {
            self.input = InputState::Closed;
            self.pending_child = None;
        }
        if worker == Worker::OutputReader {
            if workers.disconnected(worker) {
                self.output.close();
            }
            if let Err(error) = result {
                self.record_error(error, abort);
            }
            // Its sender is gone, but previously queued bytes still precede
            // cleanup.  Drain them before observing disconnect.
            return Ok(());
        }
        result
    }

    fn run(
        mut self,
        ports: Ports,
        stop: Sender<()>,
        mut resize: impl FnMut() -> portable_pty::PtySize,
        mut abort: impl FnMut(),
    ) -> Result<()> {
        let mut stop = Some(stop);
        let mut resize_closed = false;
        let mut workers = Workers::default();
        loop {
            if let Some(result) = workers.take_result() {
                return self.error.map_or(result, Err);
            }
            self.prepare_commands(&mut stop);

            let mut select = Select::new();
            let completed = select.recv(&ports.completed);
            let output = (self.output == OutputState::Reading
                && !workers.disconnected(Worker::OutputReader)
                && self.pending_terminal.is_none())
            .then(|| select.recv(&ports.output));
            let input = (self.input == InputState::Reading
                && self.pending_controls.is_none()
                && self.pending_child.is_none())
            .then(|| select.recv(&ports.input));
            let terminal = (self.pending_terminal.is_some()
                && !workers.disconnected(Worker::TerminalWriter))
            .then(|| select.send(&ports.terminal));
            let child = self
                .pending_child
                .is_some()
                .then(|| select.send(&ports.child));
            let resized = (!resize_closed && self.output == OutputState::Reading)
                .then(|| select.recv(&ports.resize));
            let operation = select.select();
            let index = operation.index();
            let outcome = if index == completed {
                let notice = operation
                    .recv(&ports.completed)
                    .context("worker notifications disconnected")?;
                self.on_completion(notice, &mut workers, &mut abort)
            } else if Some(index) == output {
                match operation.recv(&ports.output) {
                    Ok(event) => self.on_output(event),
                    Err(_) => {
                        workers.disconnect(Worker::OutputReader);
                        if workers.completed(Worker::OutputReader) {
                            self.output.close();
                        }
                    }
                }
                Ok(())
            } else if Some(index) == input {
                match operation.recv(&ports.input) {
                    Ok(event) => self.on_input(event),
                    Err(_) => {
                        workers.disconnect(Worker::InputReader);
                        self.input.close();
                    }
                }
                Ok(())
            } else if Some(index) == terminal {
                if operation
                    .send(&ports.terminal, self.pending_terminal.take().unwrap())
                    .is_err()
                {
                    workers.disconnect(Worker::TerminalWriter);
                }
                Ok(())
            } else if Some(index) == child {
                if operation
                    .send(&ports.child, self.pending_child.take().unwrap())
                    .is_err()
                {
                    workers.disconnect(Worker::ChildWriter);
                    self.input = InputState::Closed;
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
                self.record_error(error, &mut abort);
                self.output.close();
            }
        }
    }
}

fn send_event<T>(tx: &Sender<T>, stop: &Receiver<()>, event: T) -> bool {
    select! {
        send(tx, event) -> result => result.is_ok(),
        recv(stop) -> _ => false,
    }
}

fn send_output(
    tx: &Sender<ReadEvent>,
    stop: &Receiver<()>,
    bytes: Vec<u8>,
    direct: Option<&Sender<TerminalCommand>>,
) -> bool {
    let send = |bytes| {
        if let Some(tx) = direct {
            let command = TerminalCommand::Output {
                bytes,
                extra: Vec::new(),
            };
            send_event(tx, stop, command)
        } else {
            send_event(tx, stop, ReadEvent::Data(bytes))
        }
    };
    if bytes.len() <= CHUNK_SIZE {
        bytes.is_empty() || send(bytes)
    } else {
        bytes.chunks(CHUNK_SIZE).all(|chunk| send(chunk.to_vec()))
    }
}

fn read_output(
    mut reader: impl Read,
    mut filters: FilterChain,
    mut dump: Option<File>,
    tx: Sender<ReadEvent>,
    stop: Receiver<()>,
    recycled: Receiver<Vec<u8>>,
    direct: Option<Sender<TerminalCommand>>,
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
                if !send_output(&tx, &stop, bytes, direct.as_ref()) {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("read PTY output"),
        }
    }
    if send_output(&tx, &stop, filters.finish(), direct.as_ref()) {
        send_event(&tx, &stop, ReadEvent::Eof);
    }
    Ok(())
}

fn read_input(
    mut reader: impl Read,
    tx: Sender<ReadEvent>,
    stop: Receiver<()>,
    direct: Option<Sender<InputCommand>>,
) -> Result<()> {
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
                let bytes = buffer[..n].to_vec();
                let sent = if let Some(direct) = &direct {
                    send_event(direct, &stop, InputCommand::Data(bytes))
                } else {
                    send_event(&tx, &stop, ReadEvent::Data(bytes))
                };
                if !sent {
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
    let mut terminal = TerminalOutput::new(OutputBuffer::new(writer), track_sequences);
    let mut failed = false;
    let mut next = None;
    while let Some(mut command) = next.take().or_else(|| rx.recv().ok()) {
        let batch_limit = if rx.is_empty() { 1 } else { MAX_OUTPUT_BATCH };
        if !failed {
            // Avoid copying or another queue receive for an isolated update.
            terminal.writer_mut().buffered = batch_limit > 1;
        }
        let mut batch_bytes = 0;
        for count in 1..=batch_limit {
            let result = match command {
                TerminalCommand::Finish(bytes) => {
                    if !failed && let Err(error) = terminal.flush() {
                        terminal.mark_write_failed();
                        report_failure(anyhow::Error::new(error).context("write terminal output"));
                    }
                    return terminal.finish(&bytes).context("restore terminal modes");
                }
                _ if failed => break,
                TerminalCommand::Output { bytes, extra } => {
                    batch_bytes += bytes.len() + extra.len();
                    let result = terminal
                        .write_child(&bytes, &extra)
                        .context("write terminal output");
                    let _ = recycle.try_send(bytes);
                    result
                }
                TerminalCommand::Controls(bytes) => {
                    batch_bytes += bytes.len();
                    terminal
                        .write_extra(&bytes)
                        .context("write terminal controls")
                }
            };
            if let Err(error) = result {
                failed = true;
                terminal.mark_write_failed();
                report_failure(error);
                break;
            }
            if count == batch_limit || batch_bytes >= CHUNK_SIZE {
                break;
            }
            next = rx.try_recv().ok();
            if next
                .as_ref()
                .is_none_or(|command| command.byte_len() > CHUNK_SIZE - batch_bytes)
            {
                break;
            }
            command = next.take().unwrap();
        }
        if !failed && let Err(error) = terminal.flush() {
            failed = true;
            terminal.mark_write_failed();
            // Abort promptly, then drain commands until cleanup arrives.
            report_failure(anyhow::Error::new(error).context("write terminal output"));
        }
    }
    Err(anyhow::anyhow!("terminal writer closed without cleanup"))
}

/// A failed flush discards the unwritten suffix, so cleanup cannot replay it.
/// Unlike BufWriter, dropping this buffer never retries a failed write.
struct OutputBuffer<W> {
    writer: W,
    bytes: Vec<u8>,
    buffered: bool,
}

impl<W: Write> OutputBuffer<W> {
    fn new(writer: W) -> Self {
        Self {
            writer,
            bytes: Vec::new(),
            buffered: false,
        }
    }
}

impl<W: Write> Write for OutputBuffer<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.buffered {
            return self.writer.write(bytes);
        }
        if bytes.len() > CHUNK_SIZE - self.bytes.len() {
            self.flush()?;
        }
        if bytes.len() >= CHUNK_SIZE {
            return self.writer.write(bytes);
        }
        if self.bytes.capacity() == 0 {
            self.bytes.reserve_exact(CHUNK_SIZE);
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let result = self.writer.write_all(&self.bytes);
        self.bytes.clear();
        result?;
        self.writer.flush()
    }
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
    fn disconnect_never_masks_reader_error() {
        for notice_first in [false, true] {
            for worker in [Worker::OutputReader, Worker::InputReader] {
                let mut h = Harness::start(Coordinator::new(None, true, false));
                if notice_first {
                    h.complete(worker, Err(anyhow::anyhow!("original read failure")));
                }
                let (replacement, _) = bounded(1);
                drop(std::mem::replace(
                    if worker == Worker::OutputReader {
                        &mut h.output
                    } else {
                        &mut h.input
                    },
                    replacement,
                ));
                if !notice_first {
                    h.complete(worker, Err(anyhow::anyhow!("original read failure")));
                }
                assert!(matches!(
                    h.terminal.recv_timeout(TIMEOUT).unwrap(),
                    TerminalCommand::Finish(_)
                ));
                h.complete(Worker::TerminalWriter, Ok(()));
                assert_eq!(
                    h.result
                        .recv_timeout(TIMEOUT)
                        .unwrap()
                        .unwrap_err()
                        .to_string(),
                    "original read failure"
                );
            }
        }
    }

    #[test]
    fn disconnected_terminal_preserves_worker_panic() {
        let mut coordinator = Coordinator::new(None, false, false);
        coordinator.on_output(ReadEvent::Data(vec![1]));
        let mut h = Harness::start(coordinator);
        let (_, replacement) = bounded(1);
        drop(std::mem::replace(&mut h.terminal, replacement));
        h.complete(
            Worker::TerminalWriter,
            Err(anyhow::anyhow!("TerminalWriter panicked")),
        );
        assert_eq!(
            h.result
                .recv_timeout(TIMEOUT)
                .unwrap()
                .unwrap_err()
                .to_string(),
            "TerminalWriter panicked"
        );
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
    fn direct_output_flushes_filter_tail_before_eof_and_cleanup() {
        let (events, rx) = bounded(1);
        let (_stop, cancel) = bounded(0);
        let (commands, output) = bounded(8);
        let (recycle, recycled) = bounded(1);
        let input = b"text\x1b[7m ";
        read_output(
            input.as_slice(),
            FilterChain::new(vec![Box::new(InkFakeCursorFilter::new())]),
            None,
            events,
            cancel,
            recycled,
            Some(commands.clone()),
        )
        .unwrap();
        assert!(matches!(rx.recv().unwrap(), ReadEvent::Eof));
        commands
            .send(TerminalCommand::Finish(CURSOR_SHOW.to_vec()))
            .unwrap();
        let mut bytes = Vec::new();
        write_terminal(&mut bytes, true, output, recycle, |_| unreachable!()).unwrap();
        assert_eq!(bytes, [input.as_slice(), CURSOR_SHOW].concat());
    }

    #[test]
    fn direct_input_precedes_coordinated_eof() {
        let (events, rx) = bounded(1);
        let (_stop, cancel) = bounded(0);
        let (commands, input) = bounded(8);
        read_input(
            b"keyboard input".as_slice(),
            events,
            cancel,
            Some(commands.clone()),
        )
        .unwrap();
        assert!(matches!(rx.recv().unwrap(), ReadEvent::Eof));
        commands.send(InputCommand::Eof).unwrap();
        let mut bytes = Vec::new();
        write_input(&mut bytes, input).unwrap();
        assert_eq!(bytes, b"keyboard input");
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
            None,
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
        assert!(coordinator.output != OutputState::Reading);
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

    #[derive(Default)]
    struct RecordingWriter(Vec<Vec<u8>>);

    impl Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !bytes.is_empty() {
                self.0.push(bytes.to_vec());
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn queued_output_is_batched_with_a_message_limit() {
        let (tx, rx) = bounded(32);
        for _ in 0..17 {
            tx.send(TerminalCommand::Output {
                bytes: vec![b'x'],
                extra: vec![],
            })
            .unwrap();
        }
        tx.send(TerminalCommand::Finish(CURSOR_SHOW.to_vec()))
            .unwrap();
        let (recycle, _) = bounded(1);
        let mut writer = RecordingWriter::default();
        write_terminal(&mut writer, true, rx, recycle, |_| unreachable!()).unwrap();
        assert_eq!(
            writer.0,
            [
                vec![b'x'; 8],
                vec![b'x'; 8],
                vec![b'x'],
                CURSOR_SHOW.to_vec()
            ]
        );
    }

    #[test]
    fn queued_output_is_flushed_at_the_byte_limit() {
        let (tx, rx) = bounded(16);
        for byte in 0..8 {
            tx.send(TerminalCommand::Output {
                bytes: vec![byte; 20_000],
                extra: vec![],
            })
            .unwrap();
        }
        tx.send(TerminalCommand::Finish(CURSOR_SHOW.to_vec()))
            .unwrap();
        let (recycle, _) = bounded(1);
        let mut writer = RecordingWriter::default();
        write_terminal(&mut writer, true, rx, recycle, |_| unreachable!()).unwrap();
        assert_eq!(
            writer.0.iter().map(Vec::len).collect::<Vec<_>>(),
            [60_000, 60_000, 40_000, CURSOR_SHOW.len()]
        );
        let expected: Vec<_> = (0..8)
            .flat_map(|byte| vec![byte; 20_000])
            .chain(CURSOR_SHOW.iter().copied())
            .collect();
        assert_eq!(writer.0.concat(), expected);
    }

    #[test]
    fn isolated_output_is_flushed_before_waiting_for_another_command() {
        struct FlushNotice(Sender<Vec<u8>>, Vec<u8>);
        impl Write for FlushNotice {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.1.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0.send(std::mem::take(&mut self.1)).unwrap();
                Ok(())
            }
        }
        let (tx, rx) = bounded(8);
        let (flushed, notices) = bounded(8);
        let (recycle, _) = bounded(1);
        let worker = thread::spawn(move || {
            write_terminal(
                FlushNotice(flushed, vec![]),
                true,
                rx,
                recycle,
                |_| unreachable!(),
            )
        });
        tx.send(TerminalCommand::Output {
            bytes: vec![b'x'],
            extra: vec![],
        })
        .unwrap();
        assert_eq!(notices.recv_timeout(TIMEOUT).unwrap(), b"x");
        tx.send(TerminalCommand::Finish(vec![])).unwrap();
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn output_buffer_never_exceeds_one_chunk_or_flushes_on_drop() {
        let mut writer = RecordingWriter::default();
        {
            let mut output = OutputBuffer::new(&mut writer);
            output.buffered = true;
            let chunk = vec![b'x'; CHUNK_SIZE / 2 + 1];
            output.write_all(&chunk).unwrap();
            output.write_all(&chunk).unwrap();
            assert_eq!(output.bytes.capacity(), CHUNK_SIZE);
            // The second half remains buffered and must not be retried on drop.
        }
        assert_eq!(writer.0, [vec![b'x'; CHUNK_SIZE / 2 + 1]]);
    }

    #[test]
    fn partial_batch_failure_discards_suffix_and_recovers_unknown_boundary() {
        struct FailAfterEscape {
            calls: usize,
            bytes: Vec<u8>,
        }
        impl Write for FailAfterEscape {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.calls += 1;
                match self.calls {
                    1 => {
                        self.bytes.push(bytes[0]);
                        Ok(1)
                    }
                    2 => Err(io::Error::other("partial write failed")),
                    _ => {
                        self.bytes.extend_from_slice(bytes);
                        Ok(bytes.len())
                    }
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (tx, rx) = bounded(8);
        tx.send(TerminalCommand::Output {
            bytes: b"\x1b[31mtext".to_vec(),
            extra: vec![],
        })
        .unwrap();
        tx.send(TerminalCommand::Controls(b"\x1b[?25l".to_vec()))
            .unwrap();
        tx.send(TerminalCommand::Finish(CURSOR_SHOW.to_vec()))
            .unwrap();
        let (recycle, _) = bounded(1);
        let mut writer = FailAfterEscape {
            calls: 0,
            bytes: vec![],
        };
        let mut errors = Vec::new();
        write_terminal(&mut writer, true, rx, recycle, |error| {
            errors.push(error.to_string())
        })
        .unwrap();
        assert_eq!(errors, ["write terminal output"]);
        assert_eq!(writer.bytes, b"\x1b\x18\x1b\\\x18\x1b[?25h");
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
            read_output(
                FailedIo,
                FilterChain::default(),
                None,
                tx,
                cancel,
                recycled,
                None
            )
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
