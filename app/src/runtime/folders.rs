//! Where the app keeps its files: `%LOCALAPPDATA%\Basalt`, with the desktop's own
//! settings in `settings` and the engine's tools, profiles and sessions in `data`.
use std::path::{Path, PathBuf};

/// The folders from before the rename, moved into place on first start.
const EARLIER: &str = "cli.workspace";
const MOVES: [(&str, &str); 2] = [("rust-backend", "data"), ("rust-frontend", "settings")];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folders {
    pub settings: PathBuf,
    pub data: PathBuf,
}

impl Folders {
    /// The app's folders under `local` (normally `%LOCALAPPDATA%`), named after
    /// `application_id`. An install from before the rename is moved over the first
    /// time; if Windows will not move it (a file still open), this run keeps using it
    /// where it is and the move is tried again next start.
    pub fn under(local: &Path, application_id: &str) -> Self {
        let home = local.join(application_id);
        let earlier = local.join(EARLIER);
        if !home.exists() && earlier.is_dir() && !move_earlier(&earlier, &home) {
            return Self {
                settings: earlier.join("rust-frontend"),
                data: earlier.join("rust-backend"),
            };
        }
        Self {
            settings: home.join("settings"),
            data: home.join("data"),
        }
    }
}

/// Moves each earlier folder to its new name, all or nothing.
fn move_earlier(earlier: &Path, home: &Path) -> bool {
    if std::fs::create_dir_all(home).is_err() {
        return false;
    }
    let mut moved = Vec::new();
    for (from, to) in MOVES {
        let (source, target) = (earlier.join(from), home.join(to));
        if !source.exists() {
            continue;
        }
        if std::fs::rename(&source, &target).is_err() {
            for (source, target) in moved.into_iter().rev() {
                let _ = std::fs::rename(target, source);
            }
            let _ = std::fs::remove_dir(home);
            return false;
        }
        moved.push((source, target));
    }
    let _ = std::fs::remove_dir(earlier);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("basalt-folders-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_new_install_gets_its_own_folders() {
        let local = scratch("new");
        let folders = Folders::under(&local, "Basalt");
        assert_eq!(folders.settings, local.join("Basalt/settings"));
        assert_eq!(folders.data, local.join("Basalt/data"));
        std::fs::remove_dir_all(local).unwrap();
    }

    #[test]
    fn an_earlier_install_is_moved_with_its_files() {
        let local = scratch("earlier");
        std::fs::create_dir_all(local.join("cli.workspace/rust-backend/bin")).unwrap();
        std::fs::create_dir_all(local.join("cli.workspace/rust-frontend")).unwrap();
        std::fs::write(local.join("cli.workspace/rust-frontend/workspaces.json"), "{}").unwrap();
        let folders = Folders::under(&local, "Basalt");
        assert!(folders.data.join("bin").is_dir());
        assert!(folders.settings.join("workspaces.json").is_file());
        assert!(!local.join("cli.workspace").exists());
        std::fs::remove_dir_all(local).unwrap();
    }

    #[test]
    fn existing_folders_are_left_alone() {
        let local = scratch("both");
        std::fs::create_dir_all(local.join("Basalt/data")).unwrap();
        std::fs::create_dir_all(local.join("cli.workspace/rust-backend")).unwrap();
        let folders = Folders::under(&local, "Basalt");
        assert_eq!(folders.data, local.join("Basalt/data"));
        assert!(local.join("cli.workspace/rust-backend").is_dir());
        std::fs::remove_dir_all(local).unwrap();
    }
}
