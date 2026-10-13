use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

pub mod artifacts;
pub mod generations;
pub mod transaction;

use serde::{Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::{Error, ErrorKind, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperation {
    Read,
    Create,
    Write,
    Replace,
    Move,
    Remove,
}

pub trait FileObserver: Send + Sync {
    fn before(&self, operation: FileOperation, path: &Path) -> Result<()>;
}

struct NoObserver;
impl FileObserver for NoObserver {
    fn before(&self, _: FileOperation, _: &Path) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct DataRoot {
    path: PathBuf,
    observer: Arc<dyn FileObserver>,
}

impl DataRoot {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        Self::with_observer(path, Arc::new(NoObserver))
    }

    pub fn with_observer(path: impl AsRef<Path>, observer: Arc<dyn FileObserver>) -> Result<Self> {
        let path = path.as_ref();
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "An explicit absolute data root is required",
            ));
        }
        reject_links(path)?;
        observer.before(FileOperation::Create, path)?;
        fs::create_dir_all(path).map_err(|e| Error::io("Create data root", path, e))?;
        let path = fs::canonicalize(path).map_err(|e| Error::io("Resolve data root", path, e))?;
        #[cfg(windows)]
        let path = {
            let text = path
                .to_str()
                .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Data root must be Unicode"))?;
            if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
                PathBuf::from(format!(r"\\{unc}"))
            } else if let Some(local) = text.strip_prefix(r"\\?\") {
                PathBuf::from(local)
            } else {
                path
            }
        };
        Ok(Self { path, observer })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn resolve(&self, relative: impl AsRef<Path>) -> Result<PathBuf> {
        let relative = relative.as_ref();
        if relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Path must stay within the data root",
            ));
        }
        let path = self.path.join(relative);
        reject_links(&path)?;
        Ok(path)
    }

    pub fn read(&self, relative: impl AsRef<Path>) -> Result<Option<Vec<u8>>> {
        let path = self.resolve(relative)?;
        self.observer.before(FileOperation::Read, &path)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(Error::io("Read file", &path, error)),
        }
    }

    pub fn read_json<T: DeserializeOwned>(&self, relative: impl AsRef<Path>) -> Result<Option<T>> {
        self.read(relative)?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Error::corrupt))
            .transpose()
    }

    pub fn read_limited(
        &self,
        relative: impl AsRef<Path>,
        maximum: usize,
    ) -> Result<Option<Vec<u8>>> {
        let path = self.resolve(relative)?;
        self.observer.before(FileOperation::Read, &path)?;
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::io("Open bounded file", &path, error)),
        };
        if file
            .metadata()
            .map_err(|error| Error::io("Inspect bounded file", &path, error))?
            .len()
            > maximum as u64
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Metadata file exceeds its size limit",
            ));
        }
        let mut bytes = Vec::new();
        file.take((maximum as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| Error::io("Read bounded file", &path, error))?;
        if bytes.len() > maximum {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Metadata file grew beyond its size limit",
            ));
        }
        Ok(Some(bytes))
    }

    pub fn create_dir(&self, relative: impl AsRef<Path>) -> Result<PathBuf> {
        let path = self.resolve(relative)?;
        self.observer.before(FileOperation::Create, &path)?;
        fs::create_dir_all(&path).map_err(|e| Error::io("Create directory", &path, e))?;
        Ok(path)
    }

    pub fn create_new_dir(&self, relative: impl AsRef<Path>) -> Result<PathBuf> {
        let relative = relative.as_ref();
        if relative.as_os_str().is_empty() || relative == Path::new(".") {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "A new directory cannot be the data root",
            ));
        }
        let path = self.resolve(relative)?;
        self.observer.before(FileOperation::Create, &path)?;
        if let Some(parent) = relative.parent() {
            self.create_dir(parent)?;
        }
        fs::create_dir(&path)
            .map_err(|error| Error::io("Create exclusive directory", &path, error))?;
        Ok(path)
    }

    pub fn create_new_file(&self, relative: impl AsRef<Path>) -> Result<File> {
        let relative = relative.as_ref();
        let path = self.resolve(relative)?;
        self.observer.before(FileOperation::Write, &path)?;
        if let Some(parent) = relative.parent() {
            self.create_dir(parent)?;
        }
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|error| Error::io("Create exclusive file", &path, error))
    }

    pub fn copy_new_file(
        &self,
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
    ) -> Result<u64> {
        let source = self.resolve(source)?;
        self.observer.before(FileOperation::Read, &source)?;
        let mut input =
            File::open(&source).map_err(|error| Error::io("Open copy source", &source, error))?;
        let destination = destination.as_ref();
        let path = self.resolve(destination)?;
        let mut output = self.create_new_file(destination)?;
        let copied = std::io::copy(&mut input, &mut output)
            .map_err(|error| Error::io("Copy owned file", &path, error))?;
        output
            .sync_all()
            .map_err(|error| Error::io("Sync copied file", &path, error))?;
        Ok(copied)
    }

    pub fn replace_owned_file(
        &self,
        temporary: impl AsRef<Path>,
        destination: impl AsRef<Path>,
    ) -> Result<()> {
        let temporary = self.resolve(temporary)?;
        let destination = self.resolve(destination)?;
        if !temporary.is_file() || destination.exists() && !destination.is_file() {
            return Err(Error::new(
                ErrorKind::Conflict,
                "Shared file target is not a regular file",
            ));
        }
        self.observer.before(FileOperation::Replace, &destination)?;
        fs::rename(&temporary, &destination)
            .map_err(|error| Error::io("Replace owned file", &destination, error))
    }

    pub fn write(&self, relative: impl AsRef<Path>, bytes: &[u8]) -> Result<()> {
        let path = self.resolve(relative)?;
        self.observer.before(FileOperation::Write, &path)?;
        let parent = path
            .parent()
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "File has no parent"))?;
        reject_links(parent)?;
        fs::create_dir_all(parent).map_err(|e| Error::io("Create file parent", parent, e))?;
        let temporary = parent.join(format!(".{}.partial", Uuid::new_v4().simple()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .map_err(|e| Error::io("Create temporary file", &temporary, e))?;
            file.write_all(bytes)
                .and_then(|_| file.sync_all())
                .map_err(|e| Error::io("Write temporary file", &temporary, e))?;
            drop(file);
            self.observer.before(FileOperation::Replace, &path)?;
            fs::rename(&temporary, &path).map_err(|e| Error::io("Replace file", &path, e))
        })();
        if temporary.exists() {
            fs::remove_file(&temporary)
                .map_err(|e| Error::io("Remove temporary file", &temporary, e))?;
        }
        result
    }

    pub fn write_json(&self, relative: impl AsRef<Path>, value: &impl Serialize) -> Result<()> {
        self.write(
            relative,
            &serde_json::to_vec_pretty(value).map_err(Error::corrupt)?,
        )
    }

    pub fn move_path(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
        let from = self.resolve(from)?;
        let to = self.resolve(to)?;
        if to
            .try_exists()
            .map_err(|e| Error::io("Check move target", &to, e))?
        {
            return Err(Error::new(
                ErrorKind::Conflict,
                "Move target already exists",
            ));
        }
        let parent = to
            .parent()
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Move has no target parent"))?;
        fs::create_dir_all(parent).map_err(|e| Error::io("Create move parent", parent, e))?;
        self.observer.before(FileOperation::Move, &from)?;
        fs::rename(&from, &to).map_err(|e| Error::io("Move path", &from, e))
    }

    pub fn remove_file(&self, relative: impl AsRef<Path>) -> Result<()> {
        let path = self.resolve(relative)?;
        self.observer.before(FileOperation::Remove, &path)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::io("Remove file", &path, error)),
        }
    }

    pub fn remove_owned_tree(&self, relative: impl AsRef<Path>) -> Result<()> {
        let relative = relative.as_ref();
        if relative.as_os_str().is_empty() || relative == Path::new(".") {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "The data root cannot be removed",
            ));
        }
        let path = self.resolve(relative)?;
        validate_tree(&path)?;
        self.observer.before(FileOperation::Remove, &path)?;
        match fs::remove_dir_all(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::io("Remove owned tree", &path, error)),
        }
    }
}

pub fn reject_links(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        if !current.is_absolute() {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                #[cfg(windows)]
                let linked = {
                    use std::os::windows::fs::MetadataExt;
                    metadata.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let linked = metadata.file_type().is_symlink();
                if linked {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        format!("Linked paths are not supported: {}", current.display()),
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(Error::io("Inspect path", &current, error)),
        }
    }
    Ok(())
}

fn validate_tree(path: &Path) -> Result<()> {
    reject_links(path)?;
    if !path.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(path).map_err(|e| Error::io("Inspect owned tree", path, e))? {
        let entry = entry.map_err(|e| Error::io("Inspect owned tree entry", path, e))?;
        reject_links(&entry.path())?;
        if entry
            .file_type()
            .map_err(|e| Error::io("Inspect entry type", &entry.path(), e))?
            .is_dir()
        {
            validate_tree(&entry.path())?;
        }
    }
    Ok(())
}
