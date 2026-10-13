//! Cells to quads: background runs, then glyphs, then lines under or through text.

use super::{ListItem, Overlay};
use super::atlas::{Atlas, AtlasFull};
use super::glyphs::GlyphCache;
use crate::screen::cell_style;
use bytemuck::{Pod, Zeroable};
use std::time::Instant;
use basalt_core::terminal::Screen;
use wgpu::Queue;

/// One rectangle: a cell background, a glyph or a line under or through text.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct Quad {
    position: [f32; 2],
    size: [f32; 2],
    uv: [f32; 4],
    color: [u8; 4],
}

impl Quad {
    fn solid(x: f32, y: f32, width: f32, height: f32, color: [u8; 3]) -> Self {
        Self { position: [x, y], size: [width, height], uv: [0.0; 4], color: [color[0], color[1], color[2], 255] }
    }
}

/// What moves in the overlay at this frame: the angle marker, as a fractional list
/// index while it glides, and the time a progress bar is drawn at.
#[derive(Clone, Copy, Debug)]
pub struct Motion {
    pub marker: Option<f32>,
    pub now: Instant,
}

/// Where a list sits: on the lines under the command line when they fit, over it
/// otherwise. It takes those lines whole, like text a terminal program printed there.
struct ListPlace {
    top: usize,
    rows: usize,
    first: usize,
    marker: usize,
    name: usize,
    by: usize,
    detail: usize,
    note: usize,
    note_width: usize,
    /// Where the question or the progress bar starts.
    after: usize,
}

impl ListPlace {
    const MOST_ROWS: usize = 8;

    fn place(overlay: &Overlay, columns: usize, screen_rows: usize) -> Option<Self> {
        if overlay.list.is_empty() || columns < 24 || screen_rows < 3 {
            return None;
        }
        let rows = overlay.list.len().min(Self::MOST_ROWS).min(screen_rows - 1);
        let first = overlay.selected.saturating_sub(rows - 1).min(overlay.list.len() - rows);
        let width = |pick: fn(&ListItem) -> &str| overlay.list.iter().map(|item| pick(item).chars().count()).max().unwrap_or(0);
        let marker = overlay.list_column.saturating_sub(2);
        let name = marker + 2;
        let by = name + width(|item| &item.name) + 3;
        let by_width = width(|item| &item.by);
        let detail = if by_width == 0 { by } else { by + by_width + 3 };
        let detail_width = width(|item| &item.detail);
        let note = if detail_width == 0 { detail } else { detail + detail_width + 3 };
        let note_width = width(|item| &item.note);
        let after = if note_width == 0 { note } else { note + note_width + 3 };
        let top = if overlay.row + 1 + rows <= screen_rows { overlay.row + 1 } else { overlay.row.checked_sub(rows)? };
        Some(Self { top, rows, first, marker, name, by, detail, note, note_width, after })
    }

    fn covers(&self, row: usize) -> bool {
        (self.top..self.top + self.rows).contains(&row)
    }
}

/// Glyphs placed by pixel, for text drawn over the cells.
struct Pen<'a> {
    glyphs: &'a mut GlyphCache,
    atlas: &'a mut Atlas,
    queue: &'a Queue,
    cell: (f32, f32),
    columns: usize,
}

impl Pen<'_> {
    /// `text` from `column` on the row whose top is `y`, cut at the right edge.
    fn write(&mut self, quads: &mut Vec<Quad>, y: f32, column: usize, text: &str, color: [u8; 3], bold: bool) -> Result<(), AtlasFull> {
        let (cw, ch) = self.cell;
        for (offset, character) in text.chars().enumerate() {
            let column = column + offset;
            if column >= self.columns {
                break;
            }
            if character == ' ' {
                continue;
            }
            let glyph = self.glyphs.get(character, bold, false, false, self.atlas, self.queue)?;
            if glyph.width > 0 && glyph.height > 0 {
                quads.push(Quad {
                    position: [column as f32 * cw + f32::from(glyph.left), y + ch - f32::from(glyph.top)],
                    size: [f32::from(glyph.width), f32::from(glyph.height)],
                    uv: glyph.uv,
                    color: [color[0], color[1], color[2], 255],
                });
            }
        }
        Ok(())
    }
}

