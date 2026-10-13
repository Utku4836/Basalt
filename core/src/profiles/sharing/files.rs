use std::{
    collections::BTreeSet,
    fs::{self, File, FileTimes, OpenOptions},
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use regex::{Regex, RegexBuilder};
use rusqlite::{Connection, OpenFlags, backup::Backup};
use uuid::Uuid;

use crate::{
    Error, ErrorKind, Result, domain::ShareCategory, storage::DataRoot, tools::catalog::Tool,
};

use super::{normalized, relative_path};

struct Rule {
    path: PathBuf,
    expression: Option<Regex>,
}

impl Rule {
    fn parse(value: &str) -> Result<Self> {
        let normalized = value.replace('\\', "/");
        if !normalized.contains('*') {
            return Ok(Self {
                path: relative_path(&normalized)?,
                expression: None,
            });
        }
        let path = PathBuf::from(&normalized);
        let name = path
            .file_name()
            .and_then(|part| part.to_str())
            .ok_or_else(|| Error::new(ErrorKind::CorruptData, "Invalid wildcard rule"))?;
        if !name.contains('*')
            || path
                .parent()
                .is_none_or(|parent| parent.to_string_lossy().contains('*'))
        {
            return Err(Error::new(
                ErrorKind::CorruptData,
                "Wildcard must be in the final file name",
            ));
        }
        let example = normalized.replace('*', "x");
        relative_path(&example)?;
        let pattern = normalized
            .split('*')
            .map(regex::escape)
            .collect::<Vec<_>>()
            .join(".*");
        let expression = RegexBuilder::new(&format!("^{pattern}$"))
            .case_insensitive(true)
            .build()
            .map_err(Error::corrupt)?;
        Ok(Self {
            path,
            expression: Some(expression),
        })
    }

    fn manages(&self, relative: &str) -> Result<bool> {
        let path = relative_path(relative)?;
        if let Some(expression) = &self.expression {
            Ok(expression.is_match(&normalized(&path)?))
        } else {
            Ok(path == self.path || path.starts_with(&self.path))
        }
    }
}

pub(super) struct FileSync<'a> {
    pub root: &'a DataRoot,
    pub source: &'a Path,
    pub target: &'a Path,
    pub tool: &'a Tool,
    pub category: ShareCategory,
    pub rules: &'a [String],
    pub previous: &'a [String],
    pub keep_newer: bool,
}

impl FileSync<'_> {
    pub fn run(&self) -> Result<Vec<String>> {
        let Self {
            root,
            source,
            target,
            tool,
            category,
            rules,
            previous,
            keep_newer,
        } = self;
        let rules: Vec<_> = rules
            .iter()
            .map(|value| Rule::parse(value))
            .collect::<Result<_>>()?;
        let mut current = BTreeSet::new();
        for rule in &rules {
            if let Some(expression) = &rule.expression {
                let parent = rule
                    .path
                    .parent()
                    .ok_or_else(|| Error::new(ErrorKind::CorruptData, "Wildcard has no parent"))?;
                let directory = root.resolve(source.join(parent))?;
                if !directory.exists() {
                    continue;
                }
                for entry in fs::read_dir(&directory)
                    .map_err(|error| Error::io("Read shared wildcard", &directory, error))?
                {
                    let entry =
                        entry.map_err(|error| Error::io("Read shared entry", &directory, error))?;
                    let name = entry.file_name().into_string().map_err(|_| {
                        Error::new(ErrorKind::CorruptData, "Shared file name must be Unicode")
                    })?;
                    crate::domain::windows_component(&name, 255)?;
                    let relative = parent.join(name);
                    if expression.is_match(&normalized(&relative)?) {
                        visit(root, source, &relative, &mut current)?;
                    }
                }
            } else {
                visit(root, source, &rule.path, &mut current)?;
            }
        }
        for name in *previous {
            let relative = relative_path(name)?;
            validate_managed(&rules, name)?;
            if !current.contains(name) {
                archive_removed(root, target, &relative, tool, *category)?;
            }
        }
        for name in &current {
            let relative = relative_path(name)?;
            copy_file(
                root,
                &source.join(&relative),
                &target.join(relative),
                *keep_newer,
            )?;
        }
        Ok(current.into_iter().collect())
    }
}

pub(super) fn validate_previous(rules: &[String], previous: &[String]) -> Result<()> {
    let rules: Vec<_> = rules
        .iter()
        .map(|value| Rule::parse(value))
        .collect::<Result<_>>()?;
    for name in previous {
        validate_managed(&rules, name)?;
    }
    Ok(())
}

fn validate_managed(rules: &[Rule], name: &str) -> Result<()> {
    relative_path(name)?;
    for rule in rules {
        if rule.manages(name)? {
            return Ok(());
        }
    }
    Err(Error::new(
        ErrorKind::CorruptData,
        "Sharing manifest lists an unmanaged path",
    ))
}

