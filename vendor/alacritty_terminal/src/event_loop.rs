// This file has local integration changes; see vendor/CHANGES.md.
//! The main event loop which performs I/O on the pseudoterminal.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt::{self, Display, Formatter};
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::num::NonZeroUsize;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use log::error;
use polling::{Event as PollingEvent, Events, PollMode, Poller};

use crate::event::{self, Event, EventListener, WindowSize};
use crate::sync::FairMutex;
use crate::term::Term;
use crate::tty;
use vte::ansi;

/// Max bytes to read from the PTY before forced terminal synchronization.
pub(crate) const READ_BUFFER_SIZE: usize = 0x10_0000;

/// Max bytes to read from the PTY while the terminal is locked.
const MAX_LOCKED_READ: usize = u16::MAX as usize;
const INPUT_QUEUE_LIMIT: usize = 1024 * 1024;

#[derive(Default)]
struct QueueState {
    input: usize,
    resize: Option<WindowSize>,
    shutdown: bool,
    closed: bool,
}

/// Messages that may be sent to the `EventLoop`.
#[derive(Debug)]
pub enum Msg {
    /// Data that should be written to the PTY.
    Input(Cow<'static, [u8]>),

    /// Indicates that the `EventLoop` should shut down, as Alacritty is shutting down.
    Shutdown,

    /// Instruction to resize the PTY.
    Resize(WindowSize),
}

/// The main event loop.
///
/// Handles all the PTY I/O and runs the PTY parser which updates terminal
/// state.
pub struct EventLoop<T: tty::EventedPty, U: EventListener> {
    poll: Arc<Poller>,
    pty: T,
    rx: PeekableReceiver<Msg>,
    tx: Sender<Msg>,
    terminal: Arc<FairMutex<Term<U>>>,
    event_proxy: U,
    drain_on_exit: bool,
    ref_test: bool,
    queue: Arc<Mutex<QueueState>>,
}

impl<T, U> EventLoop<T, U>
where
    T: tty::EventedPty + event::OnResize + Send + 'static,
    U: EventListener + Send + 'static,
{
    /// Create a new event loop.
    pub fn new(
        terminal: Arc<FairMutex<Term<U>>>,
        event_proxy: U,
        pty: T,
        drain_on_exit: bool,
        ref_test: bool,
    ) -> io::Result<EventLoop<T, U>> {
        let (tx, rx) = mpsc::channel();
        let poll = Poller::new()?.into();
        Ok(EventLoop {
            poll,
            pty,
            tx,
            rx: PeekableReceiver::new(rx),
            terminal,
            event_proxy,
            drain_on_exit,
            ref_test,
            queue: Arc::new(Mutex::new(QueueState::default())),
        })
    }

    pub fn channel(&self) -> EventLoopSender {
        EventLoopSender {
            sender: self.tx.clone(),
            poller: self.poll.clone(),
            queue: self.queue.clone(),
        }
    }

    /// Drain the channel.
    ///
    /// Returns `false` when a shutdown message was received.
    fn drain_recv_channel(&mut self, state: &mut State) -> bool {
        while let Some(msg) = self.rx.recv() {
            match msg {
                Msg::Input(input) => {
                    if state.closing {
                        let mut queue =
                            self.queue.lock().unwrap_or_else(|error| error.into_inner());
                        queue.input = queue.input.saturating_sub(input.len());
                    } else {
                        state.write_list.push_back(input);
                    }
                }
                Msg::Resize(_) => {
                    let latest = self
                        .queue
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .resize
                        .take();
                    let Some(window_size) = latest else {
                        continue;
                    };
                    if !state.closing {
                        if let Err(error) = self.pty.resize_checked(window_size) {
                            state.error = Some(error.to_string());
                            return false;
                        }
                        self.terminal.lock().resize(window_size);
                        self.event_proxy.send_event(Event::Wakeup);
                    }
                }
                Msg::Shutdown => return false,
            }
        }

        true
    }

    #[inline]
    fn pty_read<X>(
        &mut self,
        state: &mut State,
        buf: &mut [u8],
        mut writer: Option<&mut X>,
    ) -> io::Result<()>
    where
        X: Write,
    {
        let mut unprocessed = 0;
        let mut processed = 0;
        state.read_pending = false;

        // Reserve the next terminal lock for PTY reading.
        let _terminal_lease = Some(self.terminal.lease());
        let mut terminal = None;

        loop {
            // Read from the PTY.
            match self.pty.reader().read(&mut buf[unprocessed..]) {
                // This is received on Windows/macOS when no more data is readable from the PTY.
                Ok(0) if unprocessed == 0 => {
                    state.output_eof = true;
                    break;
                }
                Ok(got) => unprocessed += got,
                Err(err) => match err.kind() {
                    ErrorKind::Interrupted | ErrorKind::WouldBlock => {
                        // Go back to mio if we're caught up on parsing and the PTY would block.
                        if unprocessed == 0 {
                            break;
                        }
                    }
                    _ => return Err(err),
                },
            }

            // Attempt to lock the terminal.
            let terminal = match &mut terminal {
                Some(terminal) => terminal,
                None => terminal.insert(match self.terminal.try_lock_unfair() {
                    // Force block if we are at the buffer size limit.
                    None if unprocessed >= READ_BUFFER_SIZE => self.terminal.lock_unfair(),
                    None => continue,
                    Some(terminal) => terminal,
                }),
            };

            // Write a copy of the bytes to the ref test file.
            if let Some(writer) = &mut writer {
                writer.write_all(&buf[..unprocessed]).unwrap();
            }

            // Parse the incoming bytes.
            terminal.scan_progress(&buf[..unprocessed]);
            state.parser.advance(&mut **terminal, &buf[..unprocessed]);

            processed += unprocessed;
            unprocessed = 0;

            // Assure we're not blocking the terminal too long unnecessarily.
            if processed >= MAX_LOCKED_READ {
                // A buffered Windows reader removes its wake registration while data is
                // available. Yielding before WouldBlock must schedule another read;
                // otherwise remaining buffered output may never generate a new event.
                state.read_pending = true;
                break;
            }
        }

        // Queue terminal redraw unless all processed bytes were synchronized.
        if state.parser.sync_bytes_count() < processed && processed > 0 {
            self.event_proxy.send_event(Event::Wakeup);
        }

        Ok(())
    }

    #[inline]
    fn pty_write(&mut self, state: &mut State) -> io::Result<()> {
        state.ensure_next();

        'write_many: while let Some(mut current) = state.take_current() {
            'write_one: loop {
                match self.pty.writer().write(current.remaining_bytes()) {
                    Ok(0) => {
                        state.set_current(Some(current));
                        break 'write_many;
                    }
                    Ok(n) => {
                        {
                            let mut queue =
                                self.queue.lock().unwrap_or_else(|error| error.into_inner());
                            queue.input = queue.input.checked_sub(n).ok_or_else(|| {
                                io::Error::other("PTY input accounting underflow")
                            })?;
                        }
                        current.advance(n);
                        if current.finished() {
                            state.goto_next();
                            break 'write_one;
                        }
                    }
                    Err(err) => {
                        state.set_current(Some(current));
                        match err.kind() {
                            ErrorKind::Interrupted | ErrorKind::WouldBlock => break 'write_many,
                            _ => return Err(err),
                        }
                    }
                }
            }
        }

