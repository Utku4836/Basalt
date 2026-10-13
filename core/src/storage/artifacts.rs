//! Recoverable file/tree replacement without loading artifact contents into RAM.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

use super::{DataRoot, FileOperation, reject_links};
use crate::{
    Error, ErrorKind, Result,
    domain::{same_windows_name, windows_component},
};

const BASE: &str = "_profiles/artifact-transactions";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Signature {
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Change {
    target: PathBuf,
    prepared: Option<PathBuf>,
    before: Option<Signature>,
    after: Option<Signature>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    version: u32,
    id: Uuid,
    committed: bool,
    changes: Vec<Change>,
}

pub struct ArtifactTransaction {
    root: DataRoot,
    journal: Journal,
}

impl ArtifactTransaction {
    pub fn new(root: &DataRoot) -> Result<Self> {
        let journal = Journal {
            version: 1,
            id: Uuid::new_v4(),
            committed: false,
            changes: Vec::new(),
        };
        let directory = directory(journal.id);
        if root.resolve(&directory)?.exists() {
            return Err(Error::new(
                ErrorKind::Conflict,
                "Artifact operation directory exists",
            ));
        }
        root.create_new_dir(&directory)?;
        if let Err(primary) = root.create_dir(directory.join("payload")) {
            return match root.remove_owned_tree(&directory) {
                Ok(()) => Err(primary),
                Err(cleanup) => Err(Error::new(
                    ErrorKind::Recovery,
                    format!("{primary}; prepare cleanup: {cleanup}"),
                )),
            };
        }
        Ok(Self {
            root: root.clone(),
            journal,
        })
    }

    pub fn directory(&self) -> PathBuf {
        directory(self.journal.id)
    }
    pub fn payload(&self) -> PathBuf {
        self.directory().join("payload")
    }

    pub fn replace(&mut self, target: PathBuf, prepared: PathBuf) -> Result<()> {
        if !prepared.starts_with(self.payload()) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Artifact payload must belong to its transaction",
            ));
        }
        let after = fingerprint(&self.root, &prepared)?
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "Prepared artifact does not exist"))?;
        self.add(target, Some(prepared), Some(after))
    }

    pub fn remove(&mut self, target: PathBuf) -> Result<()> {
        self.add(target, None, None)
    }

    fn add(
        &mut self,
        target: PathBuf,
        prepared: Option<PathBuf>,
        after: Option<Signature>,
    ) -> Result<()> {
        validate_target(&self.root, &target)?;
        if self.journal.changes.iter().any(|change| {
            overlaps(&change.target, &target)
                || prepared.as_ref().is_some_and(|next| {
                    change
                        .prepared
                        .as_ref()
                        .is_some_and(|previous| overlaps(previous, next))
                })
        }) {
            return Err(Error::new(ErrorKind::Conflict, "Artifact paths overlap"));
        }
        let before = fingerprint(&self.root, &target)?;
        self.journal.changes.push(Change {
            target,
            prepared,
            before,
            after,
        });
        Ok(())
    }

    /// No public targets are mutated until the complete journal is durable.
    pub fn commit(self) -> Result<Vec<Error>> {
        self.commit_validated(|| Ok(()))
    }

    pub fn commit_validated(mut self, verify: impl FnOnce() -> Result<()>) -> Result<Vec<Error>> {
        let preflight = (|| {
            validate(&self.root, &self.journal)?;
            for change in &self.journal.changes {
                if fingerprint(&self.root, &change.target)? != change.before
                    || prepared_signature(&self.root, change)? != change.after
                {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        "Artifact changed before activation",
                    ));
                }
            }
            self.root
                .write_json(self.directory().join("journal.json"), &self.journal)
        })();
        if let Err(primary) = preflight {
            return match self.root.remove_owned_tree(self.directory()) {
                Ok(()) => Err(primary),
                Err(cleanup) => Err(Error::new(
                    ErrorKind::Recovery,
                    format!("{primary}; prepare cleanup: {cleanup}"),
                )),
            };
        }
        let journal_path = self.directory().join("journal.json");
        let activation = (|| {
            for (index, change) in self.journal.changes.iter().enumerate() {
                if change.before.is_some() {
                    self.root
                        .move_path(&change.target, backup(self.journal.id, index))?;
                }
                if let Some(prepared) = &change.prepared {
                    self.root.move_path(prepared, &change.target)?;
                }
                if fingerprint(&self.root, &change.target)? != change.after
                    || fingerprint(&self.root, &backup(self.journal.id, index))? != change.before
                {
                    return Err(Error::new(
                        ErrorKind::Recovery,
                        "Artifact changed during activation",
                    ));
                }
            }
            verify()?;
            for (index, change) in self.journal.changes.iter().enumerate() {
                if fingerprint(&self.root, &change.target)? != change.after
                    || fingerprint(&self.root, &backup(self.journal.id, index))? != change.before
                {
                    return Err(Error::new(
                        ErrorKind::Recovery,
                        "Artifact changed during final verification",
                    ));
                }
            }
            self.journal.committed = true;
            self.root.write_json(&journal_path, &self.journal)
        })();
        if let Err(primary) = activation {
            self.journal.committed = false;
            return match rollback(&self.root, &self.journal) {
                Ok(()) => Err(primary),
                Err(recovery) => Err(Error::new(
                    ErrorKind::Recovery,
                    format!("{primary}; rollback: {recovery}"),
                )),
            };
        }
        Ok(self
            .root
            .remove_owned_tree(self.directory())
            .err()
            .into_iter()
            .collect())
    }

    /// Call when preparation fails before commit. Never removes public artifacts.
    pub fn discard(self) -> Result<()> {
        self.root.remove_owned_tree(self.directory())
    }
}

