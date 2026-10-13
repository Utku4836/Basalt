//! Derive the PTY grid from the actual pane body, rather than forcing hidden cells.
use crate::split::Rect;
use basalt_core::terminal::TerminalSize;

/// Logical body container inside the pane's frame (ui/terminal-pane.slint): 12px in
/// from every side. The renderer keeps another 2px on each horizontal side, as
/// terminal_size_with_header accounts for.
pub fn panel_inner_rect(pane: Rect) -> Rect {
    Rect {
        x: pane.x + 12.0,
        y: pane.y + 12.0,
        width: pane.width - 24.0,
        height: pane.height - 24.0,
    }
}

pub fn terminal_size(
    pane: Rect,
    cell_width: f64,
    cell_height: f64,
    scale: f64,
) -> Option<TerminalSize> {
    terminal_size_with_header(pane, cell_width, cell_height, scale, 28.0)
}
pub fn terminal_size_with_header(
    pane: Rect,
    cell_width: f64,
    cell_height: f64,
    scale: f64,
    header: f64,
) -> Option<TerminalSize> {
    if !header.is_finite() || header < 0.0 {
        return None;
    }
    if ![pane.width, pane.height, cell_width, cell_height, scale]
        .into_iter()
        .all(|v| v.is_finite() && v > 0.0)
    {
        return None;
    }
    let pixel_width = (cell_width * scale).ceil();
    let pixel_height = (cell_height * scale).ceil();
    if pixel_width > 16384.0 || pixel_height > 16384.0 {
        return None;
    }
    let columns = ((pane.width - 4.0) / cell_width)
        .floor()
        .min(1000.0)
        .min((16384.0 / pixel_width).floor());
    let rows = ((pane.height - header) / cell_height)
        .floor()
        .min(500.0)
        .min((16384.0 / pixel_height).floor());
    if columns < 2.0 || rows < 1.0 {
        return None;
    }
    TerminalSize::new(
        columns as u16,
        rows as u16,
        pixel_width.max(1.0) as u16,
        pixel_height.max(1.0) as u16,
    )
    .ok()
}