        Ok(())
    }

    pub fn spawn(self) -> JoinHandle<(Self, State)> {
        self.spawn_checked().expect("spawn PTY reader")
    }

    pub fn spawn_checked(mut self) -> io::Result<JoinHandle<(Self, State)>> {
        std::thread::Builder::new()
            .name("PTY reader".into())
            .spawn(move || {
                let mut state = State::default();
                let mut buf = [0u8; READ_BUFFER_SIZE];

                let poll_opts = PollMode::Level;
                let mut interest = PollingEvent::readable(0);

                // Register TTY through EventedRW interface.
                if let Err(err) = unsafe { self.pty.register(&self.poll, interest, poll_opts) } {
                    error!("Event loop registration error: {err}");
                    state.error = Some(err.to_string());
                    self.close_queue();
                    return (self, state);
                }

                let mut events = Events::with_capacity(NonZeroUsize::new(1024).unwrap());

                let mut pipe = if self.ref_test {
                    Some(File::create("./alacritty.recording").expect("create alacritty recording"))
                } else {
                    None
                };

                'event_loop: loop {
                    // Wakeup the event loop when a synchronized update timeout was reached.
                    let handler = state.parser.sync_timeout();
                    let mut timeout = handler
                        .sync_timeout()
                        .map(|st| st.saturating_duration_since(Instant::now()));
                    if state.read_pending {
                        timeout = Some(Duration::ZERO);
                    }
                    if let Some(deadline) = state.close_deadline {
                        if Instant::now() >= deadline {
                            state.error = Some("PTY output drain timed out".into());
                            break;
                        }
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        timeout = Some(timeout.map_or(remaining, |current| current.min(remaining)));
                    }

                    events.clear();
                    if let Err(err) = self.poll.wait(&mut events, timeout) {
                        match err.kind() {
                            ErrorKind::Interrupted => continue,
                            _ => {
                                error!("Event loop polling error: {err}");
                                state.error = Some(err.to_string());
                                break 'event_loop;
                            }
                        }
                    }

                    // Handle synchronized update timeout.
                    if events.is_empty() && self.rx.peek().is_none() && !state.read_pending {
                        state.parser.stop_sync(&mut *self.terminal.lock());
                        self.event_proxy.send_event(Event::Wakeup);
                        continue;
                    }

                    // Handle channel events, if there are any.
                    if !self.drain_recv_channel(&mut state) {
                        match self.pty.begin_shutdown() {
                            Ok(true) => {
                                state.closing = true;
                                state.close_deadline =
                                    Some(Instant::now() + Duration::from_secs(10));
                            }
                            Ok(false) => break,
                            Err(error) => {
                                state.error = Some(error.to_string());
                                break;
                            }
                        }
                    }

                    for event in events.iter() {
                        match event.key {
                            tty::PTY_CHILD_EVENT_TOKEN => {
                                if let Some(tty::ChildEvent::Exited(status)) =
                                    self.pty.next_child_event()
                                {
                                    state.exit_status = status;
                                    if let Some(status) = status {
                                        self.event_proxy.send_event(Event::ChildExit(status));
                                    }
                                    match self.pty.begin_shutdown() {
                                        Ok(true) => {
                                            state.closing = true;
                                            state.close_deadline =
                                                Some(Instant::now() + Duration::from_secs(10));
                                        }
                                        Ok(false) => (),
                                        Err(error) => {
                                            state.error = Some(error.to_string());
                                            break 'event_loop;
                                        }
                                    }
                                    if self.drain_on_exit {
                                        if let Err(error) =
                                            self.pty_read(&mut state, &mut buf, pipe.as_mut())
                                        {
                                            state.error = Some(error.to_string());
                                            break 'event_loop;
                                        }
                                    }
                                    if !state.closing {
                                        break 'event_loop;
                                    }
                                }
                            }

                            tty::PTY_READ_WRITE_TOKEN => {
                                if event.is_interrupt() {
                                    // Don't try to do I/O on a dead PTY.
                                    continue;
                                }

                                if event.readable {
                                    if let Err(err) =
                                        self.pty_read(&mut state, &mut buf, pipe.as_mut())
                                    {
                                        // On Linux, a `read` on the master side of a PTY can fail
                                        // with `EIO` if the client side hangs up.  In that case,
                                        // just loop back round for the inevitable `Exited` event.
                                        // This sucks, but checking the process is either racy or
                                        // blocking.
                                        #[cfg(target_os = "linux")]
                                        if err.raw_os_error() == Some(libc::EIO) {
                                            continue;
                                        }

                                        error!("Error reading from PTY in event loop: {err}");
                                        state.error = Some(err.to_string());
                                        break 'event_loop;
                                    }
                                }

                                if event.writable && !state.closing {
                                    if let Err(err) = self.pty_write(&mut state) {
                                        error!("Error writing to PTY in event loop: {err}");
                                        state.error = Some(err.to_string());
                                        break 'event_loop;
                                    }
                                }
                            }
                            _ => (),
                        }
                    }

                    if state.read_pending {
                        if let Err(error) = self.pty_read(&mut state, &mut buf, pipe.as_mut()) {
                            state.error = Some(error.to_string());
                            break;
                        }
                    }

                    if state.closing && state.output_eof {
                        break;
                    }

                    // Register write interest if necessary.
                    let needs_write = !state.closing && state.needs_write();
                    if needs_write != interest.writable {
                        interest.writable = needs_write;

                        // Re-register with new interest.
                        if let Err(error) = self.pty.reregister(&self.poll, interest, poll_opts) {
                            state.error = Some(error.to_string());
                            break;
                        }
                    }
                }

                // The evented instances are not dropped here so deregister them explicitly.
                let _ = self.pty.deregister(&self.poll);
                self.close_queue();
                state.parser.stop_sync(&mut *self.terminal.lock());
                self.terminal.lock().exit();
                self.event_proxy.send_event(Event::Wakeup);

                (self, state)
            })
    }

    fn close_queue(&self) {
        let mut queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        queue.closed = true;
        queue.input = 0;
        queue.resize = None;
    }
}

