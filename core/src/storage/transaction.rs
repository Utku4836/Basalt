use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Error, ErrorKind, Result, storage::DataRoot};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MovePlan {
    pub from: PathBuf,
    pub to: PathBuf,
    pub via: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journal {
    version: u32,
    id: Uuid,
    committed: bool,
    movement: Option<MovePlan>,
    changes: Vec<FileChange>,
}

pub struct Transaction {
    root: DataRoot,
    journal: Journal,
}

impl Transaction {
    pub fn new(root: &DataRoot) -> Self {
        Self {
            root: root.clone(),
            journal: Journal {
                version: 1,
                id: Uuid::new_v4(),
                committed: false,
                movement: None,
                changes: Vec::new(),
            },
        }
    }

    pub fn directory(&self) -> PathBuf {
        operation_directory(self.journal.id)
    }

    pub fn prepare_directory(&self) -> Result<PathBuf> {
        let path = self.directory().join("prepared");
        self.root.create_dir(&path)?;
        Ok(path)
    }

    pub fn move_directory(&mut self, from: PathBuf, to: PathBuf) {
        self.journal.movement = Some(MovePlan {
            from,
            to,
            via: self.directory().join("moving"),
        });
    }

    pub fn change(
        &mut self,
        path: PathBuf,
        before: Option<Vec<u8>>,
        after: Option<Vec<u8>>,
    ) -> Result<()> {
        self.root.resolve(&path)?;
        if self
            .journal
            .changes
            .iter()
            .any(|change| change.path == path)
        {
            return Err(Error::new(
                ErrorKind::Conflict,
                "A transaction cannot modify the same path twice",
            ));
        }
        self.journal.changes.push(FileChange {
            path,
            before,
            after,
        });
        Ok(())
    }

    pub fn write(&mut self, path: PathBuf, after: Vec<u8>) -> Result<()> {
        let before = self.root.read(&path)?;
        self.change(path, before, Some(after))
    }

    pub fn commit(mut self) -> Result<Vec<Error>> {
        let journal_path = self.directory().join("journal.json");
        if let Err(error) = self.root.write_json(&journal_path, &self.journal) {
            let cleanup = self.root.remove_owned_tree(self.directory());
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(recovery_error(error, cleanup)),
            };
        }
        let result = (|| {
            if let Some(movement) = &self.journal.movement {
                self.root.move_path(&movement.from, &movement.via)?;
                self.root.move_path(&movement.via, &movement.to)?;
            }
            for change in &self.journal.changes {
                apply(&self.root, &change.path, &change.after)?;
            }
            self.journal.committed = true;
            self.root.write_json(&journal_path, &self.journal)
        })();
        if let Err(error) = result {
            self.journal.committed = false;
            return match rollback(&self.root, &self.journal) {
                Ok(()) => Err(error),
                Err(rollback) => Err(recovery_error(error, rollback)),
            };
        }
        let warnings = self
            .root
            .remove_owned_tree(self.directory())
            .err()
            .into_iter()
            .collect();
        Ok(warnings)
    }
}

