//! Local Alacritty PTY, parser and grid integration; no external terminal process.

use std::{
    borrow::Cow,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use alacritty_terminal::{
    event::{Event, WindowSize},
    event_loop::{EventLoop, EventLoopSender, Msg},
    tty::windows::control::ProcessControl,
    tty::{self, Options, Shell},
    vte::ansi::{NamedColor, Rgb},
};
use parking_lot::{Condvar, Mutex};
use serde::Serialize;

use crate::{
    Error, ErrorKind, Result,
    profiles::environment::Environment,
    terminal::{TerminalModel, TerminalSize, TerminalView},
};

pub struct SpawnRequest {
    pub program: PathBuf,
    pub conpty_runtime: Option<PathBuf>,
    pub arguments: Vec<String>,
    pub cwd: PathBuf,
    pub environment: Environment,
    pub size: TerminalSize,
    pub history_limit: usize,
    pub palette: Palette,
}

#[derive(Debug, Clone)]
pub struct Palette {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub ansi: [Rgb; 16],
}

impl Default for Palette {
    fn default() -> Self {
        let colors = [
            0x000000, 0xff5a45, 0x7ed99b, 0xe6bd68, 0x86a9ef, 0xc58be5, 0x68c6d8, 0xc8c2bf,
            0x6c625e, 0xff806e, 0xa2f2bb, 0xffd98a, 0xabc5ff, 0xe0aff8, 0x8de3f3, 0xffffff,
        ];
        let rgb = |value: u32| Rgb {
            r: (value >> 16) as u8,
            g: (value >> 8) as u8,
            b: value as u8,
        };
        Self {
            foreground: rgb(0xe8e4e2),
            background: rgb(0),
            cursor: rgb(0xff6427),
            ansi: colors.map(rgb),
        }
    }
}

impl Palette {
    fn color(&self, index: usize) -> Rgb {
        match index {
            0..=15 => self.ansi[index],
            16..=231 => {
                let number = index - 16;
                let levels = [0, 95, 135, 175, 215, 255];
                Rgb {
                    r: levels[number / 36],
                    g: levels[(number / 6) % 6],
                    b: levels[number % 6],
                }
            }
            232..=255 => {
                let value = 8 + (index as u8 - 232) * 10;
                Rgb {
                    r: value,
                    g: value,
                    b: value,
                }
            }
            value if value == NamedColor::Background as usize => self.background,
            value if value == NamedColor::Cursor as usize => self.cursor,
            _ => self.foreground,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExitSummary {
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

pub trait Clipboard: Send + Sync {
    fn read(&self) -> Result<String>;
    fn write(&self, text: &str) -> Result<()>;
}

#[derive(Default)]
pub struct MemoryClipboard {
    value: Mutex<String>,
}
impl Clipboard for MemoryClipboard {
    fn read(&self) -> Result<String> {
        Ok(self.value.lock().clone())
    }
    fn write(&self, text: &str) -> Result<()> {
        if text.len() > 1024 * 1024 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Clipboard payload is too large",
            ));
        }
        *self.value.lock() = text.into();
        Ok(())
    }
}

struct Shared {
    model: TerminalModel,
    size: Mutex<TerminalSize>,
    sender: Mutex<Option<EventLoopSender>>,
    control: Mutex<Option<ProcessControl>>,
    completion: Mutex<Option<ExitSummary>>,
    completed: Condvar,
    stopping: AtomicBool,
    io_done: AtomicBool,
    title: Mutex<String>,
    title_revision: AtomicU64,
    bells: AtomicU64,
    clipboard: Arc<dyn Clipboard>,
    dispatch_error: Mutex<Option<String>>,
    palette: Mutex<Palette>,
}

pub struct NativeTerminal {
    shared: Arc<Shared>,
    pid: u32,
    coordinator: Mutex<Option<JoinHandle<()>>>,
}

impl NativeTerminal {
    pub fn spawn(request: SpawnRequest) -> Result<Self> {
        Self::with_clipboard(request, Arc::new(MemoryClipboard::default()))
    }

    pub fn with_clipboard(request: SpawnRequest, clipboard: Arc<dyn Clipboard>) -> Result<Self> {
        if !request.program.is_absolute()
            || !request.cwd.is_absolute()
            || !request.program.is_file()
            || !request.cwd.is_dir()
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "A native session needs an existing absolute executable and working directory",
            ));
        }
        let model = TerminalModel::new(request.size, request.history_limit)?;
        let invocation = crate::system::process::command::resolve(
            &request.program,
            &request.arguments,
            &request.environment,
        )?;
        let program = invocation
            .program
            .to_str()
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Executable must be Unicode"))?
            .to_owned();
        let options = Options {
            shell: Some(Shell::new(program, invocation.arguments)),
            working_directory: Some(request.cwd),
            // Windows keeps per-drive folders as hidden "=C:" variables; a child's
            // environment block cannot carry them, nor names or values with NUL.
            env: request
                .environment
                .into_iter()
                .filter(|(key, value)| !key.is_empty() && !key.contains(['\0', '=']) && !value.contains('\0'))
                .collect(),
            drain_on_exit: true,
            escape_args: !invocation.verbatim,
            clear_environment: true,
            manage_process_tree: true,
            conpty_runtime: request.conpty_runtime,
        };
        let pty = tty::new(&options, request.size.into(), 0)
            .map_err(|e| Error::new(ErrorKind::Process, format!("Create PTY: {e}")))?;
        let control = pty
            .process_control()
            .map_err(|e| Error::new(ErrorKind::Process, format!("Own PTY process: {e}")))?;
        let pid = control.pid();
        let event_loop = EventLoop::new(
            model.terminal.clone(),
            model.signals.clone(),
            pty,
            true,
            false,
        )
        .map_err(|e| Error::new(ErrorKind::Process, format!("Create terminal I/O: {e}")))?;
        let shared = Arc::new(Shared {
            model,
            size: Mutex::new(request.size),
            sender: Mutex::new(Some(event_loop.channel())),
            control: Mutex::new(Some(control)),
            completion: Mutex::new(None),
            completed: Condvar::new(),
            stopping: AtomicBool::new(false),
            io_done: AtomicBool::new(false),
            title: Mutex::new(String::new()),
            title_revision: AtomicU64::new(0),
            bells: AtomicU64::new(0),
            clipboard,
            dispatch_error: Mutex::new(None),
            palette: Mutex::new(request.palette),
        });
        let owner = shared.clone();
        let coordinator = thread::Builder::new()
            .name("terminal-session".into())
            .spawn(move || {
                let receiver = owner.clone();
                let dispatcher =
                    thread::Builder::new()
                        .name("terminal-events".into())
                        .spawn(move || {
                            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                dispatch(&receiver)
                            }))
                            .is_err()
                            {
                                fail_dispatch(
                                    &receiver,
                                    "Terminal event dispatcher panicked".into(),
                                );
                            }
                        });
                let result = match dispatcher {
                    Err(error) => {
                        drop(event_loop);
                        Err(error.to_string())
                    }
                    Ok(dispatcher) => {
                        let result = match event_loop.spawn_checked() {
                            Err(error) => Err(error.to_string()),
                            Ok(worker) => match worker.join() {
                                Err(_) => Err("Terminal I/O thread panicked".into()),
                                Ok((event_loop, state)) => {
                                    let result = state.error.clone().map_or(
                                        Ok(state.exit_status.and_then(|status| status.code())),
                                        Err,
                                    );
                                    drop(event_loop);
                                    drop(state);
                                    result
                                }
                            },
                        };
                        owner.io_done.store(true, Ordering::Release);
                        owner.model.finish_events();
                        if dispatcher.join().is_err() {
                            *owner.dispatch_error.lock() =
                                Some("Terminal event dispatcher panicked".into());
                        }
                        result
                    }
                };
                owner.io_done.store(true, Ordering::Release);
                owner.model.finish_events();
                let code = owner
                    .control
                    .lock()
                    .as_ref()
                    .and_then(|control| control.exit_status().ok().flatten())
                    .and_then(|status| status.code());
                owner.sender.lock().take();
                owner.control.lock().take();
                let dispatch_error = owner.dispatch_error.lock().take();
                let summary = match result {
                    Ok(result) => ExitSummary {
                        exit_code: result.or(code),
                        error: dispatch_error,
                    },
                    Err(error) => ExitSummary {
                        exit_code: code,
                        error: Some(
                            dispatch_error
                                .map_or(error.clone(), |dispatch| format!("{error}; {dispatch}")),
                        ),
                    },
                };
                *owner.completion.lock() = Some(summary);
                owner.completed.notify_all();
            })
            .map_err(|error| {
                Error::new(
                    ErrorKind::Process,
                    format!("Start terminal coordinator: {error}"),
                )
            })?;
        Ok(Self {
            shared,
            pid,
            coordinator: Mutex::new(Some(coordinator)),
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn model(&self) -> TerminalView {
        TerminalView::new(&self.shared.model)
    }
    pub fn scroll(&self, lines: i32) {
        self.shared.model.scroll(lines);
    }
    pub fn set_palette(&self, palette: Palette) {
        *self.shared.palette.lock() = palette;
    }
    pub fn title(&self) -> String {
        self.shared.title.lock().clone()
    }
    pub fn title_update_since(&self, version: u64) -> Option<(u64, String)> {
        if self.shared.title_revision.load(Ordering::Acquire) == version {
            return None;
        }
        let title = self.shared.title.lock();
        let version = self.shared.title_revision.load(Ordering::Acquire);
        Some((version, title.clone()))
    }
    pub fn bell_count(&self) -> u64 {
        self.shared.bells.load(Ordering::Acquire)
    }
    pub fn exit_summary(&self) -> Option<ExitSummary> {
        self.shared.completion.lock().clone()
    }
    pub fn queued_input_bytes(&self) -> usize {
        self.shared
            .sender
            .lock()
            .as_ref()
            .map_or(0, EventLoopSender::pending_input_bytes)
    }

    pub fn queued_resize_count(&self) -> usize {
        self.shared
            .sender
            .lock()
            .as_ref()
            .map_or(0, EventLoopSender::pending_resize_count)
    }

    pub fn input(&self, bytes: Vec<u8>) -> Result<()> {
        if self.shared.stopping.load(Ordering::Acquire)
            || self.shared.io_done.load(Ordering::Acquire)
        {
            return Err(Error::new(
                ErrorKind::Process,
                "Session no longer accepts input",
            ));
        }
        if bytes.is_empty() {
            return Ok(());
        }
        self.shared
            .model
            .terminal
            .lock()
            .scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        self.shared.model.signals.changed();
        send(&self.shared, Msg::Input(Cow::Owned(bytes)))
    }

    pub fn resize(&self, size: TerminalSize) -> Result<()> {
        TerminalSize::new(size.columns, size.rows, size.cell_width, size.cell_height)?;
        if self.shared.stopping.load(Ordering::Acquire)
            || self.shared.io_done.load(Ordering::Acquire)
        {
            return Err(Error::new(
                ErrorKind::Process,
                "Session no longer accepts resize",
            ));
        }
        let mut previous = self.shared.size.lock();
        if *previous != size {
            send(&self.shared, Msg::Resize(size.into()))?;
            *previous = size;
        }
        Ok(())
    }

    pub fn wait(&self, timeout: Duration) -> Result<ExitSummary> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Invalid terminal wait duration"))?;
        let mut completion = self.shared.completion.lock();
        while completion.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::new(
                    ErrorKind::Busy,
                    "Terminal shutdown has not finished",
                ));
            }
            self.shared.completed.wait_for(&mut completion, remaining);
        }
        let result = completion.clone().expect("checked completion");
        drop(completion);
        if let Some(coordinator) = self.coordinator.lock().take() {
            coordinator
                .join()
                .map_err(|_| Error::new(ErrorKind::Process, "Terminal coordinator panicked"))?;
        }
        Ok(result)
    }

    pub fn stop(&self, grace: Duration) -> Result<ExitSummary> {
        self.shared.stopping.store(true, Ordering::Release);
        if self.exit_summary().is_none() {
            let _interrupt = send(&self.shared, Msg::Input(Cow::Borrowed(b"\x03")));
            if let Ok(result) = self.wait(grace) {
                return Ok(result);
            }
            if let Some(control) = self.shared.control.lock().as_ref() {
                control.terminate(1).map_err(|error| {
                    Error::new(
                        ErrorKind::Process,
                        format!("Stop owned process tree: {error}"),
                    )
                })?;
            }
            let _shutdown = send(&self.shared, Msg::Shutdown);
        }
        self.wait(Duration::from_secs(12))
    }
}