impl<T: tty::EventedPty, U: EventListener> Drop for EventLoop<T, U> {
    fn drop(&mut self) {
        let mut queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        queue.closed = true;
        queue.input = 0;
        queue.resize = None;
    }
}

/// Helper type which tracks how much of a buffer has been written.
struct Writing {
    source: Cow<'static, [u8]>,
    written: usize,
}

pub struct Notifier(pub EventLoopSender);

impl event::Notify for Notifier {
    fn notify<B>(&self, bytes: B)
    where
        B: Into<Cow<'static, [u8]>>,
    {
        let bytes = bytes.into();
        // Terminal hangs if we send 0 bytes through.
        if bytes.is_empty() {
            return;
        }

        let _ = self.0.send(Msg::Input(bytes));
    }
}

impl event::OnResize for Notifier {
    fn on_resize(&mut self, window_size: WindowSize) {
        let _ = self.0.send(Msg::Resize(window_size));
    }
}

#[derive(Debug)]
pub enum EventLoopSendError {
    /// Error polling the event loop.
    Io(io::Error),

    /// Error sending a message to the event loop.
    Send(mpsc::SendError<Msg>),
    /// The bounded pending-input budget would be exceeded.
    InputQueueFull,
    Closed,
}

impl Display for EventLoopSendError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            EventLoopSendError::Io(err) => err.fmt(f),
            EventLoopSendError::Send(err) => err.fmt(f),
            EventLoopSendError::InputQueueFull => f.write_str("PTY input queue is full"),
            EventLoopSendError::Closed => f.write_str("PTY input is closed"),
        }
    }
}

