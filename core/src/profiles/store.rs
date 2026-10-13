use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use chrono::{SecondsFormat, Utc};
use fs2::FileExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::{
    Error, ErrorKind, Result,
    domain::{Profile, ProfileId, ProfileIndex, ProfileSettings, profile_name, same_windows_name},
    profiles::activity::{ActivityBook, ActivityKind},
    storage::{
        DataRoot,
        transaction::{self, Transaction},
    },
    tools::catalog::{Catalog, Tool},
};

const INDEX: &str = "_profiles/profiles.json";

pub trait Clock: Send + Sync {
    fn now(&self) -> String;
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> String {
        Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
    }
}

#[derive(Debug, Serialize)]
pub struct Mutation<T> {
    pub value: T,
    pub warnings: Vec<Error>,
}

pub struct ProfileRepository {
    root: DataRoot,
    catalog: Catalog,
    index: Mutex<ProfileIndex>,
    activities: ActivityBook,
    clock: Arc<dyn Clock>,
    poisoned: AtomicBool,
    warnings: Mutex<Vec<Error>>,
    _lock: File,
}

#[derive(Serialize, Deserialize)]
struct DocumentHeader {
    id: ProfileId,
    name: String,
    cli: String,
    created_at: String,
    updated_at: String,
}

#[derive(Serialize, Deserialize)]
struct ProfileDocument {
    profile: DocumentHeader,
    settings: toml::Table,
}

impl ProfileRepository {
    pub fn open(root: DataRoot, catalog: Catalog) -> Result<Self> {
        Self::with_clock(root, catalog, Arc::new(SystemClock))
    }

