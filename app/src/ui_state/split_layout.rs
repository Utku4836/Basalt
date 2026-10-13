//! Split tree of panes: layout, divider hit testing and ratio updates.
use crate::tabs::Pane;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MINIMUM_PANE_SIZE: f64 = 100.0;
pub const DIVIDER_THICKNESS: f64 = 1.0;
// Dividers are 1px; the pointer target extends this far on each side.
pub const DIVIDER_HIT_EXPANSION: f64 = 8.0;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Orientation {
    Horizontal,
    Vertical,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
impl Rect {
    pub const UNIT: Self = Self {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    };
    fn valid(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .into_iter()
            .all(f64::is_finite)
            && self.width >= 0.0
            && self.height >= 0.0
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Branch {
    pub id: Uuid,
    pub orientation: Orientation,
    pub first: Box<SplitNode>,
    pub second: Box<SplitNode>,
    pub position: f64,
    pub imposed_first_extent: Option<f64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SplitNode {
    Pane(Pane),
    Split(Branch),
}
#[derive(Debug, Clone, PartialEq)]
pub struct PaneFrame {
    pub id: Uuid,
    pub bounds: Rect,
}
#[derive(Debug, Clone, PartialEq)]
pub struct DividerFrame {
    pub id: Uuid,
    pub orientation: Orientation,
    pub bounds: Rect,
}
#[derive(Debug, Default, Clone)]
pub struct Layout {
    pub panes: Vec<PaneFrame>,
    pub dividers: Vec<DividerFrame>,
}

impl SplitNode {
    pub fn auto_layout_column_sizes(count: usize) -> Vec<usize> {
        if count == 0 {
            return vec![];
        }
        Self::balanced_columns(count, ((count as f64).sqrt().round() as usize).max(2))
    }
    pub fn linear_layout(panes: Vec<Pane>, orientation: Orientation) -> Option<Self> {
        (!panes.is_empty())
            .then(|| Self::equal_split(panes.into_iter().map(Self::Pane).collect(), orientation))
    }
    pub fn auto_layout(panes: Vec<Pane>) -> Option<Self> {
        let sizes = Self::auto_layout_column_sizes(panes.len());
        Self::tile_columns(panes, sizes)
    }
    pub fn auto_layout_at(panes: Vec<Pane>, viewport: Rect) -> Option<Self> {
        let aspect = (viewport.width / viewport.height.max(1.0)).clamp(0.25, 4.0);
        let columns = ((panes.len() as f64 * aspect / 1.6).sqrt().round() as usize)
            .max(if aspect >= 1.0 { 2 } else { 1 });
        let sizes = Self::balanced_columns(panes.len(), columns);
        Self::tile_columns(panes, sizes)
    }
    fn balanced_columns(count: usize, columns: usize) -> Vec<usize> {
        if count == 0 {
            return vec![];
        }
        let columns = columns.clamp(1, count);
        (0..columns)
            .map(|i| count / columns + usize::from(i < count % columns))
            .collect()
    }
    fn tile_columns(panes: Vec<Pane>, sizes: Vec<usize>) -> Option<Self> {
        if panes.is_empty() {
            return None;
        }
        let mut panes = panes.into_iter();
        let mut columns = Vec::new();
        for size in &sizes {
            columns.push(Self::equal_split(
                panes.by_ref().take(*size).map(Self::Pane).collect(),
                Orientation::Vertical,
            ));
        }
        Some(Self::weighted_columns(columns, &sizes))
    }
    fn weighted_columns(mut nodes: Vec<Self>, weights: &[usize]) -> Self {
        if nodes.len() == 1 {
            return nodes.remove(0);
        }
        let position = weights[0] as f64 / weights.iter().sum::<usize>() as f64;
        let first = nodes.remove(0);
        let second = Self::weighted_columns(nodes, &weights[1..]);
        Self::Split(Branch {
            id: Uuid::new_v4(),
            orientation: Orientation::Horizontal,
            first: Box::new(first),
            second: Box::new(second),
            position,
            imposed_first_extent: None,
        })
    }
    fn equal_split(mut nodes: Vec<Self>, orientation: Orientation) -> Self {
        if nodes.len() == 1 {
            return nodes.remove(0);
        }
        let position = 1.0 / nodes.len() as f64;
        let first = nodes.remove(0);
        let second = Self::equal_split(nodes, orientation);
        Self::Split(Branch {
            id: Uuid::new_v4(),
            orientation,
            first: Box::new(first),
            second: Box::new(second),
            position,
            imposed_first_extent: None,
        })
    }
    pub fn id(&self) -> Uuid {
        match self {
            Self::Pane(pane) => pane.id,
            Self::Split(branch) => branch.id,
        }
    }
    pub fn pane(&self, id: Uuid) -> Option<&Pane> {
        match self {
            Self::Pane(pane) => (pane.id == id).then_some(pane),
            Self::Split(branch) => branch.first.pane(id).or_else(|| branch.second.pane(id)),
        }
    }
    pub fn pane_mut(&mut self, id: Uuid) -> Option<&mut Pane> {
        match self {
            Self::Pane(pane) => (pane.id == id).then_some(pane),
            Self::Split(branch) => match branch.first.pane_mut(id) {
                Some(pane) => Some(pane),
                None => branch.second.pane_mut(id),
            },
        }
    }
    pub fn pane_ids(&self) -> Vec<Uuid> {
        match self {
            Self::Pane(pane) => vec![pane.id],
            Self::Split(branch) => {
                let mut ids = branch.first.pane_ids();
                ids.extend(branch.second.pane_ids());
                ids
            }
        }
    }
    pub fn split(
        &mut self,
        target: Uuid,
        new: Pane,
        orientation: Orientation,
        insert_first: bool,
    ) -> bool {
        if self.pane(new.id).is_some() || self.pane(target).is_none() {
            return false;
        }
        self.split_leaf(target, new, orientation, insert_first)
    }
    fn split_leaf(
        &mut self,
        target: Uuid,
        new: Pane,
        orientation: Orientation,
        insert_first: bool,
    ) -> bool {
        match self {
            Self::Pane(pane) if pane.id == target => {
                let old = self.clone();
                let new = Self::Pane(new);
                let (first, second) = if insert_first { (new, old) } else { (old, new) };
                *self = Self::Split(Branch {
                    id: Uuid::new_v4(),
                    orientation,
                    first: Box::new(first),
                    second: Box::new(second),
                    position: 0.5,
                    imposed_first_extent: None,
                });
                true
            }
            Self::Pane(_) => false,
            Self::Split(branch) => {
                if branch.first.pane(target).is_some() {
                    branch
                        .first
                        .split_leaf(target, new, orientation, insert_first)
                } else {
                    branch
                        .second
                        .split_leaf(target, new, orientation, insert_first)
                }
            }
        }
    }
    pub fn remove(self, target: Uuid) -> Option<Self> {
        match self {
            Self::Pane(pane) => {
                if pane.id == target {
                    None
                } else {
                    Some(Self::Pane(pane))
                }
            }
            Self::Split(mut branch) => {
                let first = (*branch.first).remove(target);
                let second = (*branch.second).remove(target);
                match (first, second) {
                    (Some(first), Some(second)) => {
                        branch.first = Box::new(first);
                        branch.second = Box::new(second);
                        Some(Self::Split(branch))
                    }
                    (Some(remaining), None) | (None, Some(remaining)) => Some(remaining),
                    (None, None) => None,
                }
            }
        }
    }
    pub fn divider_mut(&mut self, id: Uuid) -> Option<&mut Branch> {
        match self {
            Self::Split(branch) => {
                if branch.id == id {
                    return Some(branch);
                }
                match branch.first.divider_mut(id) {
                    Some(found) => Some(found),
                    None => branch.second.divider_mut(id),
                }
            }
            Self::Pane(_) => None,
        }
    }
    pub fn normalized_bounds(&self) -> Vec<PaneFrame> {
        let mut frames = Vec::new();
        self.normalized_in(Rect::UNIT, &mut frames);
        frames
    }
    fn normalized_in(&self, rect: Rect, frames: &mut Vec<PaneFrame>) {
        match self {
            Self::Pane(pane) => frames.push(PaneFrame {
                id: pane.id,
                bounds: rect,
            }),
            Self::Split(branch) => {
                let (first, _, second) = partition(
                    rect,
                    branch.orientation,
                    extent(rect, branch.orientation) * branch.position,
                    0.0,
                );
                branch.first.normalized_in(first, frames);
                branch.second.normalized_in(second, frames);
            }
        }
    }
    pub fn layout(&self, rect: Rect, scale: f64) -> Option<Layout> {
        if !rect.valid() || !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        let mut layout = Layout::default();
        self.pixels_in(rect, scale, &mut layout)?;
        Some(layout)
    }
    fn minimum_extent(&self, axis: Orientation) -> f64 {
        match self {
            Self::Pane(_) => MINIMUM_PANE_SIZE,
            Self::Split(branch) => {
                let first = branch.first.minimum_extent(axis);
                let second = branch.second.minimum_extent(axis);
                if branch.orientation == axis {
                    first + second + DIVIDER_THICKNESS
                } else {
                    first.max(second)
                }
            }
        }
    }
    fn pixels_in(&self, rect: Rect, scale: f64, layout: &mut Layout) -> Option<()> {
        match self {
            Self::Pane(pane) => layout.panes.push(PaneFrame {
                id: pane.id,
                bounds: rect,
            }),
            Self::Split(branch) => {
                if !branch.position.is_finite() || branch.position <= 0.0 || branch.position >= 1.0
                {
                    return None;
                }
                let thickness = DIVIDER_THICKNESS.min(extent(rect, branch.orientation));
                let available = (extent(rect, branch.orientation) - thickness).max(0.0);
                let first_min = branch.first.minimum_extent(branch.orientation);
                let second_min = branch.second.minimum_extent(branch.orientation);
                let fitting_scale = (available / (first_min + second_min)).min(1.0);
                let lower = first_min * fitting_scale;
                let upper = available - second_min * fitting_scale;
                let requested = if let Some(imposed) = branch.imposed_first_extent {
                    if !imposed.is_finite() {
                        return None;
                    }
                    imposed
                } else {
                    (available * branch.position * scale).round() / scale
                };
                let position = if lower <= upper {
                    requested.clamp(lower, upper)
                } else {
                    available * first_min / (first_min + second_min)
                };
                let (first, divider, second) =
                    partition(rect, branch.orientation, position, thickness);
                layout.dividers.push(DividerFrame {
                    id: branch.id,
                    orientation: branch.orientation,
                    bounds: divider,
                });
                branch.first.pixels_in(first, scale, layout)?;
                branch.second.pixels_in(second, scale, layout)?;
            }
        }
        Some(())
    }
}
impl PartialEq for SplitNode {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}
fn extent(rect: Rect, orientation: Orientation) -> f64 {
    match orientation {
        Orientation::Horizontal => rect.width,
        Orientation::Vertical => rect.height,
    }
}
fn partition(
    rect: Rect,
    orientation: Orientation,
    position: f64,
    thickness: f64,
) -> (Rect, Rect, Rect) {
    match orientation {
        Orientation::Horizontal => (
            Rect {
                width: position,
                ..rect
            },
            Rect {
                x: rect.x + position,
                width: thickness,
                ..rect
            },
            Rect {
                x: rect.x + position + thickness,
                width: (rect.width - position - thickness).max(0.0),
                ..rect
            },
        ),
        Orientation::Vertical => (
            Rect {
                height: position,
                ..rect
            },
            Rect {
                y: rect.y + position,
                height: thickness,
                ..rect
            },
            Rect {
                y: rect.y + position + thickness,
                height: (rect.height - position - thickness).max(0.0),
                ..rect
            },
        ),
    }
}