impl std::error::Error for EventLoopSendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EventLoopSendError::Io(err) => err.source(),
            EventLoopSendError::Send(err) => err.source(),
            EventLoopSendError::InputQueueFull | EventLoopSendError::Closed => None,
        }
    }
}

#[derive(Clone)]
pub struct EventLoopSender {
    sender: Sender<Msg>,
    poller: Arc<Poller>,
    queue: Arc<Mutex<QueueState>>,
}

impl EventLoopSender {
    pub fn send(&self, msg: Msg) -> Result<(), EventLoopSendError> {
        let input = match &msg {
            Msg::Input(bytes) => bytes.len(),
            _ => 0,
        };
        let mut queue = self.queue.lock().map_err(|_| {
            EventLoopSendError::Io(io::Error::other("PTY queue state was poisoned"))
        })?;
        if queue.closed || (queue.shutdown && !matches!(msg, Msg::Shutdown)) {
            return Err(EventLoopSendError::Closed);
        }
        if matches!(msg, Msg::Input(_)) && input == 0 {
            return Ok(());
        }
        if let Msg::Resize(size) = &msg {
            if queue.resize.replace(*size).is_some() {
                return Ok(());
            }
        }
        if matches!(msg, Msg::Shutdown) {
            if queue.shutdown {
                return Ok(());
            }
            queue.shutdown = true;
        }
        if input > 0 {
            queue.input = queue
                .input
                .checked_add(input)
                .filter(|total| *total <= INPUT_QUEUE_LIMIT)
                .ok_or(EventLoopSendError::InputQueueFull)?;
        }
        if let Err(error) = self.sender.send(msg) {
            queue.closed = true;
            queue.input = 0;
            queue.resize = None;
            return Err(EventLoopSendError::Send(error));
        }
        self.poller.notify().map_err(EventLoopSendError::Io)
    }

