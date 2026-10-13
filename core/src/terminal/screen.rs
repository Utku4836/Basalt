use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use alacritty_terminal::{
    Term,
    event::{Event, EventListener, WindowSize},
    grid::Dimensions,
    sync::FairMutex,
    term::{Config, MIN_COLUMNS, MIN_SCREEN_LINES, RenderableContent},
    vte::ansi::{Color, Processor},
};
use parking_lot::{Condvar, Mutex};
use serde::Serialize;

use crate::{Error, ErrorKind, Result};

const EVENT_LIMIT: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl TerminalSize {
    pub fn new(columns: u16, rows: u16, cell_width: u16, cell_height: u16) -> Result<Self> {
        if !(MIN_COLUMNS as u16..=1000).contains(&columns)
            || !(MIN_SCREEN_LINES as u16..=500).contains(&rows)
            || cell_width == 0
            || cell_height == 0
            || u32::from(columns) * u32::from(cell_width) > 16384
            || u32::from(rows) * u32::from(cell_height) > 16384
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Invalid terminal dimensions",
            ));
        }
        Ok(Self {
            columns,
            rows,
            cell_width,
            cell_height,
        })
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        usize::from(self.rows)
    }
    fn screen_lines(&self) -> usize {
        usize::from(self.rows)
    }
    fn columns(&self) -> usize {
        usize::from(self.columns)
    }
}

impl From<TerminalSize> for WindowSize {
    fn from(size: TerminalSize) -> Self {
        Self {
            num_cols: size.columns,
            num_lines: size.rows,
            cell_width: size.cell_width,
            cell_height: size.cell_height,
        }
    }
}

#[derive(Clone)]
pub(crate) struct TerminalSignals {
    pending: Arc<Mutex<VecDeque<Event>>>,
    revision: Arc<AtomicU64>,
    overflow: Arc<AtomicBool>,
    ready: Arc<Condvar>,
    closed: Arc<AtomicBool>,
    /// Called after every change, from whichever thread made it.
    wake: Arc<Mutex<Option<Waker>>>,
}

/// Wakes a renderer when the screen changes.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

impl TerminalSignals {
    fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(VecDeque::new())),
            revision: Arc::new(AtomicU64::new(0)),
            overflow: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(Condvar::new()),
            closed: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(Mutex::new(None)),
        }
    }
    pub(crate) fn changed(&self) {
        self.revision.fetch_add(1, Ordering::Release);
        let wake = self.wake.lock().clone();
        if let Some(wake) = wake {
            wake();
        }
    }
}

impl EventListener for TerminalSignals {
    fn send_event(&self, event: Event) {
        if matches!(
            event,
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange
        ) {
            self.changed();
            return;
        }
        let mut pending = self.pending.lock();
        if matches!(event, Event::Title(_) | Event::ResetTitle) {
            pending.retain(|previous| !matches!(previous, Event::Title(_) | Event::ResetTitle));
        }
        if pending.len() >= EVENT_LIMIT {
            self.overflow.store(true, Ordering::Release);
            self.ready.notify_one();
            return;
        }
        pending.push_back(event);
        self.ready.notify_one();
    }
}

#[derive(Clone)]
pub struct TerminalModel {
    pub(crate) terminal: Arc<FairMutex<Term<TerminalSignals>>>,
    pub(crate) signals: TerminalSignals,
}

/// Read-only renderer access without the terminal's internal response/event queue.
#[derive(Clone)]
pub struct TerminalView {
    model: TerminalModel,
}

impl TerminalView {
    pub(crate) fn new(model: &TerminalModel) -> Self {
        Self {
            model: model.clone(),
        }
    }
    pub fn revision(&self) -> u64 {
        self.model.revision()
    }
    pub fn snapshot(&self) -> TerminalSnapshot {
        self.model.snapshot()
    }
    pub fn read_screen(&self, screen: &mut Screen) {
        self.model.read_screen(screen)
    }
    pub fn mode(&self) -> u32 {
        self.model.mode()
    }
    pub fn live_text(&self) -> LiveText {
        self.model.live_text()
    }
    /// Call `wake` after every change to the screen, instead of polling the revision.
    /// It runs on the terminal's threads and should only schedule work.
    pub fn on_change(&self, wake: Waker) {
        *self.model.signals.wake.lock() = Some(wake);
    }
    pub fn with_view<R>(&self, read: impl FnOnce(RenderableContent<'_>) -> R) -> R {
        self.model.with_view(read)
    }
}

impl TerminalModel {
    pub(crate) fn finish_events(&self) {
        let _pending = self.signals.pending.lock();
        self.signals.closed.store(true, Ordering::Release);
        self.signals.ready.notify_all();
    }