pub fn recover(root: &DataRoot) -> Result<Vec<Error>> {
    let base = root.resolve(BASE)?;
    if !base.exists() {
        return Ok(Vec::new());
    }
    let mut warnings = Vec::new();
    let entries = fs::read_dir(&base)
        .map_err(|error| Error::io("Read artifact recovery directory", &base, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| Error::io("Read artifact operation", &base, error))?;
        reject_links(&entry.path())?;
        let id = Uuid::parse_str(&entry.file_name().to_string_lossy()).map_err(Error::corrupt)?;
        let relative = directory(id);
        let Some(journal) = root.read_json::<Journal>(relative.join("journal.json"))? else {
            root.remove_owned_tree(relative)?;
            continue;
        };
        if journal.id != id {
            return Err(Error::new(
                ErrorKind::Recovery,
                "Artifact journal identity mismatch",
            ));
        }
        validate(root, &journal)?;
        if journal.committed {
            // Later legitimate mutations do not invalidate an earlier commit.
            if let Err(error) = root.remove_owned_tree(relative) {
                warnings.push(error);
            }
        } else {
            rollback(root, &journal)?;
        }
    }
    Ok(warnings)
}

fn rollback(root: &DataRoot, journal: &Journal) -> Result<()> {
    validate(root, journal)?;
    for (index, change) in journal.changes.iter().enumerate().rev() {
        let backup = backup(journal.id, index);
        let current = fingerprint(root, &change.target)?;
        let saved = fingerprint(root, &backup)?;
        let prepared = prepared_signature(root, change)?;
        if saved.is_some() && saved != change.before {
            return Err(Error::new(
                ErrorKind::Recovery,
                "Artifact backup changed outside its transaction",
            ));
        }
        if prepared.is_some() && prepared != change.after {
            return Err(Error::new(
                ErrorKind::Recovery,
                "Prepared artifact changed outside its transaction",
            ));
        }
        if saved == change.before && saved.is_some() {
            if current.is_some() {
                if current != change.after || prepared.is_some() {
                    return Err(Error::new(
                        ErrorKind::Recovery,
                        "Artifact target changed outside its transaction",
                    ));
                }
                root.move_path(
                    &change.target,
                    change.prepared.as_ref().ok_or_else(|| {
                        Error::new(ErrorKind::Recovery, "Unexpected target after deletion")
                    })?,
                )?;
            }
            root.move_path(&backup, &change.target)?;
        } else if change.before.is_none() {
            if current.is_some() {
                if current != change.after || prepared.is_some() {
                    return Err(Error::new(
                        ErrorKind::Recovery,
                        "New artifact was modified outside its transaction",
                    ));
                }
                root.move_path(
                    &change.target,
                    change.prepared.as_ref().ok_or_else(|| {
                        Error::new(ErrorKind::Recovery, "Unexpected new artifact")
                    })?,
                )?;
            }
        } else if current != change.before {
            return Err(Error::new(
                ErrorKind::Recovery,
                "Original artifact and backup are missing or changed",
            ));
        }
    }
    root.remove_owned_tree(directory(journal.id))
}

