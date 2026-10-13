//! Shared mutations for UI buttons, keyboard shortcuts and command surfaces.
//! Every command resolves which workspace owns its target before mutating state.
use crate::workspace::Desktop;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    NewWorkspace,
    NewTerminalTab,
    NewPaneAutoLayout,
    EqualLayout,
    SideBySide,
    Stacked,
    RestoreLayout,
    ToggleDock,
    ToggleCanvasLayout,
}

impl Command {
    pub const ALL: [Self; 8] = [
        Self::NewWorkspace,
        Self::NewPaneAutoLayout,
        Self::EqualLayout,
        Self::SideBySide,
        Self::Stacked,
        Self::RestoreLayout,
        Self::ToggleDock,
        Self::ToggleCanvasLayout,
    ];
    pub fn id(self) -> &'static str {
        match self {
            Self::NewWorkspace => "new-workspace",
            Self::NewTerminalTab => "new-terminal-tab",
            Self::NewPaneAutoLayout => "new-pane-auto-layout",
            Self::EqualLayout => "layout-equal",
            Self::SideBySide => "layout-columns",
            Self::Stacked => "layout-rows",
            Self::RestoreLayout => "layout-restore",
            Self::ToggleDock => "toggle-dock",
            Self::ToggleCanvasLayout => "toggle-canvas-layout",
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Self::NewWorkspace => "New Workspace",
            Self::NewTerminalTab => "New Terminal",
            Self::NewPaneAutoLayout => "New Terminal",
            Self::EqualLayout => "Equal Layout",
            Self::SideBySide => "Side by Side",
            Self::Stacked => "Stacked Layout",
            Self::RestoreLayout => "Restore Previous Layout",
            Self::ToggleDock => "Toggle Dock",
            Self::ToggleCanvasLayout => "Toggle Canvas Layout",
        }
    }
    pub fn from_id(id: &str) -> Option<Self> {
        if id == "new-terminal-tab" {
            return Some(Self::NewPaneAutoLayout);
        }
        Self::ALL.into_iter().find(|command| command.id() == id)
    }
    pub fn matching(query: &str) -> Vec<Self> {
        let words: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
        Self::ALL
            .into_iter()
            .filter(|command| {
                let title = command.title().to_lowercase();
                words.iter().all(|word| title.contains(word))
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandTarget {
    Focused,
    Pane(Uuid),
}

impl Desktop {
    /// Return the created workspace/tab identity, or reject a stale pane target.
    /// Explicit pane ownership wins over a different selected workspace.
    pub fn execute(&mut self, command: Command, target: CommandTarget) -> Option<Uuid> {
        self.execute_at(
            command,
            target,
            crate::split::Rect {
                x: 0.0,
                y: 0.0,
                width: 859.0,
                height: 692.0,
            },
        )
    }
    pub fn execute_at(
        &mut self,
        command: Command,
        target: CommandTarget,
        viewport: crate::split::Rect,
    ) -> Option<Uuid> {
        if command == Command::NewWorkspace {
            return Some(self.new_workspace());
        }
        if command == Command::ToggleDock {
            self.toggle_dock();
            return self.dock.as_ref().map(|dock| dock.id);
        }
        let pane_id = match target {
            CommandTarget::Focused => self.focused_container().focused_view_pane(),
            CommandTarget::Pane(pane) => pane,
        };
        if !self.focus_pane(pane_id) {
            return None;
        }
        let is_dock = self.dock_focused;
        let workspace = self.owner_of_pane_mut(pane_id)?;
        if workspace.is_empty()
            && matches!(
                command,
                Command::NewTerminalTab | Command::NewPaneAutoLayout
            )
        {
            return Some(workspace.add_auto_terminal_at(viewport));
        }
        match command {
            Command::EqualLayout
            | Command::SideBySide
            | Command::Stacked
            | Command::RestoreLayout => {
                let direction = match command {
                    Command::SideBySide => Some(crate::split::Orientation::Horizontal),
                    Command::Stacked => Some(crate::split::Orientation::Vertical),
                    _ => None,
                };
                workspace.arrange(direction, command == Command::RestoreLayout, viewport);
                Some(workspace.id)
            }
            Command::ToggleCanvasLayout if is_dock => None,
            Command::ToggleCanvasLayout => {
                use crate::canvas::LayoutMode;
                let mode = if workspace.layout_mode == LayoutMode::Canvas {
                    LayoutMode::Splits
                } else {
                    LayoutMode::Canvas
                };
                workspace
                    .set_layout_mode(mode, viewport)
                    .then_some(workspace.id)
            }
            Command::NewTerminalTab
                if workspace.layout_mode == crate::canvas::LayoutMode::Canvas =>
            {
                workspace.add_canvas_terminal(pane_id, viewport, false)
            }
            Command::NewTerminalTab => workspace.add_terminal(pane_id, None),
            // Auto Layout in the focused Dock is an ordinary split to the right.
            Command::NewPaneAutoLayout if is_dock => {
                workspace.add_terminal(pane_id, Some(crate::split::Orientation::Horizontal))
            }
            Command::NewPaneAutoLayout
                if workspace.layout_mode == crate::canvas::LayoutMode::Canvas =>
            {
                workspace.add_canvas_terminal(pane_id, viewport, true)
            }
            Command::NewPaneAutoLayout => Some(workspace.add_auto_terminal_at(viewport)),
            Command::NewWorkspace | Command::ToggleDock => {
                unreachable!()
            }
        }
    }
}