    pub(crate) fn wait_events(&self) -> Result<Option<Vec<Event>>> {
        let mut pending = self.signals.pending.lock();
        while pending.is_empty()
            && !self.signals.overflow.load(Ordering::Acquire)
            && !self.signals.closed.load(Ordering::Acquire)
        {
            self.signals.ready.wait(&mut pending);
        }
        if self.signals.overflow.swap(false, Ordering::AcqRel) {
            return Err(Error::new(
                ErrorKind::Process,
                "Terminal event consumer exceeded its bounded queue",
            ));
        }
        if pending.is_empty() && self.signals.closed.load(Ordering::Acquire) {
            return Ok(None);
        }
        Ok(Some(pending.drain(..).collect()))
    }
    pub fn new(size: TerminalSize, history_limit: usize) -> Result<Self> {
        TerminalSize::new(size.columns, size.rows, size.cell_width, size.cell_height)?;
        if history_limit > 100_000 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Terminal history limit is too large",
            ));
        }
        let signals = TerminalSignals::new();
        let config = Config {
            scrolling_history: history_limit,
            ..Default::default()
        };
        let terminal = Arc::new(FairMutex::new(Term::new(config, &size, signals.clone())));
        Ok(Self { terminal, signals })
    }

    pub fn revision(&self) -> u64 {
        self.signals.revision.load(Ordering::Acquire)
    }

    /// Move the view into the scrollback by `lines`; positive goes back in history.
    pub fn scroll(&self, lines: i32) {
        self.terminal
            .lock()
            .scroll_display(alacritty_terminal::grid::Scroll::Delta(lines));
        self.signals.changed();
    }

    /// Short read access to the native grid; do not block or perform I/O in the callback.
    pub fn with_view<R>(&self, read: impl FnOnce(RenderableContent<'_>) -> R) -> R {
        let terminal = self.terminal.lock();
        read(terminal.renderable_content())
    }

    /// The live screen as text, whatever part of the history the view shows: one line
    /// per row without trailing spaces, with the last progress report.
    pub fn live_text(&self) -> LiveText {
        use alacritty_terminal::{
            index::{Column, Line},
            term::cell::Flags,
        };
        let terminal = self.terminal.lock();
        let grid = terminal.grid();
        let (rows, columns) = (terminal.screen_lines(), terminal.columns());
        let mut text = String::with_capacity(rows * (columns + 1));
        for line in 0..rows {
            let row = &grid[Line(line as i32)];
            let start = text.len();
            for column in 0..columns {
                let cell = &row[Column(column)];
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    continue;
                }
                text.push(cell.c);
                if let Some(extra) = cell.zerowidth() {
                    text.extend(extra.iter());
                }
            }
            let kept = start + text[start..].trim_end().len();
            text.truncate(kept);
            text.push('\n');
        }
        LiveText {
            revision: self.revision(),
            text,
            progress: terminal.progress().to_owned(),
        }
    }

    /// Terminal mode bits (cursor visibility, application keys, mouse reporting, ...),
    /// without copying the screen.
    pub fn mode(&self) -> u32 {
        self.terminal.lock().mode().bits()
    }

    /// Copy the visible screen into `screen`, reusing its buffers: one fixed-size cell
    /// per grid position and no allocation per cell. This is the renderer's path; the
    /// lock is held only for the copy.
    pub fn read_screen(&self, screen: &mut Screen) {
        let terminal = self.terminal.lock();
        let content = terminal.renderable_content();
        let columns = terminal.columns();
        let rows = terminal.screen_lines();
        let offset = content.display_offset as i32;
        screen.revision = self.revision();
        screen.columns = columns;
        screen.rows = rows;
        screen.display_offset = content.display_offset;
        screen.cursor_line = content.cursor.point.line.0;
        screen.cursor_column = content.cursor.point.column.0;
        screen.mode = content.mode.bits();
        screen.cells.clear();
        screen.cells.resize(columns * rows, ScreenCell::BLANK);
        screen.extras.clear();
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            let column = indexed.point.column.0;
            if row < 0 || row as usize >= rows || column >= columns {
                continue;
            }
            let index = row as usize * columns + column;
            let cell = indexed.cell;
            screen.cells[index] = ScreenCell {
                c: cell.c,
                foreground: CellColor::from(cell.fg),
                background: CellColor::from(cell.bg),
                flags: cell.flags.bits(),
            };
            if let Some(extra) = cell.zerowidth() {
                screen.extras.push((index, extra.iter().collect()));
            }
        }
    }

    /// A copied visible view for diagnostics and consumers that cannot borrow the grid.
    pub fn snapshot(&self) -> TerminalSnapshot {
        let terminal = self.terminal.lock();
        let content = terminal.renderable_content();
        let cursor = content.cursor.point;
        TerminalSnapshot {
            revision: self.revision(),
            columns: terminal.columns(),
            rows: terminal.screen_lines(),
            history: terminal.history_size(),
            display_offset: content.display_offset,
            cursor_line: cursor.line.0,
            cursor_column: cursor.column.0,
            mode: content.mode.bits(),
            cells: content
                .display_iter
                .map(|indexed| {
                    let cell = indexed.cell;
                    let mut text = cell.c.to_string();
                    if let Some(extra) = cell.zerowidth() {
                        text.extend(extra.iter());
                    }
                    VisibleCell {
                        line: indexed.point.line.0,
                        column: indexed.point.column.0,
                        text,
                        foreground: CellColor::from(cell.fg),
                        background: CellColor::from(cell.bg),
                        flags: cell.flags.bits(),
                    }
                })
                .collect(),
        }
    }

    pub fn take_events(&self) -> Result<Vec<Event>> {
        if self.signals.overflow.swap(false, Ordering::AcqRel) {
            return Err(Error::new(
                ErrorKind::Process,
                "Terminal event consumer fell behind its bounded queue",
            ));
        }
        Ok(self.signals.pending.lock().drain(..).collect())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum CellColor {
    Named(u16),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl From<Color> for CellColor {
    fn from(color: Color) -> Self {
        match color {
            Color::Named(value) => Self::Named(value as u16),
            Color::Indexed(value) => Self::Indexed(value),
            Color::Spec(value) => Self::Rgb(value.r, value.g, value.b),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VisibleCell {
    pub line: i32,
    pub column: usize,
    pub text: String,
    pub foreground: CellColor,
    pub background: CellColor,
    pub flags: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TerminalSnapshot {
    pub revision: u64,
    pub columns: usize,
    pub rows: usize,
    pub history: usize,
    pub display_offset: usize,
    pub cursor_line: i32,
    pub cursor_column: usize,
    pub mode: u32,
    pub cells: Vec<VisibleCell>,
}

/// One grid position of the visible screen, copied for rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScreenCell {
    pub c: char,
    pub foreground: CellColor,
    pub background: CellColor,
    pub flags: u16,
}

impl ScreenCell {
    /// An empty cell in the default colours.
    pub const BLANK: Self = Self {
        c: ' ',
        foreground: CellColor::Named(256),
        background: CellColor::Named(257),
        flags: 0,
    };
}

/// The bottom of a terminal as text, for reading what a program shows there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveText {
    pub revision: u64,
    pub text: String,
    /// The last progress report (`OSC 9;4`), as `4;state;value`, or empty.
    pub progress: String,
}

/// The visible screen, row-major: `cells[row * columns + column]`. Reused between
/// reads so a frame costs a copy, not an allocation per cell.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Screen {
    pub revision: u64,
    pub columns: usize,
    pub rows: usize,
    pub display_offset: usize,
    pub cursor_line: i32,
    pub cursor_column: usize,
    pub mode: u32,
    pub cells: Vec<ScreenCell>,
    /// Zero-width characters (combining marks, variation selectors) that follow the
    /// cell at the given index.
    pub extras: Vec<(usize, String)>,
}

impl Screen {
    pub fn row(&self, row: usize) -> &[ScreenCell] {
        &self.cells[row * self.columns..(row + 1) * self.columns]
    }
}

/// Byte-stream decoding without a child process. Native sessions use Alacritty's
/// I/O parser instead; the two parsers never process the same session stream.
pub struct TerminalDecoder {
    model: TerminalModel,
    parser: Processor,
}

impl TerminalDecoder {
    pub fn new(size: TerminalSize, history_limit: usize) -> Result<Self> {
        Ok(Self {
            model: TerminalModel::new(size, history_limit)?,
            parser: Processor::default(),
        })
    }
    pub fn model(&self) -> &TerminalModel {
        &self.model
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut terminal = self.model.terminal.lock();
        terminal.scan_progress(bytes);
        self.parser.advance(&mut *terminal, bytes);
        drop(terminal);
        self.model.signals.changed();
    }

    pub fn resize(&mut self, size: TerminalSize) -> Result<()> {
        TerminalSize::new(size.columns, size.rows, size.cell_width, size.cell_height)?;
        self.model.terminal.lock().resize(size);
        self.model.signals.changed();
        Ok(())
    }
}