    pub fn pending_input_bytes(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .input
    }

    pub fn pending_resize_count(&self) -> usize {
        usize::from(
            self.queue
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .resize
                .is_some(),
        )
    }
}

/// All of the mutable state needed to run the event loop.
///
/// Contains list of items to write, current write state, etc. Anything that
/// would otherwise be mutated on the `EventLoop` goes here.
#[derive(Default)]
pub struct State {
    pub error: Option<String>,
    pub exit_status: Option<std::process::ExitStatus>,
    closing: bool,
    output_eof: bool,
    read_pending: bool,
    close_deadline: Option<Instant>,
    write_list: VecDeque<Cow<'static, [u8]>>,
    writing: Option<Writing>,
    parser: ansi::Processor,
}

impl State {
    #[inline]
    fn ensure_next(&mut self) {
        if self.writing.is_none() {
            self.goto_next();
        }
    }

    #[inline]
    fn goto_next(&mut self) {
        self.writing = self.write_list.pop_front().map(Writing::new);
    }

    #[inline]
    fn take_current(&mut self) -> Option<Writing> {
        self.writing.take()
    }

    #[inline]
    fn needs_write(&self) -> bool {
        self.writing.is_some() || !self.write_list.is_empty()
    }

    #[inline]
    fn set_current(&mut self, new: Option<Writing>) {
        self.writing = new;
    }
}

impl Writing {
    #[inline]
    fn new(c: Cow<'static, [u8]>) -> Writing {
        Writing {
            source: c,
            written: 0,
        }
    }

    #[inline]
    fn advance(&mut self, n: usize) {
        self.written += n;
    }

    #[inline]
    fn remaining_bytes(&self) -> &[u8] {
        &self.source[self.written..]
    }

    #[inline]
    fn finished(&self) -> bool {
        self.written >= self.source.len()
    }
}

struct PeekableReceiver<T> {
    rx: Receiver<T>,
    peeked: Option<T>,
}

impl<T> PeekableReceiver<T> {
    fn new(rx: Receiver<T>) -> Self {
        Self { rx, peeked: None }
    }

    fn peek(&mut self) -> Option<&T> {
        if self.peeked.is_none() {
            self.peeked = self.rx.try_recv().ok();
        }

        self.peeked.as_ref()
    }

    fn recv(&mut self) -> Option<T> {
        if self.peeked.is_some() {
            self.peeked.take()
        } else {
            match self.rx.try_recv() {
                Err(TryRecvError::Disconnected) => panic!("event loop channel closed"),
                res => res.ok(),
            }
        }
    }
}
