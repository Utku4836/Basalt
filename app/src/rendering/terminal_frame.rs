//! Convert terminal cells into styled native text runs, never a scaled bitmap.
//!
//! A surface keeps a persistent model of lines. Each line is keyed by a hash of its
//! cells: unchanged lines keep their elements, lines seen before come from a cache, and
//! output that scrolls moves lines up instead of rebuilding every line on screen.
use crate::{TerminalFrame, TerminalLine, TerminalRun};
use slint::{Color, Model, ModelRc, VecModel};
use std::{collections::HashMap, rc::Rc};
use basalt_core::{
    terminal::pty::Palette,
    terminal::{CellColor, Screen, ScreenCell, TerminalSnapshot},
};

static PALETTE: std::sync::LazyLock<Palette> = std::sync::LazyLock::new(|| {
    let mut palette = Palette::default();
    let base = palette.background;
    let rgb = |r, g, b| {
        let mut value = base;
        value.r = r;
        value.g = g;
        value.b = b;
        value
    };
    palette.background = rgb(0x15, 0x1a, 0x27);
    // Neutral greys and true hues: a program that draws in the default color, or in
    // white, looks the way it was designed, without a tint of the app's own.
    palette.foreground = rgb(0xe8, 0xe8, 0xea);
    palette.cursor = rgb(0xa3, 0xb4, 0xde);
    palette.ansi = [
        0x2e3036_u32,
        0xf2706b,
        0x8fd18f,
        0xe8c26e,
        0x6fa8f5,
        0xc792ea,
        0x5fd3d3,
        0xd4d4d8,
        0x6e7079,
        0xff8c87,
        0xa8e6a3,
        0xf5d68a,
        0x93bfff,
        0xddb0f5,
        0x8de6e6,
        0xf4f4f6,
    ]
    .map(|value| rgb((value >> 16) as u8, (value >> 8) as u8, value as u8));
    palette
});
pub fn palette() -> Palette {
    PALETTE.clone()
}
/// Per-surface render state: the copied screen, each line's content key, the
/// persistent line model the surface draws, and built lines by content key.
#[derive(Default)]
pub struct ScreenRenderer {
    screen: Screen,
    keys: Vec<u64>,
    lines: Rc<VecModel<TerminalLine>>,
    cache: HashMap<u64, TerminalLine>,
}

impl ScreenRenderer {
    /// Bring the line model up to date with the screen `read` copies in. Returns the
    /// frame to draw; its line model is the same one every time.
    pub fn update(&mut self, read: impl FnOnce(&mut Screen), focused: bool) -> TerminalFrame {
        read(&mut self.screen);
        let rows = self.screen.rows;
        let keys: Vec<u64> = (0..rows).map(|row| line_key(&self.screen, row)).collect();
        if self.keys.len() != rows || self.lines.row_count() != rows {
            let lines: Vec<_> = (0..rows).map(|row| self.line(row, keys[row])).collect();
            self.lines.set_vec(lines);
        } else if self.keys != keys {
            // Output that scrolled up by `shift` lines: drop them from the top and add
            // the new ones at the bottom; the lines in between keep their elements.
            let matches = |shift: usize| (0..rows - shift).filter(|&row| self.keys[row + shift] == keys[row]).count();
            let shift = (1..rows).max_by_key(|&shift| (matches(shift), std::cmp::Reverse(shift))).unwrap_or(0);
            let mut old = self.keys.clone();
            if shift > 0 && matches(shift) > matches(0) {
                for _ in 0..shift {
                    self.lines.remove(0);
                }
                old.drain(..shift);
                for row in rows - shift..rows {
                    let line = self.line(row, keys[row]);
                    self.lines.push(line);
                    old.push(keys[row]);
                }
            }
            for row in 0..rows {
                if old[row] != keys[row] {
                    let line = self.line(row, keys[row]);
                    self.lines.set_row_data(row, line);
                }
            }
        }
        self.keys = keys;
        self.header(focused)
    }

    fn header(&self, focused: bool) -> TerminalFrame {
        TerminalFrame { lines: ModelRc::from(self.lines.clone()), ..screen_frame(&self.screen, focused) }
    }

    fn line(&mut self, row: usize, key: u64) -> TerminalLine {
        if let Some(line) = self.cache.get(&key) {
            return line.clone();
        }
        let line = TerminalLine {
            runs: ModelRc::new(VecModel::from(line_runs(&self.screen, row))),
        };
        // A few screens' worth of lines; scrolling back past that rebuilds them.
        if self.cache.len() > 8 * self.screen.rows.max(32) {
            self.cache.clear();
        }
        self.cache.insert(key, line.clone());
        line
    }
}