    pub fn with_clock(root: DataRoot, catalog: Catalog, clock: Arc<dyn Clock>) -> Result<Self> {
        root.create_dir("_profiles")?;
        let path = root.resolve("_profiles/backend.lock")?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| Error::io("Open repository lock", &path, e))?;
        lock.try_lock_exclusive().map_err(|_| {
            Error::new(
                ErrorKind::Busy,
                "The data root is already open in another backend",
            )
        })?;
        let mut warnings = transaction::recover(&root)?;
        warnings.extend(crate::storage::artifacts::recover(&root)?);
        warnings.extend(crate::storage::generations::recover(&root)?);
        let mut index = root.read_json::<ProfileIndex>(INDEX)?.unwrap_or_default();
        validate_index(&index, &catalog)?;
        if index.schema_version == 1 || index.layout_version < 2 || index.shared_install_version < 1
        {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "A legacy layout requires the layout migration service before opening profiles",
            ));
        }
        let mut transaction = Transaction::new(&root);
        let mut changed = false;
        for tool in catalog.tools() {
            let profiles = index.tools.entry(tool.id.clone()).or_default();
            if profiles.is_empty() {
                let profile = Profile::default_at(&clock.now());
                let relative = tool.profile_relative(&profile);
                if has_files(&root.resolve(&relative)?)? {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        "An unregistered Default folder contains data",
                    ));
                }
                prepare_profile(&root, &relative)?;
                transaction.write(
                    relative.join(tool.document_name(&profile)),
                    document_bytes(tool, &profile)?,
                )?;
                profiles.push(profile);
                changed = true;
            } else {
                for profile in profiles.iter_mut() {
                    let relative = tool.profile_relative(profile);
                    let file = relative.join(tool.document_name(profile));
                    if let Some(bytes) = root.read(&file)? {
                        let settings = parse_document(&bytes, tool, profile)?;
                        if settings != profile.settings {
                            profile.settings = settings;
                            changed = true;
                        }
                    } else {
                        if has_files(&root.resolve(&relative)?)? {
                            return Err(Error::new(
                                ErrorKind::Conflict,
                                "Profile folder has data without its document",
                            ));
                        }
                        prepare_profile(&root, &relative)?;
                        transaction.write(file, document_bytes(tool, profile)?)?;
                        changed = true;
                    }
                }
            }
        }
        if changed || root.read(INDEX)?.is_none() {
            transaction.write(
                INDEX.into(),
                serde_json::to_vec_pretty(&index).map_err(Error::corrupt)?,
            )?;
            warnings.extend(transaction.commit()?);
        }
        Ok(Self {
            root,
            catalog,
            index: Mutex::new(index),
            activities: ActivityBook::default(),
            clock,
            poisoned: AtomicBool::new(false),
            warnings: Mutex::new(warnings),
            _lock: lock,
        })
    }

    pub fn root(&self) -> &DataRoot {
        &self.root
    }
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }
    pub fn activities(&self) -> &ActivityBook {
        &self.activities
    }
    pub fn take_warnings(&self) -> Vec<Error> {
        std::mem::take(&mut *self.warnings.lock())
    }

    pub fn require_recovery(&self) {
        self.poisoned.store(true, Ordering::Release);
    }

    pub fn list(&self, tool_id: &str) -> Result<Vec<Profile>> {
        self.ensure_healthy()?;
        self.catalog.get(tool_id)?;
        Ok(self
            .index
            .lock()
            .tools
            .get(tool_id)
            .cloned()
            .unwrap_or_default())
    }

    pub fn get(&self, tool_id: &str, id: &ProfileId) -> Result<Profile> {
        self.list(tool_id)?
            .into_iter()
            .find(|profile| &profile.id == id)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "Profile not found"))
    }

    pub fn create(
        &self,
        tool_id: &str,
        requested_name: &str,
        settings: ProfileSettings,
    ) -> Result<Mutation<Profile>> {
        let tool = self.catalog.get(tool_id)?;
        let name = profile_name(requested_name)?;
        let id = ProfileId::new_unique();
        let _lease = self
            .activities
            .reserve(tool, Some(&id), &settings, ActivityKind::Edit)?;
        let mut current = self.index.lock();
        self.ensure_current(&current)?;
        let profiles = current
            .tools
            .get(tool_id)
            .ok_or_else(|| Error::new(ErrorKind::CorruptData, "Tool has no profile list"))?;
        check_duplicate(profiles, &name, None)?;
        let timestamp = self.clock.now();
        let profile = Profile {
            id,
            name,
            settings,
            created_at: timestamp.clone(),
            updated_at: timestamp,
        };
        let target = tool.profile_relative(&profile);
        if self.root.resolve(&target)?.exists() {
            return Err(Error::new(
                ErrorKind::Conflict,
                "A profile folder with this name already exists",
            ));
        }
        let mut next = current.clone();
        next.tools
            .get_mut(tool_id)
            .expect("validated tool")
            .push(profile.clone());
        let mut transaction = Transaction::new(&self.root);
        let stage = transaction.prepare_directory()?;
        if let Err(error) = prepare_profile(&self.root, &stage) {
            self.root.remove_owned_tree(transaction.directory())?;
            return Err(error);
        }
        transaction.move_directory(stage, target.clone());
        transaction.change(
            target.join(tool.document_name(&profile)),
            None,
            Some(document_bytes(tool, &profile)?),
        )?;
        self.commit(&mut current, next, transaction, profile)
    }

    pub fn update_settings(
        &self,
        tool_id: &str,
        id: &ProfileId,
        settings: ProfileSettings,
    ) -> Result<Mutation<Profile>> {
        let tool = self.catalog.get(tool_id)?;
        let _lease = self
            .activities
            .reserve(tool, Some(id), &settings, ActivityKind::Edit)?;
        let mut current = self.index.lock();
        self.ensure_current(&current)?;
        let mut next = current.clone();
        let profile = find_mut(&mut next, tool_id, id)?;
        profile.settings = settings;
        profile.updated_at = self.clock.now();
        let profile = profile.clone();
        let mut transaction = Transaction::new(&self.root);
        transaction.write(
            tool.profile_relative(&profile)
                .join(tool.document_name(&profile)),
            document_bytes(tool, &profile)?,
        )?;
        self.commit(&mut current, next, transaction, profile)
    }

    pub fn rename(
        &self,
        tool_id: &str,
        id: &ProfileId,
        requested_name: &str,
    ) -> Result<Mutation<Profile>> {
        let tool = self.catalog.get(tool_id)?;
        let name = profile_name(requested_name)?;
        let _lease = self.activities.reserve(
            tool,
            Some(id),
            &ProfileSettings::default(),
            ActivityKind::Rename,
        )?;
        let mut current = self.index.lock();
        self.ensure_current(&current)?;
        let profiles = current
            .tools
            .get(tool_id)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "Profile not found"))?;
        check_duplicate(profiles, &name, Some(id))?;
        let previous = profiles
            .iter()
            .find(|profile| &profile.id == id)
            .cloned()
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "Profile not found"))?;
        let mut next = current.clone();
        let profile = find_mut(&mut next, tool_id, id)?;
        profile.name = name;
        profile.updated_at = self.clock.now();
        let profile = profile.clone();
        let from = tool.profile_relative(&previous);
        let to = tool.profile_relative(&profile);
        let mut transaction = Transaction::new(&self.root);
        if from != to {
            if !same_windows_name(&previous.name, &profile.name) && self.root.resolve(&to)?.exists()
            {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "Profile rename target already exists",
                ));
            }
            let old_document = from.join(tool.document_name(&previous));
            let before = self.root.read(&old_document)?;
            transaction.move_directory(from.clone(), to.clone());
            transaction.change(to.join(tool.document_name(&previous)), before, None)?;
            transaction.change(
                to.join(tool.document_name(&profile)),
                None,
                Some(document_bytes(tool, &profile)?),
            )?;
            plan_path_updates(
                &self.root,
                tool,
                profiles,
                &previous,
                &profile,
                &mut transaction,
            )?;
        } else {
            transaction.write(
                to.join(tool.document_name(&profile)),
                document_bytes(tool, &profile)?,
            )?;
        }
        self.commit(&mut current, next, transaction, profile)
    }

    pub fn delete(&self, tool_id: &str, id: &ProfileId) -> Result<Mutation<PathBuf>> {
        if id.is_default() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "The Default profile cannot be deleted",
            ));
        }
        let tool = self.catalog.get(tool_id)?;
        let _lease = self.activities.reserve(
            tool,
            Some(id),
            &ProfileSettings::default(),
            ActivityKind::Edit,
        )?;
        let mut current = self.index.lock();
        self.ensure_current(&current)?;
        let mut next = current.clone();
        let profiles = next
            .tools
            .get_mut(tool_id)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "Profile not found"))?;
        let position = profiles
            .iter()
            .position(|profile| &profile.id == id)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "Profile not found"))?;
        let profile = profiles.remove(position);
        let mut transaction = Transaction::new(&self.root);
        let target = PathBuf::from("_profiles/trash").join(tool_id).join(format!(
            "{}-{}",
            profile.name,
            uuid::Uuid::new_v4().simple()
        ));
        transaction.move_directory(tool.profile_relative(&profile), target.clone());
        let absolute = self.root.resolve(&target)?;
        self.commit(&mut current, next, transaction, absolute)
    }

    fn commit<T>(
        &self,
        current: &mut ProfileIndex,
        next: ProfileIndex,
        mut transaction: Transaction,
        value: T,
    ) -> Result<Mutation<T>> {
        transaction.write(
            INDEX.into(),
            serde_json::to_vec_pretty(&next).map_err(Error::corrupt)?,
        )?;
        match transaction.commit() {
            Ok(warnings) => {
                *current = next;
                Ok(Mutation { value, warnings })
            }
            Err(error) => {
                if error.kind == ErrorKind::Recovery {
                    self.poisoned.store(true, Ordering::Release);
                }
                Err(error)
            }
        }
    }

    fn ensure_healthy(&self) -> Result<()> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(Error::new(
                ErrorKind::Recovery,
                "Reopen the backend to recover an unfinished profile operation",
            ));
        }
        Ok(())
    }

    fn ensure_current(&self, current: &ProfileIndex) -> Result<()> {
        self.ensure_healthy()?;
        if self.root.read_json::<ProfileIndex>(INDEX)?.as_ref() != Some(current) {
            return Err(Error::new(
                ErrorKind::Conflict,
                "Profile index changed outside the backend",
            ));
        }
        Ok(())
    }
}