pub fn recover(root: &DataRoot) -> Result<Vec<Error>> {
    let directory = root.resolve("_profiles/transactions")?;
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut warnings = Vec::new();
    for entry in
        fs::read_dir(&directory).map_err(|e| Error::io("Read transactions", &directory, e))?
    {
        let entry = entry.map_err(|e| Error::io("Read transaction entry", &directory, e))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::new(ErrorKind::CorruptData, "Invalid transaction directory"))?;
        let id = Uuid::parse_str(&name).map_err(Error::corrupt)?;
        let relative = operation_directory(id);
        let journal: Option<Journal> = root.read_json(relative.join("journal.json"))?;
        let Some(journal) = journal else {
            // No mutation may start until its journal is durable.
            root.remove_owned_tree(&relative)?;
            continue;
        };
        if journal.version != 1 || journal.id != id {
            return Err(Error::new(
                ErrorKind::Recovery,
                "Transaction identity mismatch",
            ));
        }
        validate(root, &journal)?;
        if journal.committed {
            // Later legitimate transactions may already have changed the same files.
            // A durable commit marker only leaves operation-owned scratch to remove.
            if let Err(error) = root.remove_owned_tree(&relative) {
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
    let location = journal
        .movement
        .as_ref()
        .map(|movement| locate(root, movement))
        .transpose()?;
    for change in journal.changes.iter().rev() {
        let mut path = change.path.clone();
        if let (Some(movement), Some(location)) = (&journal.movement, &location) {
            if let Ok(suffix) = path.strip_prefix(&movement.to) {
                path = location.join(suffix);
            }
        }
        let current = root.read(&path)?;
        let known = journal.changes.iter().any(|candidate| {
            crate::domain::same_windows_name(
                &candidate.path.to_string_lossy(),
                &change.path.to_string_lossy(),
            ) && (current == candidate.before || current == candidate.after)
        });
        if !known {
            return Err(Error::new(
                ErrorKind::Recovery,
                format!("File changed outside its transaction: {}", path.display()),
            ));
        }
        apply(root, &path, &change.before)?;
    }
    if let (Some(movement), Some(location)) = (&journal.movement, location) {
        if location != movement.from {
            if exact_exists(root, &movement.from)? {
                return Err(Error::new(
                    ErrorKind::Recovery,
                    "Rollback source already exists",
                ));
            }
            if location != movement.via {
                root.move_path(&location, &movement.via)?;
                root.move_path(&movement.via, &movement.from)?;
            } else {
                root.move_path(&location, &movement.from)?;
            }
        }
    }
    root.remove_owned_tree(operation_directory(journal.id))
}

fn locate(root: &DataRoot, movement: &MovePlan) -> Result<PathBuf> {
    let candidates = [&movement.from, &movement.via, &movement.to];
    let existing = candidates
        .into_iter()
        .filter_map(|path| match exact_exists(root, path) {
            Ok(true) => Some(Ok(path.clone())),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>>>()?;
    if existing.len() != 1 {
        return Err(Error::new(
            ErrorKind::Recovery,
            "Ambiguous transaction move state",
        ));
    }
    Ok(existing[0].clone())
}

fn exact_exists(root: &DataRoot, relative: &Path) -> Result<bool> {
    let path = root.resolve(relative)?;
    if !path
        .try_exists()
        .map_err(|e| Error::io("Inspect transaction path", &path, e))?
    {
        return Ok(false);
    }
    let Some(parent) = path.parent() else {
        return Ok(true);
    };
    for entry in
        fs::read_dir(parent).map_err(|e| Error::io("Inspect transaction parent", parent, e))?
    {
        let entry = entry.map_err(|e| Error::io("Inspect transaction name", parent, e))?;
        if Some(entry.file_name().as_os_str()) == path.file_name() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn validate(root: &DataRoot, journal: &Journal) -> Result<()> {
    for change in &journal.changes {
        root.resolve(&change.path)?;
    }
    if let Some(movement) = &journal.movement {
        root.resolve(&movement.from)?;
        root.resolve(&movement.to)?;
        root.resolve(&movement.via)?;
        if movement.via != operation_directory(journal.id).join("moving") {
            return Err(Error::new(
                ErrorKind::Recovery,
                "Invalid transaction staging path",
            ));
        }
    }
    Ok(())
}

fn apply(root: &DataRoot, path: &Path, contents: &Option<Vec<u8>>) -> Result<()> {
    match contents {
        Some(bytes) => root.write(path, bytes),
        None => root.remove_file(path),
    }
}

fn operation_directory(id: Uuid) -> PathBuf {
    PathBuf::from("_profiles/transactions").join(id.to_string())
}

fn recovery_error(primary: Error, cleanup: Error) -> Error {
    Error::new(
        ErrorKind::Recovery,
        format!("{primary}; recovery is required: {cleanup}"),
    )
}