pub const QUAD_ATTRIBUTES: [wgpu::VertexAttribute; 4] =
    wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Unorm8x4];

/// Fill `quads` with backgrounds, then glyphs, then decorations; returns where the
/// glyphs and the decorations start.
pub fn build(
    screen: &Screen,
    glyphs: &mut GlyphCache,
    atlas: &mut Atlas,
    queue: &Queue,
    quads: &mut Vec<Quad>,
    overlay: Option<&Overlay>,
    motion: Motion,
    cell_width: u32,
    cell_height: u32,
) -> Result<(u32, u32), AtlasFull> {
    let (cw, ch) = (cell_width as f32, cell_height as f32);
    let metrics = *glyphs.metrics();
    quads.clear();
    let mut text = Vec::new();
    let mut lines = Vec::new();
    let mut extras = screen.extras.iter().peekable();
    let list = overlay.and_then(|overlay| ListPlace::place(overlay, screen.columns, screen.rows));
    for row in 0..screen.rows {
        let y = row as f32 * ch;
        // Neighbouring cells with the same background share one rectangle.
        let mut run: Option<(usize, usize, [u8; 3])> = None;
        for (column, cell) in screen.row(row).iter().enumerate() {
            let index = row * screen.columns + column;
            let attached = extras.next_if(|(at, _)| *at == index).map(|(_, text)| text.as_str());
            // The list takes its lines whole.
            if list.as_ref().is_some_and(|list| list.covers(row)) {
                if let Some((start, end, color)) = run.take() {
                    quads.push(Quad::solid(start as f32 * cw, y, (end - start) as f32 * cw, ch, color));
                }
                continue;
            }
            if cell.flags & 64 != 0 {
                continue;
            }
            let mut style = cell_style(cell);
            if let Some(overlay) = overlay.filter(|overlay| overlay.row == row) {
                if overlay.risky.iter().any(|span| span.contains(&column)) {
                    style.foreground = overlay.risky_color;
                }
            }
            let cells = if cell.flags & 32 != 0 { 2 } else { 1 };
            let x = column as f32 * cw;
            let background = (!style.default_background).then_some(style.background);
            match (&mut run, background) {
                (Some((_, end, color)), Some(next)) if *end == column && *color == next => *end = column + cells,
                (current, next) => {
                    if let Some((start, end, color)) = current.take() {
                        quads.push(Quad::solid(start as f32 * cw, y, (end - start) as f32 * cw, ch, color));
                    }
                    *current = next.map(|color| (column, column + cells, color));
                }
            }
            if !style.hidden {
                let mut place = |character: char, zero_width: bool| -> Result<(), AtlasFull> {
                    if character == ' ' {
                        return Ok(());
                    }
                    let glyph = glyphs.get(character, style.bold, style.italic, zero_width, atlas, queue)?;
                    if glyph.width > 0 && glyph.height > 0 {
                        text.push(Quad {
                            position: [x + f32::from(glyph.left), y + ch - f32::from(glyph.top)],
                            size: [f32::from(glyph.width), f32::from(glyph.height)],
                            uv: glyph.uv,
                            color: [style.foreground[0], style.foreground[1], style.foreground[2], 255],
                        });
                    }
                    Ok(())
                };
                place(cell.c, false)?;
                for character in attached.unwrap_or("").chars() {
                    place(character, true)?;
                }
            }
            // Lines sit relative to the baseline, clamped inside the cell.
            let baseline = y + ch + metrics.descent;
            let mut line = |position: f32, thickness: f32| {
                let thickness = thickness.max(1.0).round();
                let top = (baseline - position - thickness / 2.0).round().min(y + ch - thickness);
                lines.push(Quad::solid(x, top, cells as f32 * cw, thickness, style.foreground));
            };
            if style.underline {
                line(metrics.underline_position, metrics.underline_thickness);
            }
            if style.strike {
                line(metrics.strikeout_position, metrics.strikeout_thickness);
            }
        }
        if let Some((start, end, color)) = run {
            quads.push(Quad::solid(start as f32 * cw, y, (end - start) as f32 * cw, ch, color));
        }
    }
    // The suggestion continues the command where the cursor is; the hint sits at the
    // right end of the line when it fits.
    if let Some(overlay) = overlay.filter(|overlay| overlay.row < screen.rows) {
        let y = overlay.row as f32 * ch;
        let hint_column = screen.columns.saturating_sub(overlay.hint.chars().count() + 1);
        let ghost_end = overlay.column + overlay.ghost.chars().count();
        let mut runs = vec![(overlay.column, overlay.ghost.as_str())];
        if !overlay.hint.is_empty() && ghost_end + 2 <= hint_column {
            runs.push((hint_column, overlay.hint.as_str()));
        }
        for (start, run) in runs {
            for (offset, character) in run.chars().enumerate() {
                let column = start + offset;
                if column >= screen.columns {
                    break;
                }
                if character == ' ' {
                    continue;
                }
                let glyph = glyphs.get(character, false, false, false, atlas, queue)?;
                if glyph.width > 0 && glyph.height > 0 {
                    text.push(Quad {
                        position: [column as f32 * cw + f32::from(glyph.left), y + ch - f32::from(glyph.top)],
                        size: [f32::from(glyph.width), f32::from(glyph.height)],
                        uv: glyph.uv,
                        color: [overlay.color[0], overlay.color[1], overlay.color[2], 255],
                    });
                }
            }
        }
    }
    if let (Some(list), Some(overlay)) = (&list, overlay) {
        let [selected_color, name_color, fill_color] = overlay.list_colors;
        let mut pen = Pen { glyphs, atlas, queue, cell: (cw, ch), columns: screen.columns };
        for (index, item) in overlay.list.iter().enumerate().skip(list.first).take(list.rows) {
            let y = (list.top + index - list.first) as f32 * ch;
            let chosen = index == overlay.selected;
            pen.write(&mut text, y, list.name, &item.name, if chosen { selected_color } else { name_color }, false)?;
            pen.write(&mut text, y, list.by, &item.by, name_color, false)?;
            pen.write(&mut text, y, list.detail, &item.detail, overlay.color, false)?;
            let note = list.note + list.note_width - item.note.chars().count();
            pen.write(&mut text, y, note, &item.note, overlay.color, false)?;
        }
        // The marker, between rows while it glides.
        let at = motion.marker.unwrap_or(overlay.selected as f32) - list.first as f32;
        let y = (list.top as f32 + at.clamp(0.0, (list.rows - 1) as f32)) * ch;
        pen.write(&mut text, y.round(), list.marker, &crate::command_line::PROMPT.to_string(), selected_color, true)?;
        let y = (list.top + overlay.selected.saturating_sub(list.first)) as f32 * ch;
        match overlay.progress {
            Some(progress) => {
                // A hairline track with a thicker fill, then the percentage.
                let value = progress.value(motion.now);
                let cells = 24.min(screen.columns.saturating_sub(list.after + 6));
                if cells >= 6 {
                    let (left, width) = (list.after as f32 * cw, cells as f32 * cw);
                    let track = (ch / 18.0).round().max(1.0);
                    let fill = track * 2.0;
                    let middle = (y + ch * 0.55).round();
                    lines.push(Quad::solid(left, middle - (track / 2.0).floor(), width, track, overlay.color));
                    lines.push(Quad::solid(left, middle - (fill / 2.0).floor(), (width * value).round(), fill, fill_color));
                }
                let percent = format!("{}%", (value * 100.0).floor() as u32);
                pen.write(&mut text, y, list.after + cells + 2, &percent, overlay.color, false)?;
            }
            None => pen.write(&mut text, y, list.after, &overlay.question, overlay.question_color, false)?,
        }
    }
    let glyphs_from = quads.len() as u32;
    quads.extend(text);
    let decorations_from = quads.len() as u32;
    quads.extend(lines);
    Ok((glyphs_from, decorations_from))
}