/// A frame with the screen's size and cursor but no lines, for grids drawn elsewhere.
pub fn screen_frame(screen: &Screen, focused: bool) -> TerminalFrame {
    TerminalFrame {
        columns: screen.columns as i32,
        rows: screen.rows as i32,
        cursor_column: screen.cursor_column as i32,
        cursor_row: screen.cursor_line,
        cursor_visible: focused && screen.mode & 1 != 0 && screen.cursor_line >= 0 && screen.display_offset == 0,
        cursor_color: rgb(cursor_rgb()),
        ..Default::default()
    }
}

/// The cursor colour of the palette.
pub fn cursor_rgb() -> [u8; 3] {
    [PALETTE.cursor.r, PALETTE.cursor.g, PALETTE.cursor.b]
}

/// The zero-width characters attached to cells of `row`, in column order.
fn row_extras(screen: &Screen, row: usize) -> &[(usize, String)] {
    let start = row * screen.columns;
    let end = start + screen.columns;
    let first = screen.extras.partition_point(|(index, _)| *index < start);
    let last = screen.extras.partition_point(|(index, _)| *index < end);
    &screen.extras[first..last]
}

/// FNV-1a over a line's cells and attached characters.
fn line_key(screen: &Screen, row: usize) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut eat = |value: u64| {
        for byte in value.to_le_bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
    };
    let colour = |value: &CellColor| match *value {
        CellColor::Named(index) => u64::from(index),
        CellColor::Indexed(index) => 0x1_0000 | u64::from(index),
        CellColor::Rgb(r, g, b) => 0x100_0000 | u64::from(r) << 16 | u64::from(g) << 8 | u64::from(b),
    };
    for cell in screen.row(row) {
        eat(u64::from(cell.c as u32) | u64::from(cell.flags) << 32);
        eat(colour(&cell.foreground) << 32 | colour(&cell.background));
    }
    for (index, text) in row_extras(screen, row) {
        eat(*index as u64);
        for ch in text.chars() {
            eat(u64::from(ch as u32));
        }
    }
    hash
}

/// A line model flattened into runs with their row, for tests and diagnostics.
pub fn flat_runs(frame: &TerminalFrame) -> ModelRc<TerminalRun> {
    let mut runs = Vec::new();
    for (row, line) in frame.lines.iter().enumerate() {
        runs.extend(line.runs.iter().map(|run| TerminalRun { row: row as i32, ..run }));
    }
    ModelRc::new(VecModel::from(runs))
}

#[derive(PartialEq)]
struct Style {
    foreground: [u8; 3],
    background: [u8; 3],
    flags: u16,
}
fn style(foreground: &CellColor, background: &CellColor, flags: u16) -> Style {
    let mut fg = color(foreground, false);
    let mut bg = color(background, true);
    if flags & 1 != 0 {
        std::mem::swap(&mut fg, &mut bg);
    }
    if flags & 128 != 0 {
        for value in &mut fg {
            *value = (*value as f32 * 0.65) as u8;
        }
    }
    Style {
        foreground: fg,
        background: bg,
        flags: flags & (2 | 4 | 8 | 512),
    }
}
/// Resolved colours and attributes of one cell, for renderers that draw cells
/// themselves.
pub(crate) struct CellStyle {
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    /// Left to the surface, which is the theme colour.
    pub default_background: bool,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub hidden: bool,
}
pub(crate) fn cell_style(cell: &ScreenCell) -> CellStyle {
    let style = style(&cell.foreground, &cell.background, cell.flags);
    CellStyle {
        foreground: style.foreground,
        background: style.background,
        default_background: matches!(cell.background, CellColor::Named(257)) && cell.flags & 1 == 0,
        bold: style.flags & 2 != 0,
        italic: style.flags & 4 != 0,
        underline: style.flags & 8 != 0,
        strike: style.flags & 512 != 0,
        hidden: cell.flags & (256 | 1024) != 0,
    }
}
fn rgb(value: [u8; 3]) -> Color {
    Color::from_rgb_u8(value[0], value[1], value[2])
}
/// One frame from a copied snapshot, for tests and diagnostics; surfaces keep a
/// `ScreenRenderer` instead.
pub fn frame(snapshot: &TerminalSnapshot, focused: bool) -> TerminalFrame {
    ScreenRenderer::default().update(
        |screen| {
            screen.revision = snapshot.revision;
            screen.columns = snapshot.columns;
            screen.rows = snapshot.rows;
            screen.display_offset = snapshot.display_offset;
            screen.cursor_line = snapshot.cursor_line;
            screen.cursor_column = snapshot.cursor_column;
            screen.mode = snapshot.mode;
            screen.cells = vec![ScreenCell::BLANK; snapshot.columns * snapshot.rows];
            screen.extras.clear();
            for cell in &snapshot.cells {
                let row = cell.line + snapshot.display_offset as i32;
                if row < 0 || row as usize >= snapshot.rows || cell.column >= snapshot.columns {
                    continue;
                }
                let index = row as usize * snapshot.columns + cell.column;
                let mut chars = cell.text.chars();
                screen.cells[index] = ScreenCell {
                    c: chars.next().unwrap_or(' '),
                    foreground: cell.foreground,
                    background: cell.background,
                    flags: cell.flags,
                };
                let extra: String = chars.collect();
                if !extra.is_empty() {
                    screen.extras.push((index, extra));
                }
            }
        },
        focused,
    )
}

