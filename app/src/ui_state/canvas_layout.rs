//! Independent canvas groups/frames; terminal tabs remain owned by the split tree.
use crate::split::{Layout, PaneFrame, Rect};
use uuid::Uuid;

pub const GAP: f64 = 16.0;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum LayoutMode {
    #[default]
    Splits,
    Canvas,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CanvasPane {
    pub id: Uuid,
    pub frame: Rect,
    pub panels: Vec<Uuid>,
    pub selected: Uuid,
}
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Canvas {
    pub panes: Vec<CanvasPane>,
    pub focused: Option<Uuid>,
    pub offset: (f64, f64),
    pub viewport_size: Option<(f64, f64)>,
}
impl Canvas {
    /// User-requested responsive sizing. Preserve relative placement/identities,
    /// while real cell metrics determine new terminal grids (never scale text).
    pub fn resize_viewport(&mut self, viewport: Rect) {
        if !viewport.width.is_finite()
            || !viewport.height.is_finite()
            || viewport.width <= 0.0
            || viewport.height <= 0.0
        {
            return;
        }
        if let Some((width, height)) = self.viewport_size {
            let x = viewport.width / width;
            let y = viewport.height / height;
            for pane in &mut self.panes {
                pane.frame.x *= x;
                pane.frame.y *= y;
                pane.frame.width *= x;
                pane.frame.height *= y;
            }
            self.offset.0 *= x;
            self.offset.1 *= y;
        }
        self.viewport_size = Some((viewport.width, viewport.height));
    }
    pub fn pan_by(&mut self, x: f64, y: f64) {
        if x.is_finite() && y.is_finite() {
            self.offset.0 += x;
            self.offset.1 += y;
        }
    }
    pub fn pane(&self, id: Uuid) -> Option<&CanvasPane> {
        self.panes.iter().find(|pane| pane.id == id)
    }
    pub fn pane_mut(&mut self, id: Uuid) -> Option<&mut CanvasPane> {
        self.panes.iter_mut().find(|pane| pane.id == id)
    }
    pub fn insert(&mut self, panel: Uuid, frame: Rect) {
        if self.panes.iter().any(|pane| pane.panels.contains(&panel)) {
            return;
        }
        self.panes.push(CanvasPane {
            id: panel,
            frame,
            panels: vec![panel],
            selected: panel,
        });
    }
    /// Try right/down/left/up, bounded outward scan, then right of all content.
    /// Existing panes are never shifted to make space, matching CanvasPlacer.
    pub fn place(&self, anchor: Option<Rect>, width: f64, height: f64) -> Rect {
        let width = width.max(200.0);
        let height = height.max(120.0);
        let make = |x, y| Rect {
            x,
            y,
            width,
            height,
        };
        let fallback = || {
            make(
                self.panes
                    .iter()
                    .map(|p| p.frame.x + p.frame.width)
                    .reduce(f64::max)
                    .unwrap_or(-GAP)
                    + GAP,
                self.panes
                    .iter()
                    .map(|p| p.frame.y)
                    .reduce(f64::min)
                    .unwrap_or(0.0),
            )
        };
        let Some(anchor) = anchor else {
            return fallback();
        };
        for candidate in [
            make(anchor.x + anchor.width + GAP, anchor.y),
            make(anchor.x, anchor.y + anchor.height + GAP),
            make(anchor.x - GAP - width, anchor.y),
            make(anchor.x, anchor.y - GAP - height),
        ] {
            if self.is_free(candidate) {
                return candidate;
            }
        }
        for row in 0..8 {
            for column in 0..16 {
                let candidate = make(
                    anchor.x + anchor.width + GAP + f64::from(column) * (width + GAP),
                    anchor.y + f64::from(row) * (height + GAP),
                );
                if self.is_free(candidate) {
                    return candidate;
                }
            }
        }
        fallback()
    }
    fn is_free(&self, candidate: Rect) -> bool {
        let padding = GAP - 0.5;
        !self.panes.iter().any(|pane| {
            let other = pane.frame;
            candidate.x - padding < other.x + other.width
                && candidate.x + candidate.width + padding > other.x
                && candidate.y - padding < other.y + other.height
                && candidate.y + candidate.height + padding > other.y
        })
    }
    pub fn remove(&mut self, panel: Uuid) {
        let Some(index) = self
            .panes
            .iter()
            .position(|pane| pane.panels.contains(&panel))
        else {
            return;
        };
        let pane = &mut self.panes[index];
        let position = pane.panels.iter().position(|id| *id == panel).unwrap();
        pane.panels.remove(position);
        if pane.panels.is_empty() {
            let removed = self.panes.remove(index);
            if self.focused == Some(removed.id) {
                self.focused = self
                    .panes
                    .get(index.min(self.panes.len().saturating_sub(1)))
                    .map(|pane| pane.id);
            }
        } else if pane.selected == panel {
            pane.selected = pane.panels[position.min(pane.panels.len() - 1)];
        }
    }
    pub fn reveal(&mut self, id: Uuid, viewport: Rect) {
        let Some(frame) = self.pane(id).map(|pane| pane.frame) else {
            return;
        };
        if frame.x < self.offset.0 {
            self.offset.0 = frame.x;
        } else if frame.x + frame.width > self.offset.0 + viewport.width {
            self.offset.0 = frame.x + frame.width - viewport.width;
        }
        if frame.y < self.offset.1 {
            self.offset.1 = frame.y;
        } else if frame.y + frame.height > self.offset.1 + viewport.height {
            self.offset.1 = frame.y + frame.height - viewport.height;
        }
    }
    pub fn layout(&self, viewport: Rect) -> Option<Layout> {
        if ![
            viewport.width,
            viewport.height,
            self.offset.0,
            self.offset.1,
        ]
        .into_iter()
        .all(f64::is_finite)
        {
            return None;
        }
        Some(Layout {
            panes: self
                .panes
                .iter()
                .map(|pane| PaneFrame {
                    id: pane.id,
                    bounds: Rect {
                        x: viewport.x + pane.frame.x - self.offset.0,
                        y: viewport.y + pane.frame.y - self.offset.1,
                        ..pane.frame
                    },
                })
                .collect(),
            dividers: vec![],
        })
    }
}
