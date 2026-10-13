//! Profile-private data synchronization with explicit per-category opt-in.

mod config;
mod files;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::{
    Error, ErrorKind, Result,
    domain::{Profile, ShareCategory},
    storage::DataRoot,
    tools::catalog::Tool,
};

const MANIFEST: &str = ".sharing.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Hydrate,
    Persist,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CategoryState {
    files: Vec<String>,
    config_keys: Vec<String>,
    mcp_present: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    categories: BTreeMap<String, CategoryState>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: 1,
            categories: BTreeMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct SharedData {
    root: DataRoot,
    locks: Arc<Mutex<BTreeMap<String, Arc<Mutex<()>>>>>,
}

impl SharedData {
    pub fn new(root: DataRoot) -> Self {
        Self {
            root,
            locks: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn hydrate(&self, tool: &Tool, profile: &Profile) -> Result<()> {
        self.synchronize(Direction::Hydrate, tool, profile)
    }

    pub fn persist(&self, tool: &Tool, profile: &Profile) -> Result<()> {
        self.synchronize(Direction::Persist, tool, profile)
    }

    fn synchronize(&self, direction: Direction, tool: &Tool, profile: &Profile) -> Result<()> {
        self.synchronize_folders(
            direction,
            tool,
            &|category| profile.settings.shared(category),
            tool.profile_relative(profile),
            PathBuf::from(&tool.dir).join("_shared"),
        )
    }

    /// A session profile's data for `tool` receives the shared data of the given
    /// categories, as a terminal starts.
    pub fn hydrate_session(
        &self,
        tool: &Tool,
        profile: &str,
        categories: &[ShareCategory],
    ) -> Result<()> {
        let (private, shared) = session_folders(tool, profile)?;
        self.synchronize_folders(
            Direction::Hydrate,
            tool,
            &|category| categories.contains(&category),
            private,
            shared,
        )
    }

    /// A session profile's changes for `tool` become the shared data of the given
    /// categories, as its CLI exits.
    pub fn persist_session(
        &self,
        tool: &Tool,
        profile: &str,
        categories: &[ShareCategory],
    ) -> Result<()> {
        let (private, shared) = session_folders(tool, profile)?;
        self.synchronize_folders(
            Direction::Persist,
            tool,
            &|category| categories.contains(&category),
            private,
            shared,
        )
    }

    /// A session profile starts sharing `category` for `tool`: what it has joins the
    /// shared data without replacing what other profiles shared, then it takes the
    /// combined data.
    pub fn join_session(&self, tool: &Tool, profile: &str, category: ShareCategory) -> Result<()> {
        let (private, shared) = session_folders(tool, profile)?;
        {
            let lock = self.lock(&tool.id);
            let _guard = lock.lock();
            let shared = shared.join(category.key());
            if category == ShareCategory::SharedMcp {
                config::merge_mcp(&self.root, &private, &shared, tool)?;
            }
            files::FileSync {
                root: &self.root,
                source: &private,
                target: &shared,
                tool,
                category,
                rules: &tool.paths(category),
                previous: &[],
                keep_newer: true,
            }
            .run()?;
        }
        self.hydrate_session(tool, profile, &[category])
    }

    fn lock(&self, tool: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .entry(tool.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn synchronize_folders(
        &self,
        direction: Direction,
        tool: &Tool,
        shared_category: &dyn Fn(ShareCategory) -> bool,
        private: PathBuf,
        shared_root: PathBuf,
    ) -> Result<()> {
        if !ShareCategory::ALL.into_iter().any(shared_category) {
            return Ok(());
        }
        let lock = self.lock(&tool.id);
        let _guard = lock.lock();
        let manifest_path = private.join(MANIFEST);
        let mut manifest = self
            .root
            .read_json::<Manifest>(&manifest_path)?
            .unwrap_or_default();
        if manifest.version != 1
            || manifest.categories.keys().any(|key| {
                !ShareCategory::ALL
                    .into_iter()
                    .any(|category| category.key() == key)
            })
        {
            return Err(Error::new(
                ErrorKind::CorruptData,
                "Invalid profile sharing manifest",
            ));
        }
        for category in ShareCategory::ALL {
            if let Some(state) = manifest.categories.get(category.key()) {
                files::validate_previous(&tool.paths(category), &state.files)?;
            }
            if shared_category(category)
                && matches!(
                    category,
                    ShareCategory::SharedConfig | ShareCategory::SharedMcp
                )
            {
                let shared = shared_root.join(category.key());
                let (source, target) = match direction {
                    Direction::Hydrate => (shared.as_path(), private.as_path()),
                    Direction::Persist => (private.as_path(), shared.as_path()),
                };
                config::preflight(&self.root, source, target, tool)?;
            }
        }
        for category in ShareCategory::ALL {
            if !shared_category(category) {
                continue;
            }
            let shared = shared_root.join(category.key());
            let (source, target) = match direction {
                Direction::Hydrate => (shared.as_path(), private.as_path()),
                Direction::Persist => (private.as_path(), shared.as_path()),
            };
            let previous = manifest
                .categories
                .get(category.key())
                .cloned()
                .unwrap_or_default();
            let rules = tool.paths(category);
            let files = files::FileSync {
                root: &self.root,
                source,
                target,
                tool,
                category,
                rules: &rules,
                previous: &previous.files,
                keep_newer: direction == Direction::Hydrate
                    && category == ShareCategory::SharedConfig,
            }
            .run()?;
            let (config_keys, mcp_present) = if matches!(
                category,
                ShareCategory::SharedConfig | ShareCategory::SharedMcp
            ) {
                config::synchronize(&self.root, source, target, tool, category, &previous)?
            } else {
                (Vec::new(), false)
            };
            manifest.categories.insert(
                category.key().into(),
                CategoryState {
                    files,
                    config_keys,
                    mcp_present,
                },
            );
            self.root.write_json(&manifest_path, &manifest)?;
        }
        Ok(())
    }
}

/// How many skills and MCP servers a CLI keeps under `home`, its folder in a profile.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Contents {
    pub skills: usize,
    pub mcp_servers: usize,
}

pub fn contents(root: &DataRoot, tool: &Tool, home: &Path) -> Contents {
    let skills = tool
        .paths(ShareCategory::SharedSkills)
        .iter()
        .filter_map(|folder| root.resolve(home.join(folder)).ok())
        .filter_map(|folder| std::fs::read_dir(folder).ok())
        .flatten()
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .count();
    Contents {
        skills,
        mcp_servers: config::mcp_count(root, home, tool),
    }
}

/// A session profile's folder for one CLI, and where its shared data lives.
fn session_folders(tool: &Tool, profile: &str) -> Result<(PathBuf, PathBuf)> {
    crate::domain::windows_component(profile, 64)?;
    Ok((
        crate::profiles::environment::session_folder(profile).join(&tool.id),
        PathBuf::from("Sessions").join("_shared").join(&tool.id),
    ))
}

fn relative_path(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if value.is_empty()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(Error::new(
            ErrorKind::CorruptData,
            "Sharing path must be relative",
        ));
    }
    for component in path.components() {
        let name = component
            .as_os_str()
            .to_str()
            .ok_or_else(|| Error::new(ErrorKind::CorruptData, "Sharing path must be Unicode"))?;
        crate::domain::windows_component(name, 255)?;
    }
    Ok(path.to_owned())
}

fn normalized(path: &Path) -> Result<String> {
    let mut names = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(Error::new(ErrorKind::CorruptData, "Invalid managed path"));
        };
        names.push(
            name.to_str().ok_or_else(|| {
                Error::new(ErrorKind::CorruptData, "Managed path must be Unicode")
            })?,
        );
    }
    Ok(names.join("/"))
}