fn validate_index(index: &ProfileIndex, catalog: &Catalog) -> Result<()> {
    if ![1, 2].contains(&index.schema_version) {
        return Err(Error::new(
            ErrorKind::CorruptData,
            "Unsupported profile metadata schema",
        ));
    }
    for (tool_id, profiles) in &index.tools {
        catalog.get(tool_id)?;
        let mut ids = BTreeSet::new();
        for profile in profiles {
            if !ids.insert(&profile.id) || profile_name(&profile.name)? != profile.name {
                return Err(Error::new(
                    ErrorKind::CorruptData,
                    "Invalid or duplicate profile metadata",
                ));
            }
            check_duplicate(profiles, &profile.name, Some(&profile.id))?;
        }
    }
    Ok(())
}

fn check_duplicate(profiles: &[Profile], name: &str, except: Option<&ProfileId>) -> Result<()> {
    if profiles
        .iter()
        .any(|profile| Some(&profile.id) != except && same_windows_name(&profile.name, name))
    {
        return Err(Error::new(
            ErrorKind::Conflict,
            "A profile with this name already exists",
        ));
    }
    Ok(())
}

fn find_mut<'a>(
    index: &'a mut ProfileIndex,
    tool_id: &str,
    id: &ProfileId,
) -> Result<&'a mut Profile> {
    index
        .tools
        .get_mut(tool_id)
        .and_then(|profiles| profiles.iter_mut().find(|profile| &profile.id == id))
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "Profile not found"))
}