impl Drop for NativeTerminal {
    fn drop(&mut self) {
        if self.exit_summary().is_none() {
            let _closed = self.stop(Duration::ZERO);
        }
        if self.exit_summary().is_some() {
            if let Some(coordinator) = self.coordinator.lock().take() {
                let _joined = coordinator.join();
            }
        }
    }
}

fn send(shared: &Shared, message: Msg) -> Result<()> {
    shared
        .sender
        .lock()
        .as_ref()
        .ok_or_else(|| Error::new(ErrorKind::Process, "Terminal I/O is closed"))?
        .send(message)
        .map_err(|error| Error::new(ErrorKind::Process, error.to_string()))
}

fn reply(shared: &Shared, text: String) -> Result<()> {
    if shared.io_done.load(Ordering::Acquire) {
        return Ok(());
    }
    if shared
        .control
        .lock()
        .as_ref()
        .is_some_and(|control| control.exit_status().ok().flatten().is_some())
    {
        return Ok(());
    }
    match send(shared, Msg::Input(Cow::Owned(text.into_bytes()))) {
        Err(_)
            if shared.stopping.load(Ordering::Acquire)
                || shared.io_done.load(Ordering::Acquire) =>
        {
            Ok(())
        }
        result => result,
    }
}

fn dispatch(shared: &Shared) {
    let result = (|| -> Result<()> {
        while let Some(events) = shared.model.wait_events()? {
            for event in events {
                match event {
                    Event::PtyWrite(text) => reply(shared, text)?,
                    Event::TextAreaSizeRequest(format) => {
                        reply(shared, format(WindowSize::from(*shared.size.lock())))?
                    }
                    Event::ColorRequest(index, format) => {
                        let color = if index < alacritty_terminal::term::color::COUNT {
                            shared.model.with_view(|view| view.colors[index])
                        } else {
                            None
                        }
                        .unwrap_or_else(|| shared.palette.lock().color(index));
                        reply(shared, format(color))?;
                    }
                    Event::ClipboardStore(_, text) => shared.clipboard.write(&text)?,
                    Event::ClipboardLoad(_, format) => {
                        reply(shared, format(&shared.clipboard.read()?))?
                    }
                    Event::Title(title) => {
                        let mut previous = shared.title.lock();
                        if *previous != title {
                            *previous = title;
                            shared.title_revision.fetch_add(1, Ordering::Release);
                        }
                    }
                    Event::ResetTitle => {
                        let mut previous = shared.title.lock();
                        if !previous.is_empty() {
                            previous.clear();
                            shared.title_revision.fetch_add(1, Ordering::Release);
                        }
                    }
                    Event::Bell => {
                        shared.bells.fetch_add(1, Ordering::Relaxed);
                    }
                    Event::Exit => (),
                    Event::ChildExit(_) => {
                        shared.stopping.store(true, Ordering::Release);
                    }
                    Event::MouseCursorDirty | Event::CursorBlinkingChange | Event::Wakeup => (),
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        fail_dispatch(shared, error.to_string());
    }
}

fn fail_dispatch(shared: &Shared, message: String) {
    *shared.dispatch_error.lock() = Some(message);
    if let Some(control) = shared.control.lock().as_ref() {
        let _terminated = control.terminate(1);
    }
    let _shutdown = send(shared, Msg::Shutdown);
}
