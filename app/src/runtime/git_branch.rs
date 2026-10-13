//! Current Git branch of a workspace folder, read from `.git/HEAD` without
//! spawning git. Worktrees and submodules use a `.git` file with `gitdir:`.
use std::path::{Path, PathBuf};

fn git_dir(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let dot = dir.join(".git");
        if dot.is_dir() {
            return Some(dot);
        }
        if dot.is_file() {
            let text = std::fs::read_to_string(&dot).ok()?;
            let target = text.strip_prefix("gitdir:")?.trim();
            let path = Path::new(target);
            return Some(if path.is_absolute() { path.to_path_buf() } else { dir.join(path) });
        }
    }
    None
}

/// Branch name, or a short commit id for a detached HEAD.
pub fn branch(folder: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir(folder)?.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(reference) => Some(reference.strip_prefix("refs/heads/").unwrap_or(reference).to_string()),
        None if head.len() >= 7 && head.bytes().all(|b| b.is_ascii_hexdigit()) => Some(head[..7].to_string()),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_branches_detached_heads_and_worktree_links() {
        let root = std::env::temp_dir().join(format!("git-branch-{}", std::process::id()));
        let repo = root.join("repo");
        let nested = repo.join("src").join("deep");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/ui\n").unwrap();
        assert_eq!(super::branch(&nested).as_deref(), Some("feature/ui"));
        std::fs::write(repo.join(".git/HEAD"), "673ab53c0ffee00000000000000000000000000\n").unwrap();
        assert_eq!(super::branch(&repo).as_deref(), Some("673ab53"));
        let worktree = root.join("worktree");
        std::fs::create_dir_all(root.join("meta")).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(root.join("meta/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: ../meta\n").unwrap();
        assert_eq!(super::branch(&worktree).as_deref(), Some("main"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