pub fn prepare_profile(root: &DataRoot, relative: &Path) -> Result<()> {
    for directory in [
        "",
        ".config",
        ".local/share",
        ".local/state",
        ".cache",
        "AppData/Roaming",
        "AppData/Local",
        "Temp",
        "workspace",
        "logs",
    ] {
        root.create_dir(relative.join(directory))?;
    }
    Ok(())
}

fn has_files(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    crate::storage::reject_links(path)?;
    for entry in fs::read_dir(path).map_err(|e| Error::io("Inspect profile data", path, e))? {
        let entry = entry.map_err(|e| Error::io("Inspect profile entry", path, e))?;
        crate::storage::reject_links(&entry.path())?;
        let metadata = entry
            .file_type()
            .map_err(|e| Error::io("Inspect profile entry", &entry.path(), e))?;
        if !metadata.is_dir() || has_files(&entry.path())? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn document_bytes(tool: &Tool, profile: &Profile) -> Result<Vec<u8>> {
    let settings = &profile.settings;
    let fields = [
        ("full_permission", settings.full_permission),
        ("shared_sessions", settings.shared_sessions),
        ("shared_models", settings.shared_models),
        ("shared_config", settings.shared_config),
        ("shared_skills", settings.shared_skills),
        ("shared_mcp", settings.shared_mcp),
    ];
    let document = ProfileDocument {
        profile: DocumentHeader {
            id: profile.id.clone(),
            name: profile.name.clone(),
            cli: tool.id.clone(),
            created_at: profile.created_at.clone(),
            updated_at: profile.updated_at.clone(),
        },
        settings: fields
            .into_iter()
            .map(|(key, value)| (key.into(), toml::Value::Boolean(value)))
            .collect(),
    };
    Ok(toml::to_string(&document)
        .map_err(Error::corrupt)?
        .into_bytes())
}

fn parse_document(bytes: &[u8], tool: &Tool, profile: &Profile) -> Result<ProfileSettings> {
    let document: ProfileDocument =
        toml::from_str(std::str::from_utf8(bytes).map_err(Error::corrupt)?)
            .map_err(Error::corrupt)?;
    if document.profile.id != profile.id
        || document.profile.cli != tool.id
        || document.profile.name != profile.name
    {
        return Err(Error::new(
            ErrorKind::Conflict,
            "Profile document identity does not match its folder and index",
        ));
    }
    let flag = |name: &str| match document.settings.get(name) {
        None => Ok(false),
        Some(toml::Value::Boolean(value)) => Ok(*value),
        _ => Err(Error::new(
            ErrorKind::CorruptData,
            "Profile setting must be a boolean",
        )),
    };
    Ok(ProfileSettings {
        full_permission: flag("full_permission")?,
        shared_sessions: flag("shared_sessions")?,
        shared_models: flag("shared_models")?,
        shared_config: flag("shared_config")?,
        shared_skills: flag("shared_skills")?,
        shared_mcp: flag("shared_mcp")?,
    })
}

pub fn replace_profile_paths(text: &str, old: &str, new: &str) -> String {
    let mut pairs = vec![
        (old.replace('\\', "\\\\"), new.replace('\\', "\\\\")),
        (old.replace('\\', "/"), new.replace('\\', "/")),
        (old.into(), new.into()),
    ];
    pairs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    pairs.dedup_by(|left, right| left.0 == right.0);
    let mut output = text.to_owned();
    for (from, to) in pairs {
        let pattern = regex::RegexBuilder::new(&regex::escape(&from))
            .case_insensitive(true)
            .build()
            .expect("escaped path regex");
        let mut replaced = String::new();
        let mut end = 0;
        for matched in pattern.find_iter(&output) {
            let next = output[matched.end()..].chars().next();
            if next.is_some_and(|c| c.is_alphanumeric() || "._-".contains(c)) {
                continue;
            }
            replaced.push_str(&output[end..matched.start()]);
            replaced.push_str(&to);
            end = matched.end();
        }
        replaced.push_str(&output[end..]);
        output = replaced;
    }
    output
}

fn plan_path_updates(
    root: &DataRoot,
    tool: &Tool,
    profiles: &[Profile],
    previous: &Profile,
    next: &Profile,
    transaction: &mut Transaction,
) -> Result<()> {
    let old = root
        .resolve(tool.profile_relative(previous))?
        .display()
        .to_string();
    let new = root
        .resolve(tool.profile_relative(next))?
        .display()
        .to_string();
    let mut paths = tool.paths(crate::domain::ShareCategory::SharedConfig);
    if let Some(rule) = &tool.mcp {
        paths.push(rule.path.clone());
    }
    paths.extend(
        match tool.id.as_str() {
            "claude" => vec![".claude.json"],
            "opencode" => vec![".config/opencode/opencode.jsonc"],
            "aider" => vec![".aider.conf.yml", ".aider.conf.yaml"],
            "crush" => vec![".config/crush/crushrc"],
            _ => vec![],
        }
        .into_iter()
        .map(String::from),
    );
    let mut roots: Vec<_> = profiles
        .iter()
        .map(|profile| {
            (
                tool.profile_relative(profile),
                if profile.id == previous.id {
                    tool.profile_relative(next)
                } else {
                    tool.profile_relative(profile)
                },
            )
        })
        .collect();
    for category in ["sharedConfig", "sharedMcp"] {
        let path = PathBuf::from(&tool.dir).join("_shared").join(category);
        roots.push((path.clone(), path));
    }
    for (source, target) in roots {
        for relative in &paths {
            if let Some(bytes) = root.read(source.join(relative))? {
                if bytes.len() > 16 * 1024 * 1024 {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "Config is too large to update safely",
                    ));
                }
                let text = std::str::from_utf8(&bytes).map_err(Error::corrupt)?;
                let updated = replace_profile_paths(text, &old, &new);
                if updated != text {
                    transaction.change(
                        target.join(relative),
                        Some(bytes),
                        Some(updated.into_bytes()),
                    )?;
                }
            }
        }
    }
    Ok(())
}
