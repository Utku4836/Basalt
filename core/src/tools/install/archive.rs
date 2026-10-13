use serde::Serialize;
use std::{
    fs::File,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use zip::ZipArchive;
mod envelope;

use crate::{
    Error, ErrorKind, Result, domain::windows_component, storage::DataRoot,
    system::process::CancellationToken,
};

#[derive(Clone, Copy)]
pub struct Limits {
    pub entries: usize,
    pub expanded_bytes: u64,
    pub file_bytes: u64,
    pub compressed_bytes: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: 100000,
            expanded_bytes: 4 * 1024 * 1024 * 1024,
            file_bytes: 2 * 1024 * 1024 * 1024,
            compressed_bytes: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Extracted {
    pub files: usize,
    pub directories: usize,
    pub bytes: u64,
}

/// Extract only into a newly owned directory, never an existing runtime/workspace.
/// A failed extraction removes only that directory; activation is a later transaction.
pub fn extract_zip(
    root: &DataRoot,
    archive: &Path,
    destination: &Path,
    limits: Limits,
    cancel: &CancellationToken,
) -> Result<Extracted> {
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    if limits.entries == 0
        || limits.entries > 200000
        || limits.file_bytes == 0
        || limits.file_bytes > limits.expanded_bytes
        || limits.compressed_bytes == 0
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Invalid archive limits",
        ));
    }
    let source = root.resolve(archive)?;
    let mut input =
        File::open(&source).map_err(|error| Error::io("Open release archive", &source, error))?;
    if input
        .metadata()
        .map_err(|error| Error::io("Inspect archive size", &source, error))?
        .len()
        > limits.compressed_bytes
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Release archive exceeds its compressed size limit",
        ));
    }
    let declared_entries = envelope::inspect(&mut input, limits.entries)?;
    let mut zip = ZipArchive::new(input).map_err(archive_error)?;
    if zip.len() as u64 != declared_entries {
        return Err(Error::new(
            ErrorKind::CorruptData,
            "Archive has duplicate or inconsistent directory entries",
        ));
    }
    if zip.len() > limits.entries {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Release archive has too many entries",
        ));
    }
    root.create_new_dir(destination)?;
    let result = (|| {
        let mut receipt = Extracted {
            files: 0,
            directories: 0,
            bytes: 0,
        };
        let mut buffer = [0; 65536];
        for index in 0..zip.len() {
            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            let mut entry = zip.by_index(index).map_err(archive_error)?;
            let relative = entry_path(entry.name())?;
            if entry
                .unix_mode()
                .is_some_and(|mode| !matches!(mode & 0o170000, 0 | 0o040000 | 0o100000))
            {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Archive links and special files are not supported",
                ));
            }
            let target = destination.join(relative);
            if entry.is_dir() {
                root.create_dir(&target)?;
                receipt.directories += 1;
                continue;
            }
            if entry.size() > limits.file_bytes
                || receipt
                    .bytes
                    .checked_add(entry.size())
                    .is_none_or(|bytes| bytes > limits.expanded_bytes)
            {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Archive exceeds its expanded size limit",
                ));
            }
            let mut output = root.create_new_file(&target)?;
            let resolved_target = root.resolve(&target)?;
            let mut file_bytes = 0;
            loop {
                if cancel.is_cancelled() {
                    return Err(cancelled());
                }
                let length = entry.read(&mut buffer).map_err(|error| {
                    Error::new(
                        ErrorKind::CorruptData,
                        format!("Read archive entry: {error}"),
                    )
                })?;
                if length == 0 {
                    break;
                }
                file_bytes += length as u64;
                if file_bytes > entry.size() || file_bytes > limits.file_bytes {
                    return Err(Error::new(
                        ErrorKind::CorruptData,
                        "Archive entry exceeds its declared size",
                    ));
                }
                output
                    .write_all(&buffer[..length])
                    .map_err(|error| Error::io("Write archive entry", &resolved_target, error))?;
            }
            if file_bytes != entry.size() {
                return Err(Error::new(
                    ErrorKind::CorruptData,
                    "Archive entry is incomplete",
                ));
            }
            output
                .sync_all()
                .map_err(|error| Error::io("Sync archive entry", &resolved_target, error))?;
            receipt.files += 1;
            receipt.bytes += file_bytes;
        }
        Ok(receipt)
    })();
    if let Err(primary) = result {
        return match root.remove_owned_tree(destination) {
            Ok(()) => Err(primary),
            Err(cleanup) => Err(Error::new(
                ErrorKind::Recovery,
                format!("{primary}; extraction cleanup: {cleanup}"),
            )),
        };
    }
    result
}

fn entry_path(name: &str) -> Result<PathBuf> {
    let normalized = name.replace('\\', "/");
    let mut path = PathBuf::new();
    for component in Path::new(&normalized).components() {
        match component {
            Component::Normal(name) => {
                windows_component(
                    name.to_str().ok_or_else(|| {
                        Error::new(ErrorKind::InvalidInput, "Archive names must be Unicode")
                    })?,
                    255,
                )?;
                path.push(name);
            }
            Component::CurDir => (),
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Archive entry escapes its extraction directory",
                ));
            }
        }
    }
    if path.as_os_str().is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput, "Empty archive entry"));
    }
    Ok(path)
}
fn cancelled() -> Error {
    Error::new(ErrorKind::Cancelled, "Archive extraction cancelled")
}
fn archive_error(error: zip::result::ZipError) -> Error {
    Error::new(
        ErrorKind::CorruptData,
        format!("Read release archive: {error}"),
    )
}
