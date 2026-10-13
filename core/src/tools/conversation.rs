//! CLI-native conversation selection. Never substitute another terminal's latest chat.
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Conversation {
    pub tool: String,
    pub id: Option<String>,
    pub directory: PathBuf,
    pub started: bool,
}
impl Conversation {
    pub fn new(tool: &str, directory: PathBuf) -> Self {
        let id = matches!(tool, "claude" | "copilot" | "qwen" | "kimi" | "goose")
            .then(|| uuid::Uuid::new_v4().to_string());
        Self {
            tool: tool.into(),
            id,
            directory,
            started: false,
        }
    }
    pub fn validate(&self) -> Result<()> {
        crate::tools::catalog::Catalog::built_in().get(&self.tool)?;
        if !self.directory.is_absolute() || self.id.as_ref().is_some_and(|id| !valid_id(id)) {
            return Err(Error::new(
                ErrorKind::CorruptData,
                "Invalid conversation reference",
            ));
        }
        Ok(())
    }
    pub fn arguments(&self) -> Result<Vec<String>> {
        self.validate()?;
        let pair = |flag: &str, id: &str| vec![flag.to_owned(), id.to_owned()];
        let id = self.id.as_deref();
        match self.tool.as_str() {
            "claude" | "qwen" => Ok(pair(
                if self.started {
                    "--resume"
                } else {
                    "--session-id"
                },
                id.ok_or_else(missing)?,
            )),
            "copilot" => Ok(pair("--session-id", id.ok_or_else(missing)?)),
            "kimi" => Ok(pair("--session", id.ok_or_else(missing)?)),
            "goose" => {
                let mut args = vec![
                    "session".into(),
                    "--name".into(),
                    format!("basalt-{}", id.ok_or_else(missing)?),
                ];
                if self.started {
                    args.extend(["--resume".into(), "--history".into()]);
                }
                Ok(args)
            }
            "aider" => {
                let mut args = vec![];
                for (flag, file) in [
                    ("--chat-history-file", "chat.history.md"),
                    ("--input-history-file", "input.history"),
                    ("--llm-history-file", "llm.history"),
                ] {
                    args.extend([
                        flag.into(),
                        self.directory.join(file).to_string_lossy().into_owned(),
                    ]);
                }
                if self.started {
                    args.push("--restore-chat-history".into());
                }
                Ok(args)
            }
            "crush" => {
                let mut args = pair(
                    "--data-dir",
                    &self.directory.join("crush").to_string_lossy(),
                );
                if self.started {
                    if let Some(id) = id {
                        args.extend(pair("--session", id));
                    } else {
                        args.push("--continue".into());
                    }
                }
                Ok(args)
            }
            "agy" if self.started => Ok(id
                .map(|id| pair("--conversation", id))
                .unwrap_or_else(|| vec!["--continue".into()])),
            "codex" if self.started => Ok(vec!["resume".into(), id.ok_or_else(missing)?.into()]),
            "opencode" | "cursor-agent" if self.started => Ok(pair(
                if self.tool == "opencode" {
                    "--session"
                } else {
                    "--resume"
                },
                id.ok_or_else(missing)?,
            )),
            "amp" if self.started => Ok(vec![
                "threads".into(),
                "continue".into(),
                id.ok_or_else(missing)?.into(),
            ]),
            _ => Ok(vec![]),
        }
    }
    /// Read only the owned profile's session metadata, scoped to this terminal's
    /// working directory. UI hooks remain authoritative when available.
    pub fn discover(&self, profile_home: &Path) -> Option<String> {
        if self.tool == "agy" {
            let map: serde_json::Value = serde_json::from_slice(
                &std::fs::read(
                    profile_home.join(".gemini/antigravity-cli/cache/last_conversations.json"),
                )
                .ok()?,
            )
            .ok()?;
            return map
                .get(self.directory.to_str()?)?
                .as_str()
                .filter(|id| valid_id(id))
                .map(str::to_owned);
        }
        let roots: Vec<&str> = match self.tool.as_str() {
            "amp" => vec![".local/share/amp/threads"],
            "codex" => vec![".codex/sessions"],
            "opencode" => vec![".local/share/opencode"],
            "cursor-agent" => vec![".cursor/chats"],
            _ => return None,
        };
        let mut files = vec![];
        for root in roots {
            collect(&profile_home.join(root), 0, &mut files);
        }
        files.sort_by_key(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok());
        for path in files.iter().rev() {
            let meta = std::fs::metadata(path).ok()?;
            if path
                .extension()
                .is_some_and(|extension| extension == "db" || extension == "sqlite")
            {
                if let Some(id) = sqlite_id(path, &self.directory) {
                    return Some(id);
                }
                continue;
            }
            if meta.len() > 16 * 1024 * 1024 {
                continue;
            }
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            for line in bytes.split(|byte| *byte == b'\n').take(32) {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) {
                    if let Some(id) = metadata_id(&value, &self.directory, 0) {
                        return Some(id);
                    }
                }
            }
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Some(id) = metadata_id(&value, &self.directory, 0) {
                    return Some(id);
                }
            }
        }
        None
    }
}
fn missing() -> Error {
    Error::new(
        ErrorKind::NotFound,
        "The saved conversation has not been identified yet. Its session data is retained; restart it after the CLI records its session ID.",
    )
}
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:".contains(&byte))
}
fn collect(path: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    if depth > 8 || files.len() > 2048 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect(&entry.path(), depth + 1, files);
        } else if entry
            .path()
            .extension()
            .is_some_and(|ext| ext == "json" || ext == "jsonl" || ext == "db" || ext == "sqlite")
        {
            files.push(entry.path());
        }
        if files.len() > 2048 {
            break;
        }
    }
}
fn metadata_id(value: &serde_json::Value, cwd: &Path, depth: usize) -> Option<String> {
    if depth > 6 {
        return None;
    }
    if let Some(object) = value.as_object() {
        let matches = matches_cwd(value, cwd)
            || ["env", "environment", "metadata", "context", "project"]
                .iter()
                .any(|key| {
                    object
                        .get(*key)
                        .is_some_and(|value| matches_cwd(value, cwd))
                });
        if matches {
            for key in [
                "session_id",
                "sessionId",
                "conversation_id",
                "conversationId",
                "threadId",
                "id",
            ] {
                if let Some(id) = object
                    .get(key)
                    .and_then(|v| v.as_str())
                    .filter(|id| valid_id(id))
                {
                    return Some(id.into());
                }
            }
            // Codex rollout envelopes carry metadata in payload.
            if let Some(payload) = object.get("payload") {
                return metadata_id(payload, cwd, depth + 1);
            }
        }
        for value in object.values() {
            if let Some(id) = metadata_id(value, cwd, depth + 1) {
                return Some(id);
            }
        }
    }
    None
}
fn matches_cwd(value: &serde_json::Value, cwd: &Path) -> bool {
    [
        "cwd",
        "workingDirectory",
        "working_directory",
        "directory",
        "projectPath",
    ]
    .iter()
    .any(|key| {
        value
            .get(*key)
            .and_then(|v| v.as_str())
            .is_some_and(|path| {
                path.replace('\\', "/")
                    .eq_ignore_ascii_case(&cwd.to_string_lossy().replace('\\', "/"))
            })
    })
}
fn sqlite_id(path: &Path, cwd: &Path) -> Option<String> {
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    db.busy_timeout(std::time::Duration::from_millis(100))
        .ok()?;
    for (table, column, time) in [
        ("session", "directory", "time_updated"),
        ("threads", "cwd", "updated_at"),
        ("sessions", "working_dir", "updated_at"),
    ] {
        // Fixed identifiers, never taken from session documents or user input.
        let sql =
            format!("SELECT id FROM {table} WHERE {column} = ?1 ORDER BY {time} DESC LIMIT 1");
        if let Ok(id) = db.query_row(&sql, [cwd.to_string_lossy().as_ref()], |row| {
            row.get::<_, String>(0)
        }) {
            if valid_id(&id) {
                return Some(id);
            }
        }
    }
    None
}
