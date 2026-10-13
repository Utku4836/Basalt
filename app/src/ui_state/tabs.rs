//! Tabs inside a pane: selection, pinning, ordering and titles.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceKind {
    Terminal,
    Browser,
    Chat,
    File,
    Diff,
    Canvas,
    Simulator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tab {
    pub id: Uuid,
    pub title: String,
    #[serde(default)]
    pub runtime_title: Option<String>,
    #[serde(default)]
    pub custom_title: Option<String>,
    pub kind: SurfaceKind,
    pub pinned: bool,
    pub dirty: bool,
    pub unread: bool,
}
impl Tab {
    pub fn display_title(&self) -> &str {
        self.custom_title
            .as_deref()
            .or(self.runtime_title.as_deref())
            .unwrap_or(&self.title)
    }
    pub fn detail_title(&self) -> &str {
        self.runtime_title
            .as_deref()
            .unwrap_or_else(|| self.display_title())
    }
    pub fn compact_title(&self) -> String {
        if let Some(title) = &self.custom_title {
            return title.clone();
        }
        let raw = self.display_title();
        let lower = raw.to_ascii_lowercase();
        for (needle, name) in [
            ("claude", "Claude"),
            ("codex", "Codex"),
            ("opencode", "OpenCode"),
            ("gemini", "Gemini"),
            ("aider", "Aider"),
            ("powershell", "PowerShell"),
            ("pwsh", "PowerShell"),
            ("cmd.exe", "CMD"),
            ("wsl.exe", "WSL"),
            ("bash", "Bash"),
        ] {
            if lower.contains(needle) {
                return name.into();
            }
        }
        if raw.chars().count() > 40 {
            "Terminal".into()
        } else {
            raw.into()
        }
    }
    pub fn set_runtime_title(&mut self, title: &str) -> bool {
        if self.kind != SurfaceKind::Terminal {
            return false;
        }
        let cleaned: String = title
            .chars()
            .filter(|ch| !ch.is_control())
            .take(512)
            .collect();
        let cleaned = cleaned.trim();
        let next = (!cleaned.is_empty()).then(|| cleaned.to_owned());
        if self.runtime_title == next {
            return false;
        }
        self.runtime_title = next;
        true
    }
    pub fn terminal(title: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            runtime_title: None,
            custom_title: None,
            kind: SurfaceKind::Terminal,
            pinned: false,
            dirty: false,
            unread: false,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pane {
    pub id: Uuid,
    pub tabs: Vec<Tab>,
    pub selected: Option<Uuid>,
    pub full_width_tab: bool,
}
impl Pane {
    pub fn new(tabs: Vec<Tab>) -> Self {
        let selected = tabs.first().map(|tab| tab.id);
        Self {
            id: Uuid::new_v4(),
            tabs,
            selected,
            full_width_tab: false,
        }
    }
    pub fn selected_tab(&self) -> Option<&Tab> {
        self.tabs.iter().find(|tab| Some(tab.id) == self.selected)
    }
    pub fn select(&mut self, id: Uuid) -> bool {
        if !self.tabs.iter().any(|tab| tab.id == id) {
            return false;
        }
        self.selected = Some(id);
        true
    }
    pub fn add(&mut self, tab: Tab, select: bool) -> bool {
        let index = if tab.pinned {
            self.pinned_count()
        } else {
            self.tabs.len()
        };
        self.insert(tab, index as isize, select)
    }
    pub fn insert(&mut self, tab: Tab, index: isize, select: bool) -> bool {
        if self.tabs.iter().any(|existing| existing.id == tab.id) {
            return false;
        }
        let requested = index.max(0) as usize;
        let requested = requested.min(self.tabs.len());
        let pinned = self.pinned_count();
        let index = if tab.pinned {
            requested.min(pinned)
        } else {
            requested.max(pinned)
        };
        let id = tab.id;
        self.tabs.insert(index, tab);
        if select {
            self.selected = Some(id);
        }
        true
    }
    pub fn remove(&mut self, id: Uuid) -> Option<Tab> {
        let index = self.tabs.iter().position(|tab| tab.id == id)?;
        let removed = self.tabs.remove(index);
        if self.selected == Some(id) {
            self.selected = self
                .tabs
                .get(index.min(self.tabs.len().saturating_sub(1)))
                .map(|tab| tab.id);
        }
        Some(removed)
    }
    pub fn reorder(&mut self, source: usize, destination: isize) -> bool {
        if source >= self.tabs.len() || destination < 0 || destination as usize > self.tabs.len() {
            return false;
        }
        let destination = destination as usize;
        if destination == source || destination == source + 1 {
            return false;
        }
        let tab = self.tabs.remove(source);
        let requested = if destination > source {
            destination - 1
        } else {
            destination
        };
        let pinned = self.pinned_count();
        let adjusted = if tab.pinned {
            requested.min(pinned)
        } else {
            requested.max(pinned)
        };
        self.tabs.insert(adjusted.min(self.tabs.len()), tab);
        true
    }
    fn pinned_count(&self) -> usize {
        self.tabs.iter().filter(|tab| tab.pinned).count()
    }
}