/// Styled runs for one line. Plain monospace alphanumerics share a bounded run;
/// Unicode, symbols and styled clusters keep individual cell anchors. Empty cells in
/// the default colours are left to the surface background.
fn line_runs(screen: &Screen, row: usize) -> Vec<TerminalRun> {
    // A run's text grows in one buffer and becomes a shared string once, at the end.
    struct Building {
        run: TerminalRun,
        text: String,
    }
    let mut runs: Vec<Building> = Vec::new();
    let mut pending_spaces = 0;
    let default_background = [PALETTE.background.r, PALETTE.background.g, PALETTE.background.b];
    let extras = row_extras(screen, row);
    let mut extra = extras.iter().peekable();
    for (column, cell) in screen.row(row).iter().enumerate() {
        let attached = extra
            .next_if(|(index, _)| *index == row * screen.columns + column)
            .map(|(_, text)| text.as_str());
        if cell.flags & 64 != 0 {
            continue;
        }
        let style = style(&cell.foreground, &cell.background, cell.flags);
        let uses_default_background = matches!(cell.background, CellColor::Named(257)) && cell.flags & 1 == 0;
        let cells = if cell.flags & 32 != 0 { 2 } else { 1 };
        let hidden = cell.flags & (256 | 1024) != 0;
        let mut glyph = [0u8; 4];
        let combined;
        let text: &str = if hidden {
            if cells == 2 { "  " } else { " " }
        } else if let Some(attached) = attached {
            combined = format!("{}{attached}", cell.c);
            &combined
        } else {
            cell.c.encode_utf8(&mut glyph)
        };
        if text.trim().is_empty()
            && uses_default_background
            && style.background == default_background
            && style.flags & (8 | 512) == 0
        {
            if text == " "
                && cells == 1
                && style.flags == 0
                && runs.last().is_some_and(|last| {
                    last.run.plain_ascii
                        && last.run.default_background
                        && last.run.background == rgb(default_background)
                        && last.run.column + last.run.cells + pending_spaces == column as i32
                })
            {
                pending_spaces += 1;
            } else {
                pending_spaces = 0;
            }
            continue;
        }
        let plain_ascii = cells == 1 && style.flags == 0 && text.len() == 1 && text.as_bytes()[0].is_ascii_alphanumeric();
        if plain_ascii {
            if let Some(last) = runs.last_mut().filter(|last| {
                last.run.plain_ascii
                    && last.run.default_background == uses_default_background
                    && last.run.cells + pending_spaces < 128
                    && last.run.column + last.run.cells + pending_spaces == column as i32
                    && last.run.foreground == rgb(style.foreground)
                    && last.run.background == rgb(style.background)
            }) {
                last.text.extend(std::iter::repeat_n(' ', pending_spaces as usize));
                last.text.push_str(text);
                last.run.cells += pending_spaces + 1;
                pending_spaces = 0;
                continue;
            }
        }
        pending_spaces = 0;
        runs.push(Building {
            run: TerminalRun {
                row: row as i32,
                column: column as i32,
                cells,
                text: Default::default(),
                foreground: rgb(style.foreground),
                background: rgb(style.background),
                bold: style.flags & 2 != 0,
                italic: style.flags & 4 != 0,
                underline: style.flags & 8 != 0,
                strike: style.flags & 512 != 0,
                plain_ascii,
                default_background: uses_default_background,
            },
            text: text.to_string(),
        });
    }
    runs.into_iter()
        .map(|building| TerminalRun { text: building.text.into(), ..building.run })
        .collect()
}
fn color(value: &CellColor, background: bool) -> [u8; 3] {
    let index = match value {
        CellColor::Rgb(r, g, b) => return [*r, *g, *b],
        CellColor::Named(index) => *index,
        CellColor::Indexed(index) => (*index).into(),
    };
    let palette = &*PALETTE;
    let resolved = match index {
        0..=15 => palette.ansi[index as usize],
        16..=231 => {
            let n = index - 16;
            let levels = [0, 95, 135, 175, 215, 255];
            return [
                levels[(n / 36) as usize],
                levels[((n / 6) % 6) as usize],
                levels[(n % 6) as usize],
            ];
        }
        232..=255 => {
            let value = 8 + (index - 232) as u8 * 10;
            return [value; 3];
        }
        257 => palette.background,
        _ if background => palette.background,
        _ => palette.foreground,
    };
    [resolved.r, resolved.g, resolved.b]
}
