//! Versioned workspace documents. Runtime processes are deliberately not serialized.
use crate::workspace::Desktop;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
use basalt_core::{Error, ErrorKind, Result, storage::DataRoot};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TerminalLaunch {
    pub tool: Option<String>,
    pub profile: String,
    pub cwd: PathBuf,
    pub conversation: Option<basalt_core::tools::conversation::Conversation>,
    #[serde(default)]
    pub utility: Option<String>,
}
#[derive(Serialize, Deserialize)]
pub struct WorkspaceDocument {
    pub version: u32,
    pub desktop: Desktop,
    pub terminals: BTreeMap<Uuid, TerminalLaunch>,
}
impl WorkspaceDocument {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1
            || self.desktop.workspaces.len() > 128
            || self.terminals.len() > 512
            || self.desktop.profiles.len() > 64
            || self
                .desktop
                .profiles
                .iter()
                .any(|profile| profile.name.as_ref().is_some_and(|name| name.chars().count() > 64))
        {
            return Err(Error::new(
                ErrorKind::CorruptData,
                "Unsupported or oversized workspace document",
            ));
        }
        let mut ids = std::collections::HashSet::new();
        for workspace in self.desktop.containers() {
            if workspace.title.trim().is_empty()
                || workspace.title.len() > 256
                || !ids.insert(workspace.id)
            {
                return Err(Error::new(
                    ErrorKind::CorruptData,
                    "Invalid workspace identity",
                ));
            }
            if workspace
                .color_palette
                .is_some_and(|id| !(0..crate::preferences::PALETTE_COUNT).contains(&id))
                || workspace
                    .font_percent
                    .is_some_and(|size| !(50..=200).contains(&size))
            {
                return Err(Error::new(
                    ErrorKind::CorruptData,
                    "Invalid workspace appearance",
                ));
            }
            if workspace
                .repository_path
                .as_ref()
                .is_some_and(|path| !path.is_absolute())
            {
                return Err(Error::new(
                    ErrorKind::CorruptData,
                    "Invalid repository folder",
                ));
            }
            validate_tree(&workspace.tree, 0, &mut ids)?;
            if workspace.tree.pane(workspace.focused).is_none()
                || workspace.canvas.panes.len() > 512
            {
                return Err(Error::new(
                    ErrorKind::CorruptData,
                    "Invalid workspace focus",
                ));
            }
            let mut panels = std::collections::HashSet::new();
            for pane in &workspace.canvas.panes {
                if ![
                    pane.frame.x,
                    pane.frame.y,
                    pane.frame.width,
                    pane.frame.height,
                ]
                .iter()
                .all(|value| value.is_finite())
                    || pane.frame.width <= 0.0
                    || pane.frame.height <= 0.0
                    || !pane.panels.contains(&pane.selected)
                {
                    return Err(Error::new(
                        ErrorKind::CorruptData,
                        "Invalid canvas geometry",
                    ));
                }
                for tab in &pane.panels {
                    if workspace.base_pane_for_tab(*tab).is_none() || !panels.insert(*tab) {
                        return Err(Error::new(
                            ErrorKind::CorruptData,
                            "Invalid canvas terminal reference",
                        ));
                    }
                }
            }
        }
        for (id, launch) in &self.terminals {
            if !self.desktop.contains_tab(*id)
                || !launch.cwd.is_absolute()
                || launch.profile.is_empty()
            {
                return Err(Error::new(
                    ErrorKind::CorruptData,
                    "Invalid saved terminal launch",
                ));
            }
            if launch
                .utility
                .as_deref()
                .is_some_and(|utility| utility != "lazygit")
                || (launch.utility.is_some()
                    && (launch.tool.is_some() || launch.conversation.is_some()))
            {
                return Err(Error::new(
                    ErrorKind::CorruptData,
                    "Invalid utility terminal",
                ));
            }
            if let Some(tool) = &launch.tool {
                basalt_core::tools::catalog::Catalog::built_in().get(tool)?;
            }
            basalt_core::domain::ProfileId::try_from(launch.profile.clone())?;
            if let Some(conversation) = &launch.conversation {
                conversation.validate()?;
                if launch.tool.as_deref() != Some(&conversation.tool)
                    || launch.cwd != conversation.directory
                {
                    return Err(Error::new(
                        ErrorKind::CorruptData,
                        "Conversation does not match the saved terminal",
                    ));
                }
            }
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        serde_json::to_vec_pretty(self)
            .map_err(|error| Error::new(ErrorKind::CorruptData, error.to_string()))
    }
    pub fn load(root: &DataRoot) -> Result<Option<Self>> {
        let Some(bytes) = root.read_limited("workspaces.json", 8 * 1024 * 1024)? else {
            return Ok(None);
        };
        let mut document: Self = serde_json::from_slice(&bytes)
            .map_err(|error| Error::new(ErrorKind::CorruptData, error.to_string()))?;
        document.validate()?;
        // Resume where the user left off: keep the last selected workspace.
        document.desktop.resume();
        Ok(Some(document))
    }
}
fn validate_tree(
    tree: &crate::split::SplitNode,
    depth: usize,
    ids: &mut std::collections::HashSet<Uuid>,
) -> Result<()> {
    use crate::split::SplitNode;
    if depth > 32 {
        return Err(Error::new(
            ErrorKind::CorruptData,
            "Workspace split nesting exceeds limit",
        ));
    }
    match tree {
        SplitNode::Pane(pane) => {
            if !ids.insert(pane.id)
                || pane.tabs.len() > 128
                || pane
                    .selected
                    .is_some_and(|id| !pane.tabs.iter().any(|tab| tab.id == id))
            {
                return Err(Error::new(ErrorKind::CorruptData, "Invalid saved pane"));
            }
            for tab in &pane.tabs {
                if !ids.insert(tab.id) {
                    return Err(Error::new(
                        ErrorKind::CorruptData,
                        "Duplicate terminal identity",
                    ));
                }
            }
        }
        SplitNode::Split(branch) => {
            if !branch.position.is_finite()
                || !(0.0..=1.0).contains(&branch.position)
                || !ids.insert(branch.id)
            {
                return Err(Error::new(ErrorKind::CorruptData, "Invalid split ratio"));
            }
            validate_tree(&branch.first, depth + 1, ids)?;
            validate_tree(&branch.second, depth + 1, ids)?;
        }
    }
    Ok(())
}
