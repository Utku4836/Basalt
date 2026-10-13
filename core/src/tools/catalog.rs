use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    Error, ErrorKind, Result,
    domain::{Profile, ShareCategory},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installer {
    #[serde(rename = "type")]
    pub kind: String,
    pub package: Option<String>,
    pub manifest: Option<String>,
    pub url: Option<String>,
    pub repo: Option<String>,
    pub asset: Option<String>,
    pub executable: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRule {
    pub path: String,
    pub format: String,
    pub key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub id: String,
    pub name: String,
    pub dir: String,
    pub bin: String,
    pub installer: Installer,
    pub sharing: BTreeMap<String, Vec<String>>,
    pub mcp: Option<McpRule>,
    pub permission_args: Vec<String>,
}

impl Tool {
    pub fn directory(&self, root: &Path) -> PathBuf {
        root.join(&self.dir)
    }

    pub fn profile_relative(&self, profile: &Profile) -> PathBuf {
        PathBuf::from(&self.dir)
            .join("Profiles")
            .join(&profile.name)
    }

    pub fn document_name(&self, profile: &Profile) -> String {
        format!("{}-{}.Toml", profile.name, self.name)
    }

    pub fn paths(&self, category: ShareCategory) -> Vec<String> {
        let mut paths = self
            .sharing
            .get(category.key())
            .cloned()
            .unwrap_or_default();
        if category == ShareCategory::SharedMcp {
            if let Some(rule) = &self.mcp {
                if rule.format == "file" {
                    paths.push(rule.path.clone());
                }
            }
        }
        paths
    }

    pub fn executable_candidates(&self, root: &Path) -> Vec<PathBuf> {
        let base = self.directory(root);
        let mut paths = Vec::new();
        if self.installer.kind == "npm" {
            for ext in [".cmd", ".exe", ".bat", ""] {
                paths.push(
                    base.join("node_modules/.bin")
                        .join(format!("{}{ext}", self.bin)),
                );
            }
            for ext in [".cmd", ".exe", ".bat"] {
                paths.push(base.join(format!("{}{ext}", self.bin)));
            }
        }
        for relative in match self.id.as_str() {
            "agy" => vec!["AppData/Local/agy/bin/agy.exe"],
            "aider" => vec!["bin/aider.exe", ".local/bin/aider.exe"],
            "goose" => vec!["bin/goose.exe"],
            "cursor-agent" => vec!["bin/cursor-agent.exe", "bin/cursor-agent.cmd"],
            _ => vec![],
        } {
            paths.push(base.join(relative));
        }
        paths
    }

    pub fn global_credentials(&self) -> bool {
        self.id == "agy"
    }
}

#[derive(Debug, Clone)]
pub struct Catalog {
    tools: Vec<Tool>,
}

impl Catalog {
    pub fn built_in() -> Self {
        Self {
            tools: serde_json::from_str(include_str!("../../config/tool-catalog.json"))
                .expect("built-in catalog is validated by the build verification"),
        }
    }

    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    pub fn get(&self, id: &str) -> Result<&Tool> {
        self.tools
            .iter()
            .find(|tool| tool.id == id)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "Tool not found"))
    }
}