fn visit(
    root: &DataRoot,
    base: &Path,
    relative: &Path,
    files: &mut BTreeSet<String>,
) -> Result<()> {
    let path = root.resolve(base.join(relative))?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::io("Inspect shared source", &path, error)),
    };
    if metadata.is_dir() {
        for entry in
            fs::read_dir(&path).map_err(|error| Error::io("Read shared directory", &path, error))?
        {
            let entry = entry.map_err(|error| Error::io("Read shared entry", &path, error))?;
            let name = entry.file_name().into_string().map_err(|_| {
                Error::new(ErrorKind::CorruptData, "Shared file name must be Unicode")
            })?;
            crate::domain::windows_component(&name, 255)?;
            visit(root, base, &relative.join(name), files)?;
        }
    } else if metadata.is_file() && !sqlite_sidecar(&path) {
        if files.len() >= 100_000 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Shared category has too many files",
            ));
        }
        files.insert(normalized(relative)?);
    } else if !metadata.is_file() {
        return Err(Error::new(
            ErrorKind::Conflict,
            "Shared data contains a non-file entry",
        ));
    }
    Ok(())
}

fn sqlite_sidecar(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    [".db-", ".sqlite-"].iter().any(|marker| {
        ["wal", "shm", "journal"]
            .iter()
            .any(|suffix| name.ends_with(&format!("{marker}{suffix}")))
    })
}

fn sqlite_database(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("db") || extension.eq_ignore_ascii_case("sqlite")
        })
}

fn archive_removed(
    root: &DataRoot,
    base: &Path,
    relative: &Path,
    tool: &Tool,
    category: ShareCategory,
) -> Result<()> {
    let from = base.join(relative);
    let path = root.resolve(&from)?;
    if !path.exists() {
        return Ok(());
    }
    if !path.is_file() {
        return Err(Error::new(
            ErrorKind::Conflict,
            "Shared deletion target is not a file",
        ));
    }
    let to = PathBuf::from("_profiles/trash/shared-deletions")
        .join(&tool.id)
        .join(category.key())
        .join(Uuid::new_v4().to_string())
        .join(relative);
    root.move_path(from, to)
}

fn copy_file(root: &DataRoot, source: &Path, destination: &Path, keep_newer: bool) -> Result<()> {
    let source_path = root.resolve(source)?;
    let destination_path = root.resolve(destination)?;
    let source_metadata = fs::metadata(&source_path)
        .map_err(|error| Error::io("Inspect shared source", &source_path, error))?;
    if !source_metadata.is_file() {
        return Err(Error::new(
            ErrorKind::Conflict,
            "Shared source is not a file",
        ));
    }
    if let Ok(existing) = fs::metadata(&destination_path) {
        if !existing.is_file() {
            return Err(Error::new(
                ErrorKind::Conflict,
                "Shared target is not a file",
            ));
        }
        if keep_newer && existing.modified().ok() > source_metadata.modified().ok() {
            return Ok(());
        }
    }
    let parent = destination
        .parent()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Shared target has no parent"))?;
    root.create_dir(parent)?;
    let temporary = parent.join(format!(".shared-{}.partial", Uuid::new_v4().simple()));
    let result = (|| {
        if sqlite_database(source) {
            sqlite_snapshot(root, source, &temporary)?;
        } else {
            let mut input = File::open(&source_path)
                .map_err(|error| Error::io("Open shared source", &source_path, error))?;
            let mut output = root.create_new_file(&temporary)?;
            io::copy(&mut input, &mut output)
                .map_err(|error| Error::io("Copy shared file", &destination_path, error))?;
            output
                .sync_all()
                .map_err(|error| Error::io("Sync shared file", &destination_path, error))?;
        }
        let temporary_path = root.resolve(&temporary)?;
        let file = OpenOptions::new()
            .write(true)
            .open(&temporary_path)
            .map_err(|error| Error::io("Open copied file", &temporary_path, error))?;
        file.set_times(
            FileTimes::new().set_modified(
                source_metadata
                    .modified()
                    .map_err(|error| Error::io("Inspect shared time", &source_path, error))?,
            ),
        )
        .map_err(|error| Error::io("Preserve shared time", &temporary_path, error))?;
        root.replace_owned_file(&temporary, destination)
    })();
    if root.resolve(&temporary)?.exists() {
        root.remove_file(&temporary)?;
    }
    result
}

fn sqlite_snapshot(root: &DataRoot, source: &Path, temporary: &Path) -> Result<()> {
    let source_path = root.resolve(source)?;
    let temporary_path = root.resolve(temporary)?;
    drop(root.create_new_file(temporary)?);
    let from = Connection::open_with_flags(&source_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(Error::corrupt)?;
    let mut to = Connection::open_with_flags(&temporary_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(Error::corrupt)?;
    let backup = Backup::new(&from, &mut to).map_err(Error::corrupt)?;
    backup
        .run_to_completion(256, Duration::from_millis(5), None)
        .map_err(Error::corrupt)?;
    drop(backup);
    to.execute_batch("PRAGMA journal_mode=DELETE;")
        .map_err(Error::corrupt)?;
    drop(to);
    drop(from);
    OpenOptions::new()
        .write(true)
        .open(&temporary_path)
        .and_then(|file| file.sync_all())
        .map_err(|error| Error::io("Sync database snapshot", &temporary_path, error))
}
