//! Immutable final paths for non-relocatable runtimes; only the launch record swaps.
use super::DataRoot;
use crate::{
    Error, ErrorKind, Result,
    domain::windows_component,
    tools::catalog::Tool,
    tools::install::command::{RECORD, RuntimeRecord},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const JOURNALS: &str = "_profiles/install-generations";
const MARKER: &str = ".generation.json";

#[derive(Serialize, Deserialize)]
struct Journal {
    schema: u32,
    id: Uuid,
    tool_id: String,
    tool_dir: String,
}
pub struct Generation {
    root: DataRoot,
    journal: Journal,
}

impl Generation {
    pub fn new(root: &DataRoot, tool: &Tool) -> Result<Self> {
        windows_component(&tool.dir, 255)?;
        let journal = Journal {
            schema: 1,
            id: Uuid::new_v4(),
            tool_id: tool.id.clone(),
            tool_dir: tool.dir.clone(),
        };
        let value = Self {
            root: root.clone(),
            journal,
        };
        let path = value.path();
        if root.resolve(&path)?.exists() {
            return Err(Error::new(
                ErrorKind::Conflict,
                "Runtime generation already exists",
            ));
        }
        root.write_json(value.journal_path(), &value.journal)?;
        if let Err(primary) = root.create_new_dir(&path) {
            return match root.remove_file(value.journal_path()) {
                Ok(()) => Err(primary),
                Err(cleanup) => Err(Error::new(
                    ErrorKind::Recovery,
                    format!("{primary}; generation journal cleanup: {cleanup}"),
                )),
            };
        }
        if let Err(primary) = root.write_json(path.join(MARKER), &value.journal) {
            return match value.discard() {
                Ok(()) => Err(primary),
                Err(cleanup) => Err(Error::new(
                    ErrorKind::Recovery,
                    format!("{primary}; generation cleanup: {cleanup}"),
                )),
            };
        }
        Ok(value)
    }
    pub fn relative(&self) -> PathBuf {
        PathBuf::from("versions").join(self.journal.id.to_string())
    }
    pub fn path(&self) -> PathBuf {
        PathBuf::from(&self.journal.tool_dir).join(self.relative())
    }
    pub fn discard(self) -> Result<()> {
        remove_owned(&self.root, &self.journal)?;
        self.root.remove_file(self.journal_path())
    }
    pub fn published(self) -> Result<Vec<Error>> {
        let record = read_record(&self.root, &self.journal)?;
        if !is_active(&self.journal, record.as_ref()) {
            return Err(Error::new(
                ErrorKind::Recovery,
                "Published runtime generation is not the selected command",
            ));
        }
        Ok(self
            .root
            .remove_file(self.journal_path())
            .err()
            .into_iter()
            .collect())
    }
    fn journal_path(&self) -> PathBuf {
        PathBuf::from(JOURNALS).join(format!("{}.json", self.journal.id))
    }
}

pub fn recover(root: &DataRoot) -> Result<Vec<Error>> {
    let base = root.resolve(JOURNALS)?;
    if !base.exists() {
        return Ok(Vec::new());
    }
    for entry in
        fs::read_dir(&base).map_err(|error| Error::io("Read generation journals", &base, error))?
    {
        let entry = entry.map_err(|error| Error::io("Read generation journal", &base, error))?;
        super::reject_links(&entry.path())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let id =
            Uuid::parse_str(name.strip_suffix(".json").ok_or_else(|| {
                Error::new(ErrorKind::Recovery, "Invalid generation journal name")
            })?)
            .map_err(Error::corrupt)?;
        let relative = PathBuf::from(JOURNALS).join(&name);
        let bytes = root
            .read_limited(&relative, 16384)?
            .ok_or_else(|| Error::new(ErrorKind::Recovery, "Generation journal disappeared"))?;
        let journal: Journal = serde_json::from_slice(&bytes).map_err(Error::corrupt)?;
        validate(&journal, id)?;
        if !is_active(&journal, read_record(root, &journal)?.as_ref()) {
            remove_owned(root, &journal)?;
        }
        root.remove_file(relative)?;
    }
    Ok(Vec::new())
}

pub fn cleanup_previous(
    root: &DataRoot,
    tool: &Tool,
    previous: &Path,
    current: &Path,
) -> Result<()> {
    if previous == current {
        return Ok(());
    }
    let mut components = previous.components();
    if components.next().map(|part| part.as_os_str()) != Some(std::ffi::OsStr::new("versions")) {
        return Err(Error::new(
            ErrorKind::Recovery,
            "Previous generation is outside managed versions",
        ));
    }
    let id = components
        .next()
        .and_then(|part| part.as_os_str().to_str())
        .and_then(|name| Uuid::parse_str(name).ok())
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Recovery,
                "Previous generation has no ownership identity",
            )
        })?;
    if components.next().is_some() {
        return Err(Error::new(
            ErrorKind::Recovery,
            "Invalid previous generation path",
        ));
    }
    let journal = Journal {
        schema: 1,
        id,
        tool_id: tool.id.clone(),
        tool_dir: tool.dir.clone(),
    };
    if is_active(&journal, read_record(root, &journal)?.as_ref()) {
        return Err(Error::new(
            ErrorKind::Recovery,
            "An active runtime generation cannot be cleaned",
        ));
    }
    remove_owned(root, &journal)
}

fn validate(journal: &Journal, id: Uuid) -> Result<()> {
    windows_component(&journal.tool_dir, 255)?;
    if journal.schema != 1 || journal.id != id || journal.tool_id.is_empty() {
        return Err(Error::new(
            ErrorKind::Recovery,
            "Invalid runtime generation journal",
        ));
    }
    Ok(())
}
fn path(journal: &Journal) -> PathBuf {
    PathBuf::from(&journal.tool_dir)
        .join("versions")
        .join(journal.id.to_string())
}
fn is_active(journal: &Journal, record: Option<&RuntimeRecord>) -> bool {
    record.is_some_and(|record| {
        record.schema == 1
            && record.tool_id == journal.tool_id
            && record.generation.as_deref()
                == Some(Path::new("versions").join(journal.id.to_string()).as_path())
    })
}
fn read_record(root: &DataRoot, journal: &Journal) -> Result<Option<RuntimeRecord>> {
    root.read_limited(PathBuf::from(&journal.tool_dir).join(RECORD), 1024 * 1024)?
        .map(|bytes| serde_json::from_slice(&bytes).map_err(Error::corrupt))
        .transpose()
}
fn remove_owned(root: &DataRoot, journal: &Journal) -> Result<()> {
    let directory = path(journal);
    let absolute = root.resolve(&directory)?;
    if !absolute.exists() {
        return Ok(());
    }
    if let Some(bytes) = root.read_limited(directory.join(MARKER), 16384)? {
        let marker: Journal = serde_json::from_slice(&bytes).map_err(Error::corrupt)?;
        if marker.id != journal.id
            || marker.tool_id != journal.tool_id
            || marker.tool_dir != journal.tool_dir
            || marker.schema != 1
        {
            return Err(Error::new(
                ErrorKind::Recovery,
                "Runtime generation ownership does not match",
            ));
        }
    } else if fs::read_dir(&absolute)
        .map_err(|error| Error::io("Inspect unmarked generation", &absolute, error))?
        .next()
        .is_some()
    {
        return Err(Error::new(
            ErrorKind::Recovery,
            "Unmarked runtime generation contains foreign data",
        ));
    }
    root.remove_owned_tree(directory)
}