fn validate(root: &DataRoot, journal: &Journal) -> Result<()> {
    if journal.version != 1 || journal.changes.len() > 10000 {
        return Err(Error::new(
            ErrorKind::Recovery,
            "Unsupported artifact journal",
        ));
    }
    for (index, change) in journal.changes.iter().enumerate() {
        validate_target(root, &change.target)?;
        root.resolve(backup(journal.id, index))?;
        if change.prepared.is_some() != change.after.is_some() {
            return Err(Error::new(
                ErrorKind::Recovery,
                "Invalid artifact payload identity",
            ));
        }
        if let Some(path) = &change.prepared {
            root.resolve(path)?;
            if !path.starts_with(directory(journal.id).join("payload")) {
                return Err(Error::new(
                    ErrorKind::Recovery,
                    "Artifact payload is not operation-owned",
                ));
            }
        }
        for signature in [&change.before, &change.after].into_iter().flatten() {
            if signature.sha256.len() != 64
                || !signature
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(Error::new(
                    ErrorKind::Recovery,
                    "Invalid artifact fingerprint",
                ));
            }
        }
        for previous in &journal.changes[..index] {
            if overlaps(&previous.target, &change.target)
                || previous
                    .prepared
                    .as_ref()
                    .zip(change.prepared.as_ref())
                    .is_some_and(|(a, b)| overlaps(a, b))
            {
                return Err(Error::new(
                    ErrorKind::Recovery,
                    "Artifact journal has overlapping paths",
                ));
            }
        }
    }
    Ok(())
}

fn validate_target(root: &DataRoot, path: &Path) -> Result<()> {
    root.resolve(path)?;
    for component in path.components() {
        if let Component::Normal(name) = component {
            windows_component(
                name.to_str().ok_or_else(|| {
                    Error::new(ErrorKind::InvalidInput, "Artifact paths must be Unicode")
                })?,
                255,
            )?;
        }
    }
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        || path
            .components()
            .next()
            .is_some_and(|part| same_windows_name(&part.as_os_str().to_string_lossy(), "_profiles"))
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Artifact target must be a public data path, not control metadata",
        ));
    }
    Ok(())
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.components().zip(b.components()).all(|(a, b)| {
        same_windows_name(
            &a.as_os_str().to_string_lossy(),
            &b.as_os_str().to_string_lossy(),
        )
    })
}
fn directory(id: Uuid) -> PathBuf {
    PathBuf::from(BASE).join(id.to_string())
}
fn backup(id: Uuid, index: usize) -> PathBuf {
    directory(id).join("before").join(index.to_string())
}
fn prepared_signature(root: &DataRoot, change: &Change) -> Result<Option<Signature>> {
    change
        .prepared
        .as_ref()
        .map(|path| fingerprint(root, path))
        .transpose()
        .map(Option::flatten)
}

fn fingerprint(root: &DataRoot, relative: &Path) -> Result<Option<Signature>> {
    let path = root.resolve(relative)?;
    if !path
        .try_exists()
        .map_err(|error| Error::io("Inspect artifact", &path, error))?
    {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    let mut entries = 0;
    hash_path(root, &path, &mut hasher, 0, &mut entries)?;
    Ok(Some(Signature {
        sha256: format!("{:x}", hasher.finalize()),
    }))
}

fn hash_path(
    root: &DataRoot,
    path: &Path,
    hasher: &mut Sha256,
    depth: usize,
    entries: &mut usize,
) -> Result<()> {
    reject_links(path)?;
    *entries += 1;
    if depth > 128 || *entries > 200000 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Artifact tree exceeds inspection limits",
        ));
    }
    root.observer.before(FileOperation::Read, path)?;
    let metadata =
        fs::metadata(path).map_err(|error| Error::io("Inspect artifact type", path, error))?;
    if metadata.is_dir() {
        hasher.update(b"D");
        let mut children = fs::read_dir(path)
            .map_err(|error| Error::io("Read artifact tree", path, error))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|error| Error::io("Read artifact child", path, error))?;
        children.sort();
        hasher.update((children.len() as u64).to_le_bytes());
        for child in children {
            let name = child
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    Error::new(ErrorKind::InvalidInput, "Artifact names must be Unicode")
                })?;
            hasher.update((name.len() as u64).to_le_bytes());
            hasher.update(name.as_bytes());
            hash_path(root, &child, hasher, depth + 1, entries)?;
        }
    } else if metadata.is_file() {
        hasher.update(b"F");
        hasher.update(metadata.len().to_le_bytes());
        let mut file = fs::File::open(path)
            .map_err(|error| Error::io("Open artifact for hashing", path, error))?;
        let mut buffer = [0; 65536];
        let mut bytes = 0;
        loop {
            let length = file
                .read(&mut buffer)
                .map_err(|error| Error::io("Hash artifact", path, error))?;
            if length == 0 {
                break;
            }
            bytes += length as u64;
            hasher.update(&buffer[..length]);
        }
        if bytes != metadata.len() {
            return Err(Error::new(
                ErrorKind::Conflict,
                "Artifact changed while hashing",
            ));
        }
    } else {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Unsupported artifact type",
        ));
    }
    Ok(())
}
