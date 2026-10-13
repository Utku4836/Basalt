//! Window state: workspaces, the Dock and their pane trees.
use crate::{
    canvas::{Canvas, LayoutMode},
    split::{Layout, Orientation, Rect, SplitNode},
    tabs::{Pane, Tab},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DropZone {
    Tab(usize),
    Center,
    Left,
    Right,
    Top,
    Bottom,
}
// Drop zones: horizontal edges win at the corners.
pub fn drop_zone(x: f64, y: f64, width: f64, height: f64) -> DropZone {
    let horizontal = (width * 0.25).max(80.0);
    let vertical = (height * 0.25).max(80.0);
    if x < horizontal {
        DropZone::Left
    } else if x > width - horizontal {
        DropZone::Right
    } else if y < vertical {
        DropZone::Top
    } else if y > height - vertical {
        DropZone::Bottom
    } else {
        DropZone::Center
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Workspace {
    pub id: Uuid,
    pub title: String,
    pub tree: SplitNode,
    pub focused: Uuid,
    pub layout_mode: LayoutMode,
    pub canvas: Canvas,
    pub zoomed: Option<Uuid>,
    #[serde(default)]
    pub color_palette: Option<i32>,
    #[serde(default)]
    pub font_percent: Option<i32>,
    #[serde(default)]
    pub repository_path: Option<std::path::PathBuf>,
    /// The user named this session; otherwise its title follows the first terminal.
    #[serde(default)]
    pub renamed: bool,
    /// Unix seconds of the last time this session was opened or launched into.
    #[serde(default)]
    pub last_active: u64,
    /// Stable session number, shown as "Session #N" until a real title arrives.
    #[serde(default)]
    pub number: usize,
    /// The session profile its new terminals start with; None is the user's own setup.
    #[serde(default)]
    pub profile: Option<Uuid>,
    pane_order: Vec<Uuid>,
    #[serde(default)]
    layout_backup: Option<SplitNode>,
    #[serde(default)]
    layout_backup_mode: Option<LayoutMode>,
}
impl Workspace {
    pub fn with_terminal(number: usize) -> Self {
        let pane = Pane::new(vec![Tab::terminal("Terminal")]);
        Self {
            id: Uuid::new_v4(),
            title: format!("Workspace {number}"),
            focused: pane.id,
            layout_mode: LayoutMode::Splits,
            canvas: Canvas::default(),
            zoomed: None,
            color_palette: None,
            font_percent: None,
            repository_path: None,
            renamed: false,
            last_active: 0,
            number,
            profile: None,
            pane_order: vec![pane.id],
            layout_backup: None,
            layout_backup_mode: None,
            tree: SplitNode::Pane(pane),
        }
    }
    pub fn new(number: usize) -> Self {
        let mut workspace = Self::with_terminal(number);
        let pane = workspace.tree.pane_mut(workspace.focused).unwrap();
        pane.tabs.clear();
        pane.selected = None;
        workspace
    }
    /// Sidebar title: the user's name, else what the first terminal reports
    /// (agent CLIs title the terminal with their current task), else the tool.
    pub fn session_title(&self) -> String {
        if self.renamed {
            return self.title.clone();
        }
        self.auto_title().unwrap_or_else(|| self.placeholder_title())
    }
    /// "Session #N": the name a session shows until its terminal reports a real title.
    pub fn placeholder_title(&self) -> String {
        format!("Session #{}", self.number.max(1))
    }
    /// What the first terminal reports about its task, unless that is only the name of
    /// a shell, a program or a path.
    pub fn auto_title(&self) -> Option<String> {
        let tab = self
            .tree
            .pane_ids()
            .into_iter()
            .filter_map(|id| self.tree.pane(id))
            .find_map(|pane| pane.tabs.first())?;
        let runtime = tab.runtime_title.as_deref()?;
        let task = runtime.trim_start_matches(|ch: char| !ch.is_alphanumeric()).trim();
        let lower = task.to_lowercase();
        let lower = lower.strip_prefix("administrator: ").unwrap_or(&lower);
        let generic = task.is_empty()
            || lower.contains(".exe")
            || lower.contains('\\')
            || lower.contains('/')
            || tab.custom_title.as_deref().is_some_and(|tool| tool.eq_ignore_ascii_case(task))
            || matches!(
                lower,
                "claude" | "claude code" | "codex" | "terminal" | "cmd" | "command prompt"
                    | "powershell" | "windows powershell" | "pwsh" | "lazygit"
            );
        (!generic).then(|| task.chars().take(60).collect())
    }
    pub fn is_empty(&self) -> bool {
        self.tree
            .pane_ids()
            .iter()
            .all(|id| self.tree.pane(*id).unwrap().tabs.is_empty())
    }
    pub fn selected_tab(&self) -> Option<Uuid> {
        if self.layout_mode == LayoutMode::Canvas {
            return Some(self.canvas.pane(self.canvas.focused?)?.selected);
        }
        self.tree.pane(self.focused)?.selected
    }
    pub fn focused_view_pane(&self) -> Uuid {
        self.canvas
            .focused
            .filter(|_| self.layout_mode == LayoutMode::Canvas)
            .unwrap_or(self.focused)
    }
    pub fn owns_pane(&self, id: Uuid) -> bool {
        if self.layout_mode == LayoutMode::Canvas {
            self.canvas.pane(id).is_some()
        } else {
            self.tree.pane(id).is_some()
        }
    }
    pub fn base_pane_for_tab(&self, tab: Uuid) -> Option<Uuid> {
        self.tree.pane_ids().into_iter().find(|id| {
            self.tree
                .pane(*id)
                .unwrap()
                .tabs
                .iter()
                .any(|item| item.id == tab)
        })
    }
    pub fn projected_pane(&self, id: Uuid) -> Option<Pane> {
        if self.layout_mode == LayoutMode::Splits {
            return self.tree.pane(id).cloned();
        }
        let group = self.canvas.pane(id)?;
        let tabs = group
            .panels
            .iter()
            .filter_map(|tab| {
                self.tree
                    .pane(self.base_pane_for_tab(*tab)?)
                    .unwrap()
                    .tabs
                    .iter()
                    .find(|item| item.id == *tab)
                    .cloned()
            })
            .collect();
        Some(Pane {
            id: group.id,
            tabs,
            selected: Some(group.selected),
            full_width_tab: false,
        })
    }
    pub fn focus_view_pane(&mut self, id: Uuid) -> bool {
        let Some(pane) = self.projected_pane(id) else {
            return false;
        };
        if self.layout_mode == LayoutMode::Canvas {
            let Some(selected) = pane.selected else {
                return false;
            };
            let Some(base) = self.base_pane_for_tab(selected) else {
                return false;
            };
            self.tree.pane_mut(base).unwrap().select(selected);
            self.focused = base;
            self.canvas.focused = Some(id);
        } else {
            self.focused = id;
        }
        true
    }
    pub fn select_view_tab(&mut self, pane: Uuid, tab: Uuid) -> bool {
        if self.layout_mode == LayoutMode::Canvas {
            let Some(group) = self.canvas.pane_mut(pane) else {
                return false;
            };
            if !group.panels.contains(&tab) {
                return false;
            }
            group.selected = tab;
        } else if !self
            .tree
            .pane_mut(pane)
            .is_some_and(|pane| pane.select(tab))
        {
            return false;
        }
        self.focus_view_pane(pane)
    }
    pub fn set_layout_mode(&mut self, mode: LayoutMode, viewport: Rect) -> bool {
        if mode == self.layout_mode {
            return false;
        }
        if mode == LayoutMode::Canvas {
            let Some(layout) = self.tree.layout(viewport, 1.0) else {
                return false;
            };
            let selected = self.selected_tab();
            let mut canvas = Canvas::default();
            canvas.resize_viewport(viewport);
            for frame in &layout.panes {
                let pane = self.tree.pane(frame.id).unwrap();
                if let Some(tab) = pane.selected {
                    canvas.insert(
                        tab,
                        Rect {
                            x: frame.bounds.x - viewport.x,
                            y: frame.bounds.y - viewport.y,
                            ..frame.bounds
                        },
                    );
                }
            }
            for frame in layout.panes {
                let pane = self.tree.pane(frame.id).unwrap();
                for tab in &pane.tabs {
                    if Some(tab.id) != pane.selected {
                        let anchor = pane
                            .selected
                            .and_then(|id| canvas.pane(id))
                            .map(|pane| pane.frame);
                        let bounds = canvas.place(anchor, frame.bounds.width, frame.bounds.height);
                        canvas.insert(tab.id, bounds);
                    }
                }
            }
            canvas.focused = selected;
            self.canvas = canvas;
        }
        self.layout_mode = mode;
        true
    }
    pub fn layout(&self, viewport: Rect, scale: f64) -> Option<Layout> {
        if let Some(id) = self.zoomed.filter(|id| self.owns_pane(*id)) {
            return Some(Layout {
                panes: vec![crate::split::PaneFrame {
                    id,
                    bounds: viewport,
                }],
                dividers: vec![],
            });
        }
        if self.layout_mode == LayoutMode::Canvas {
            self.canvas.layout(viewport)
        } else {
            self.tree.layout(viewport, scale)
        }
    }
    fn layout_pane_count(&self) -> usize {
        if self.layout_mode == LayoutMode::Canvas {
            self.canvas.panes.len()
        } else {
            self.tree.pane_ids().len()
        }
    }
    pub fn toggle_zoom(&mut self, pane: Uuid) -> bool {
        if !self.focus_view_pane(pane) {
            return false;
        }
        // A lone pane already fills the workspace.
        if self.zoomed.is_none() && self.layout_pane_count() < 2 {
            return false;
        }
        self.zoomed = if self.zoomed == Some(pane) {
            None
        } else {
            Some(pane)
        };
        true
    }
    pub fn add_canvas_terminal(
        &mut self,
        pane: Uuid,
        viewport: Rect,
        separate: bool,
    ) -> Option<Uuid> {
        let anchor = self.canvas.pane(pane)?.clone();
        let base = self.base_pane_for_tab(anchor.selected)?;
        let tab = Tab::terminal("Terminal");
        let id = tab.id;
        let bounds = self
            .canvas
            .place(Some(anchor.frame), anchor.frame.width, anchor.frame.height);
        self.tree.pane_mut(base)?.add(tab, true);
        self.focused = base;
        let target = if separate {
            self.canvas.insert(id, bounds);
            id
        } else {
            let group = self.canvas.pane_mut(pane).unwrap();
            group.panels.push(id);
            group.selected = id;
            pane
        };
        self.canvas.focused = Some(target);
        self.canvas.reveal(target, viewport);
        Some(id)
    }
    pub fn add_terminal(&mut self, pane: Uuid, split: Option<Orientation>) -> Option<Uuid> {
        self.tree.pane(pane)?;
        let tab = Tab::terminal("Terminal");
        let id = tab.id;
        if let Some(orientation) = split {
            let new = Pane::new(vec![tab]);
            self.focused = new.id;
            self.pane_order.push(new.id);
            self.tree.split(pane, new, orientation, false);
        } else {
            self.tree.pane_mut(pane)?.add(tab, true);
            self.focused = pane;
        }
        Some(id)
    }
    /// Add a terminal by splitting the focused pane (see add_auto_terminal_at).
    pub fn add_auto_terminal(&mut self) -> Uuid {
        self.add_auto_terminal_at(Rect {
            x: 0.0,
            y: 0.0,
            width: 1400.0,
            height: 900.0,
        })
    }
    pub fn add_auto_terminal_at(&mut self, viewport: Rect) -> Uuid {
        self.zoomed = None;
        let tab = Tab::terminal("Terminal");
        let id = tab.id;
        if self.is_empty() {
            self.layout_mode = LayoutMode::Splits;
            let pane_id = self.tree.pane_ids()[0];
            self.tree.pane_mut(pane_id).unwrap().add(tab, true);
            self.focused = pane_id;
            return id;
        }
        // Split only the focused pane, along its longer side: every other pane keeps
        // its place and size, so opening a terminal never reshuffles the session.
        let target = if self.tree.pane(self.focused).is_some() {
            self.focused
        } else {
            self.tree.pane_ids()[0]
        };
        let orientation = self
            .tree
            .layout(viewport, 1.0)
            .and_then(|layout| layout.panes.into_iter().find(|frame| frame.id == target))
            .map_or(Orientation::Horizontal, |frame| {
                if frame.bounds.width >= frame.bounds.height {
                    Orientation::Horizontal
                } else {
                    Orientation::Vertical
                }
            });
        let pane = Pane::new(vec![tab]);
        self.focused = pane.id;
        self.pane_order.retain(|id| self.tree.pane(*id).is_some());
        self.pane_order.push(pane.id);
        self.tree.split(target, pane, orientation, false);
        id
    }
    /// Arrange existing panes without replacing terminals or their selection.
    pub fn arrange(&mut self, orientation: Option<Orientation>, restore: bool, viewport: Rect) {
        if self.is_empty() {
            return;
        }
        let mut panes = self
            .tree
            .pane_ids()
            .into_iter()
            .map(|id| self.tree.pane(id).unwrap().clone())
            .collect::<Vec<_>>();
        self.pane_order
            .retain(|id| panes.iter().any(|pane| pane.id == *id));
        panes.sort_by_key(|pane| {
            self.pane_order
                .iter()
                .position(|id| *id == pane.id)
                .unwrap_or(usize::MAX)
        });
        let mut restore_mode = if restore {
            self.layout_backup_mode.take().unwrap_or(LayoutMode::Splits)
        } else {
            LayoutMode::Splits
        };
        if restore {
            if let Some(mut backup) = self.layout_backup.take() {
                if backup.pane_ids().len() == panes.len()
                    && panes.iter().all(|pane| backup.pane(pane.id).is_some())
                {
                    for pane in panes {
                        let id = pane.id;
                        *backup.pane_mut(id).unwrap() = pane;
                    }
                    self.tree = backup;
                } else {
                    self.tree = SplitNode::auto_layout_at(panes, viewport).unwrap();
                }
            } else {
                self.tree = SplitNode::auto_layout_at(panes, viewport).unwrap();
            }
        } else {
            if self.layout_backup.is_none() {
                self.layout_backup = Some(self.tree.clone());
                self.layout_backup_mode = Some(self.layout_mode);
            }
            self.tree = match orientation {
                Some(direction) => SplitNode::linear_layout(panes, direction).unwrap(),
                None => SplitNode::auto_layout_at(panes, viewport).unwrap(),
            };
        }
        if restore_mode == LayoutMode::Canvas
            && self.tree.pane_ids().iter().any(|id| {
                self.tree.pane(*id).unwrap().tabs.iter().any(|tab| {
                    !self
                        .canvas
                        .panes
                        .iter()
                        .any(|pane| pane.panels.contains(&tab.id))
                })
            })
        {
            restore_mode = LayoutMode::Splits;
        }
        self.layout_mode = restore_mode;
        self.zoomed = None;
    }
    /// Copy structure with fresh identities; runtime processes are never copied.
    pub fn duplicate(&self) -> (Self, std::collections::BTreeMap<Uuid, Uuid>) {
        fn mapped(ids: &mut std::collections::BTreeMap<Uuid, Uuid>, id: Uuid) -> Uuid {
            *ids.entry(id).or_insert_with(Uuid::new_v4)
        }
        fn tree_ids(tree: &mut SplitNode, ids: &mut std::collections::BTreeMap<Uuid, Uuid>) {
            match tree {
                SplitNode::Pane(pane) => {
                    pane.id = mapped(ids, pane.id);
                    pane.selected = pane.selected.map(|id| mapped(ids, id));
                    for tab in &mut pane.tabs {
                        tab.id = mapped(ids, tab.id);
                    }
                }
                SplitNode::Split(branch) => {
                    branch.id = mapped(ids, branch.id);
                    tree_ids(&mut branch.first, ids);
                    tree_ids(&mut branch.second, ids);
                }
            }
        }
        let mut copy = self.clone();
        let mut ids = std::collections::BTreeMap::new();
        copy.id = mapped(&mut ids, copy.id);
        copy.title = format!("{} Copy", self.title.chars().take(59).collect::<String>());
        tree_ids(&mut copy.tree, &mut ids);
        if let Some(backup) = copy.layout_backup.as_mut() {
            tree_ids(backup, &mut ids);
        }
        copy.focused = mapped(&mut ids, copy.focused);
        copy.zoomed = None;
        copy.pane_order = copy
            .pane_order
            .iter()
            .map(|id| mapped(&mut ids, *id))
            .collect();
        copy.canvas.focused = copy.canvas.focused.map(|id| mapped(&mut ids, id));
        for pane in &mut copy.canvas.panes {
            pane.id = mapped(&mut ids, pane.id);
            pane.selected = mapped(&mut ids, pane.selected);
            pane.panels = pane.panels.iter().map(|id| mapped(&mut ids, *id)).collect();
        }
        (copy, ids)
    }
    pub fn close_tab(&mut self, pane: Uuid, tab: Uuid) -> bool {
        if self.layout_mode == LayoutMode::Splits {
            return self.close_tree_tab(pane, tab);
        }
        if !self
            .canvas
            .pane(pane)
            .is_some_and(|group| group.panels.contains(&tab))
        {
            return false;
        }
        let Some(base) = self.base_pane_for_tab(tab) else {
            return false;
        };
        let last = self.close_tree_tab(base, tab);
        if self.base_pane_for_tab(tab).is_none() {
            self.canvas.remove(tab);
        }
        if let Some(focused) = self.canvas.focused {
            self.focus_view_pane(focused);
        }
        last
    }
    fn close_tree_tab(&mut self, pane: Uuid, tab: Uuid) -> bool {
        let Some(target) = self.tree.pane_mut(pane) else {
            return false;
        };
        if target.tabs.iter().any(|item| item.id == tab && item.pinned) {
            return false;
        }
        if target.remove(tab).is_none() {
            return false;
        }
        if target.tabs.is_empty() {
            if self.tree.pane_ids().len() == 1 {
                return true;
            }
            self.tree = self.tree.clone().remove(pane).expect("other pane remains");
            if self.focused == pane {
                self.focused = self.tree.pane_ids()[0];
            }
        }
        false
    }
    pub fn move_tab(&mut self, source: Uuid, destination: Uuid, tab: Uuid, zone: DropZone) -> bool {
        let Some(source_pane) = self.tree.pane(source) else {
            return false;
        };
        let Some(source_index) = source_pane.tabs.iter().position(|item| item.id == tab) else {
            return false;
        };
        if self.tree.pane(destination).is_none() {
            return false;
        }
        if source == destination {
            if let DropZone::Tab(index) = zone {
                return self
                    .tree
                    .pane_mut(source)
                    .unwrap()
                    .reorder(source_index, index as isize);
            }
            if zone == DropZone::Center || source_pane.tabs.len() == 1 {
                return false;
            }
        }
        // Stage the move so validation failures cannot orphan a tab or its PTY.
        let mut tree = self.tree.clone();
        let pane = tree.pane_mut(source).unwrap();
        let moved = pane.remove(tab).unwrap();
        let empty_source = pane.tabs.is_empty();
        let focused = match zone {
            DropZone::Tab(index) => {
                tree.pane_mut(destination)
                    .unwrap()
                    .insert(moved, index as isize, true);
                destination
            }
            DropZone::Center => {
                tree.pane_mut(destination).unwrap().add(moved, true);
                destination
            }
            DropZone::Left | DropZone::Right | DropZone::Top | DropZone::Bottom => {
                let pane = Pane::new(vec![moved]);
                let id = pane.id;
                let horizontal = matches!(zone, DropZone::Left | DropZone::Right);
                let first = matches!(zone, DropZone::Left | DropZone::Top);
                if !tree.split(
                    destination,
                    pane,
                    if horizontal {
                        Orientation::Horizontal
                    } else {
                        Orientation::Vertical
                    },
                    first,
                ) {
                    return false;
                }
                id
            }
        };
        if empty_source {
            tree = tree.remove(source).expect("move destination remains");
        }
        if !self.pane_order.contains(&focused) {
            self.pane_order.push(focused);
        }
        self.tree = tree;
        self.focused = focused;
        true
    }
    pub fn move_divider(&mut self, id: Uuid, x: f64, y: f64, viewport: Rect) {
        if !x.is_finite() || !y.is_finite() {
            return;
        }
        let Some(layout) = self.tree.layout(viewport, 1.0) else {
            return;
        };
        let Some(branch) = self.tree.divider_mut(id) else {
            return;
        };
        let mut ids = branch.first.pane_ids();
        ids.extend(branch.second.pane_ids());
        let frames: Vec<_> = layout
            .panes
            .iter()
            .filter(|frame| ids.contains(&frame.id))
            .collect();
        let start_x = frames
            .iter()
            .map(|frame| frame.bounds.x)
            .fold(f64::INFINITY, f64::min);
        let start_y = frames
            .iter()
            .map(|frame| frame.bounds.y)
            .fold(f64::INFINITY, f64::min);
        let end_x = frames
            .iter()
            .map(|frame| frame.bounds.x + frame.bounds.width)
            .fold(0.0, f64::max);
        let end_y = frames
            .iter()
            .map(|frame| frame.bounds.y + frame.bounds.height)
            .fold(0.0, f64::max);
        let (offset, extent) = match branch.orientation {
            Orientation::Horizontal => (x - start_x, end_x - start_x - 1.0),
            Orientation::Vertical => (y - start_y, end_y - start_y - 1.0),
        };
        if extent > 0.0 {
            branch.position = (offset / extent).clamp(0.1, 0.9);
            branch.imposed_first_extent = None;
        }
    }
}

/// A named set of CLI accounts and settings. Each CLI keeps its own folder per profile;
/// a session's terminals start with its profile. Named "Profile #N" until renamed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionProfile {
    pub id: Uuid,
    pub number: usize,
    #[serde(default)]
    pub name: Option<String>,
    /// MCP servers shared with the other profiles that share them.
    #[serde(default)]
    pub share_mcp: bool,
    /// Skills shared with the other profiles that share them.
    #[serde(default)]
    pub share_skills: bool,
}
impl SessionProfile {
    /// The data this profile shares, as backend categories.
    pub fn shared(&self) -> Vec<basalt_core::domain::ShareCategory> {
        use basalt_core::domain::ShareCategory;
        [(self.share_mcp, ShareCategory::SharedMcp), (self.share_skills, ShareCategory::SharedSkills)]
            .into_iter()
            .filter_map(|(on, category)| on.then_some(category))
            .collect()
    }
    pub fn title(&self) -> String {
        self.name.clone().unwrap_or_else(|| format!("Profile #{}", self.number))
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Desktop {
    #[serde(skip, default = "empty_workspace")]
    empty: Workspace,
    pub workspaces: Vec<Workspace>,
    pub selected: Uuid,
    /// Each desktop/window owns its Dock, independently of workspace selection.
    pub dock: Option<Workspace>,
    pub dock_visible: bool,
    pub dock_focused: bool,
    history: Vec<Uuid>,
    cursor: usize,
    next_number: usize,
    #[serde(default)]
    pub profiles: Vec<SessionProfile>,
    /// Profile for the session about to start: the one the user was working in.
    #[serde(skip)]
    pending_profile: Option<Uuid>,
}
/// The smallest number from 1 that is not in `used`.
fn lowest_free(used: impl Iterator<Item = usize>) -> usize {
    let used: std::collections::HashSet<usize> = used.collect();
    (1..).find(|number| !used.contains(number)).unwrap()
}
fn empty_workspace() -> Workspace {
    Workspace::new(0)
}
impl Default for Desktop {
    fn default() -> Self {
        Self {
            empty: empty_workspace(),
            workspaces: vec![],
            selected: Uuid::nil(),
            dock: None,
            dock_visible: false,
            dock_focused: false,
            history: vec![],
            cursor: 0,
            next_number: 1,
            profiles: vec![],
            pending_profile: None,
        }
    }
}
impl Desktop {
    pub fn demo() -> Self {
        let workspace = Workspace::with_terminal(1);
        let id = workspace.id;
        Self {
            empty: empty_workspace(),
            workspaces: vec![workspace],
            selected: id,
            dock: None,
            dock_visible: false,
            dock_focused: false,
            history: vec![id],
            cursor: 0,
            next_number: 2,
            profiles: vec![],
            pending_profile: None,
        }
    }
}
impl Desktop {
    /// Keep a saved selection only if it still names a workspace; history restarts there.
    pub fn resume(&mut self) {
        // Sessions saved before numbering take the number from their old "Workspace N" title.
        for workspace in &mut self.workspaces {
            if workspace.number == 0 {
                workspace.number = workspace
                    .title
                    .strip_prefix("Workspace ")
                    .and_then(|number| number.parse().ok())
                    .unwrap_or(0);
            }
        }
        for index in 0..self.workspaces.len() {
            if self.workspaces[index].number == 0 {
                self.workspaces[index].number = self.free_number();
            }
        }
        // A session whose profile was removed returns to the user's own setup.
        let profiles: Vec<_> = self.profiles.iter().map(|profile| profile.id).collect();
        for workspace in &mut self.workspaces {
            if workspace.profile.is_some_and(|id| !profiles.contains(&id)) {
                workspace.profile = None;
            }
        }
        let highest = self.workspaces.iter().map(|workspace| workspace.number).max().unwrap_or(0);
        self.next_number = self.next_number.max(highest + 1);
        if !self.workspaces.iter().any(|workspace| workspace.id == self.selected) {
            self.selected = Uuid::nil();
        }
        self.history = if self.selected.is_nil() { vec![] } else { vec![self.selected] };
        self.cursor = 0;
        self.dock_focused = false;
    }
    pub fn apply_terminal_title(&mut self, tab: Uuid, title: &str) -> bool {
        for workspace in self.workspaces.iter_mut().chain(self.dock.iter_mut()) {
            for id in workspace.tree.pane_ids() {
                if let Some(tab) = workspace
                    .tree
                    .pane_mut(id)
                    .unwrap()
                    .tabs
                    .iter_mut()
                    .find(|item| item.id == tab)
                {
                    return tab.set_runtime_title(title);
                }
            }
        }
        false
    }
    pub fn dock_viewport(viewport: Rect, width: f64) -> Rect {
        Rect {
            x: viewport.x + viewport.width + 1.0,
            width,
            ..viewport
        }
    }
    /// Nearest visible pane from the focused one in direction (dx, dy): ahead
    /// of its center, preferring panes that overlap the same row or column.
    pub fn neighbor_pane(&self, viewport: Rect, dock_width: f64, dx: f64, dy: f64) -> Option<Uuid> {
        let layout = self.visible_layout(viewport, 1.0, dock_width)?;
        let focused = self.focused_container().focused_view_pane();
        let center = |r: &Rect| (r.x + r.width / 2.0, r.y + r.height / 2.0);
        let origin = center(&layout.panes.iter().find(|frame| frame.id == focused)?.bounds);
        layout
            .panes
            .iter()
            .filter(|frame| frame.id != focused)
            .filter_map(|frame| {
                let (x, y) = center(&frame.bounds);
                let along = (x - origin.0) * dx + (y - origin.1) * dy;
                let across = ((x - origin.0) * dy - (y - origin.1) * dx).abs();
                (along > 1.0).then_some((along + 2.0 * across, frame.id))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, id)| id)
    }
    pub fn visible_layout(&self, viewport: Rect, scale: f64, dock_width: f64) -> Option<Layout> {
        let mut layout = self.current().layout(viewport, scale)?;
        if let Some(dock) = self.dock.as_ref().filter(|_| self.dock_visible) {
            let dock_layout = dock
                .tree
                .layout(Self::dock_viewport(viewport, dock_width), scale)?;
            layout.panes.extend(dock_layout.panes);
            layout.dividers.extend(dock_layout.dividers);
        }
        Some(layout)
    }
    pub fn duplicate_workspace(
        &mut self,
        id: Uuid,
    ) -> Option<(Uuid, std::collections::BTreeMap<Uuid, Uuid>)> {
        let index = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == id)?;
        if self.workspaces.len() >= 128 {
            return None;
        }
        let (mut copy, ids) = self.workspaces[index].duplicate();
        copy.number = self.free_number();
        let new_id = copy.id;
        let base = self.workspaces[index]
            .title
            .chars()
            .take(54)
            .collect::<String>();
        let mut number = 1;
        while self
            .workspaces
            .iter()
            .any(|workspace| workspace.title == copy.title)
        {
            number += 1;
            copy.title = format!("{base} Copy {number}");
        }
        self.workspaces.insert(index + 1, copy);
        Some((new_id, ids))
    }
    pub fn move_workspace(&mut self, id: Uuid, direction: i32) -> bool {
        let Some(index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == id)
        else {
            return false;
        };
        let target = (index as i64 + i64::from(direction.signum()))
            .clamp(0, self.workspaces.len() as i64 - 1) as usize;
        if target == index {
            return false;
        }
        self.workspaces.swap(index, target);
        true
    }
    pub fn close_surface(&mut self, pane: Uuid, tab: Uuid) -> Vec<Uuid> {
        let Some(owner) = self.owner_of_pane(pane).map(|workspace| workspace.id) else {
            return vec![];
        };
        let is_dock = self.dock.as_ref().is_some_and(|dock| dock.id == owner);
        let last = self.owner_of_pane_mut(pane).unwrap().close_tab(pane, tab);
        if self.contains_tab(tab) {
            return vec![];
        }
        let retired = vec![tab];
        if last {
            if is_dock {
                self.dock = None;
                self.dock_visible = false;
                self.dock_focused = false;
            } else {
                // A session outlives its terminals: closing the last one leaves it empty,
                // ready for a new terminal; only closing the session removes it.
                self.owner_of_pane_mut(pane).unwrap().zoomed = None;
            }
        }
        retired
    }
    pub fn resize_divider(&mut self, id: Uuid, x: f64, y: f64, viewport: Rect, dock_width: f64) {
        if let Some(dock) = self.dock.as_mut().filter(|_| self.dock_visible) {
            if dock.tree.divider_mut(id).is_some() {
                dock.move_divider(id, x, y, Self::dock_viewport(viewport, dock_width));
                return;
            }
        }
        self.current_mut().move_divider(id, x, y, viewport);
    }
    pub fn containers(&self) -> impl Iterator<Item = &Workspace> {
        self.workspaces.iter().chain(self.dock.iter())
    }
    pub fn owner_of_pane(&self, pane: Uuid) -> Option<&Workspace> {
        self.containers()
            .find(|workspace| workspace.owns_pane(pane))
    }
    pub fn owner_of_pane_mut(&mut self, pane: Uuid) -> Option<&mut Workspace> {
        self.workspaces
            .iter_mut()
            .chain(self.dock.iter_mut())
            .find(|workspace| workspace.owns_pane(pane))
    }
    pub fn focused_container(&self) -> &Workspace {
        if self.dock_visible && self.dock_focused {
            self.dock.as_ref().expect("focused Dock exists")
        } else {
            self.current()
        }
    }
    pub fn focus_pane(&mut self, pane: Uuid) -> bool {
        let Some(owner) = self.owner_of_pane(pane).map(|workspace| workspace.id) else {
            return false;
        };
        let is_dock = self.dock.as_ref().is_some_and(|dock| dock.id == owner);
        if is_dock {
            self.dock_visible = true;
        } else {
            self.select(owner);
        }
        self.dock_focused = is_dock;
        self.owner_of_pane_mut(pane).unwrap().focus_view_pane(pane)
    }
    pub fn toggle_dock(&mut self) {
        self.dock_visible = !self.dock_visible;
        if self.dock_visible && self.dock.is_none() {
            let mut dock = Workspace::new(0);
            dock.title = "Dock".into();
            self.dock = Some(dock);
        }
        self.dock_focused = self.dock_visible;
    }
    pub fn current(&self) -> &Workspace {
        self.workspaces
            .iter()
            .find(|item| item.id == self.selected)
            .unwrap_or(&self.empty)
    }
    pub fn current_mut(&mut self) -> &mut Workspace {
        self.workspaces
            .iter_mut()
            .find(|item| item.id == self.selected)
            .unwrap_or(&mut self.empty)
    }
    pub fn select(&mut self, id: Uuid) {
        if self.history.is_empty() && self.workspaces.iter().any(|workspace| workspace.id == id) {
            self.history.push(id);
            self.cursor = 0;
            self.selected = id;
            return;
        }
        if self.workspaces.iter().any(|workspace| workspace.id == id) {
            self.dock_focused = false;
        }
        if id == self.selected || !self.workspaces.iter().any(|item| item.id == id) {
            return;
        }
        self.history.truncate(self.cursor + 1);
        self.history.push(id);
        self.cursor += 1;
        self.selected = id;
    }
    pub fn new_demo_workspace(&mut self) -> Uuid {
        let id = self.new_workspace();
        self.current_mut().add_auto_terminal();
        id
    }
    /// The smallest session number no open session uses: deleting sessions frees
    /// their numbers, so a fresh start counts from 1 again.
    fn free_number(&self) -> usize {
        lowest_free(self.workspaces.iter().map(|workspace| workspace.number))
    }
    /// The profile the current session uses (or the next one will).
    pub fn session_profile(&self) -> Option<Uuid> {
        if self.selected.is_nil() {
            return self.pending_profile;
        }
        self.workspaces
            .iter()
            .find(|workspace| workspace.id == self.selected)
            .and_then(|workspace| workspace.profile)
    }
    /// Choose the profile for the current session; terminals opened from now on use it.
    pub fn set_session_profile(&mut self, profile: Option<Uuid>) -> bool {
        if profile.is_some_and(|id| !self.profiles.iter().any(|item| item.id == id)) {
            return false;
        }
        match self.workspaces.iter_mut().find(|workspace| workspace.id == self.selected) {
            Some(workspace) => workspace.profile = profile,
            None => self.pending_profile = profile,
        }
        true
    }
    /// A new "Profile #N", numbered after the highest existing one.
    pub fn add_profile(&mut self) -> Option<Uuid> {
        if self.profiles.len() >= 64 {
            return None;
        }
        let number = lowest_free(self.profiles.iter().map(|profile| profile.number));
        let id = Uuid::new_v4();
        self.profiles.push(SessionProfile { id, number, name: None, share_mcp: false, share_skills: false });
        Some(id)
    }
    /// An empty name returns the profile to its "Profile #N" placeholder.
    pub fn rename_profile(&mut self, id: Uuid, name: &str) -> bool {
        let name = name.trim();
        if name.chars().count() > 64 {
            return false;
        }
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) else {
            return false;
        };
        profile.name = (!name.is_empty()).then(|| name.to_string());
        true
    }
    /// Remove a profile; sessions that used it return to the user's own setup. Its
    /// folders stay on disk.
    pub fn delete_profile(&mut self, id: Uuid) -> bool {
        let before = self.profiles.len();
        self.profiles.retain(|profile| profile.id != id);
        for workspace in &mut self.workspaces {
            if workspace.profile == Some(id) {
                workspace.profile = None;
            }
        }
        if self.pending_profile == Some(id) {
            self.pending_profile = None;
        }
        self.profiles.len() != before
    }
    /// Sidebar titles in session order, each with whether it is still a placeholder.
    /// A title already shown by an earlier session (the same conversation opened twice)
    /// is not repeated: the later session keeps its "Session #N".
    pub fn session_titles(&self) -> Vec<(String, bool)> {
        let mut shown = std::collections::HashSet::new();
        for workspace in self.workspaces.iter().filter(|workspace| workspace.renamed) {
            shown.insert(workspace.title.to_lowercase());
        }
        self.workspaces
            .iter()
            .map(|workspace| {
                if workspace.renamed {
                    return (workspace.title.clone(), false);
                }
                match workspace.auto_title() {
                    Some(title) if shown.insert(title.to_lowercase()) => (title, false),
                    _ => (workspace.placeholder_title(), true),
                }
            })
            .collect()
    }
    /// Leave the current session; the next terminal opened starts a new one, with the
    /// profile of the session the user was in.
    pub fn begin_session(&mut self) {
        self.pending_profile = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == self.selected)
            .and_then(|workspace| workspace.profile);
        self.selected = Uuid::nil();
        self.dock_focused = false;
    }
    pub fn new_workspace(&mut self) -> Uuid {
        let mut workspace = Workspace::new(self.next_number);
        workspace.number = self.free_number();
        workspace.profile = self.pending_profile.take();
        self.next_number += 1;
        let id = workspace.id;
        self.workspaces.push(workspace);
        self.select(id);
        id
    }
    pub fn close_workspace(&mut self, id: Uuid) -> Vec<Uuid> {
        let Some(index) = self.workspaces.iter().position(|item| item.id == id) else {
            return vec![];
        };
        let workspace = self.workspaces.remove(index);
        let tabs = workspace
            .tree
            .pane_ids()
            .iter()
            .flat_map(|pane| {
                workspace
                    .tree
                    .pane(*pane)
                    .unwrap()
                    .tabs
                    .iter()
                    .map(|tab| tab.id)
            })
            .collect();
        if self.workspaces.is_empty() {
            self.selected = Uuid::nil();
            self.history.clear();
            self.cursor = 0;
            return tabs;
        }
        if self.selected == id {
            self.selected = self.workspaces[index.min(self.workspaces.len() - 1)].id;
        }
        self.history.retain(|entry| *entry != id);
        // Closing a workspace must leave history pointed at the actual selection.
        if let Some(cursor) = self
            .history
            .iter()
            .rposition(|entry| *entry == self.selected)
        {
            self.cursor = cursor;
        } else {
            self.history.push(self.selected);
            self.cursor = self.history.len() - 1;
        }
        tabs
    }
    pub fn can_back(&self) -> bool {
        !self.history.is_empty() && self.cursor > 0
    }
    pub fn can_forward(&self) -> bool {
        self.cursor + 1 < self.history.len()
    }
    pub fn navigate(&mut self, forward: bool) {
        if self.history.is_empty() {
            return;
        }
        if forward && self.can_forward() {
            self.cursor += 1;
        } else if !forward && self.can_back() {
            self.cursor -= 1;
        }
        self.selected = self.history[self.cursor];
    }
    pub fn contains_tab(&self, id: Uuid) -> bool {
        self.containers().any(|workspace| {
            workspace.tree.pane_ids().iter().any(|pane| {
                workspace
                    .tree
                    .pane(*pane)
                    .unwrap()
                    .tabs
                    .iter()
                    .any(|tab| tab.id == id)
            })
        })
    }
}
