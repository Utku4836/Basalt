//! Commands run at the prompt, kept per profile: the source of inline suggestions and
//! of history search. Commands with secrets are never kept, failed ones are not
//! recorded, and risky ones are kept for search but never suggested inline.

use serde::{Deserialize, Serialize};
use std::path::Path;

const LIMIT: usize = 5_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub command: String,
    /// The folder it was last run in.
    pub folder: String,
    pub uses: u32,
    /// Unix seconds of the last run.
    pub last: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct History {
    entries: Vec<Entry>,
}

const RISKY: [&str; 16] = [
    "--dangerously",
    "--yolo",
    "--allow-all",
    "--yes-always",
    "--force",
    "push -f",
    "reset --hard",
    "clean -f",
    "rm -r",
    "rm -fr",
    "remove-item",
    "rd /s",
    "del /s",
    "format ",
    "shutdown",
    "drop table",
];

/// Commands that delete, force or skip safety checks: shown in search, colored amber
/// on the command line, never offered by a single keypress.
pub fn risky(command: &str) -> bool {
    !risky_spans(command).is_empty()
}

/// Where the risky parts of `command` are, as character ranges, each running to the
/// end of its word ("--dangerously-skip-permissions" as a whole).
pub fn risky_spans(command: &str) -> Vec<std::ops::Range<usize>> {
    let folded: Vec<char> = command.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect();
    let mut spans: Vec<std::ops::Range<usize>> = Vec::new();
    for part in RISKY {
        let part: Vec<char> = part.chars().collect();
        let mut at = 0;
        while at + part.len() <= folded.len() {
            if folded[at..at + part.len()] == part[..] {
                let mut end = at + part.len();
                while end < folded.len() && !folded[end].is_whitespace() {
                    end += 1;
                }
                spans.push(at..end);
                at = end;
            } else {
                at += 1;
            }
        }
    }
    spans.sort_by_key(|span| span.start);
    spans
}

/// Commands that look like they carry a password, token or key.
pub fn secret(command: &str) -> bool {
    let lower = command.to_lowercase();
    const WORDS: [&str; 9] = ["password", "passwd", "secret", "token", "api_key", "apikey", "api-key", "bearer ", "authorization:"];
    WORDS.iter().any(|word| lower.contains(word))
        || command.split(|c: char| c.is_whitespace() || c == '=' || c == '"' || c == '\'').any(|word| {
            word.starts_with("sk-") && word.len() > 20 || word.starts_with("ghp_") || word.starts_with("github_pat_")
        })
}

impl History {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        let partial = path.with_extension("partial");
        std::fs::write(&partial, bytes)?;
        std::fs::rename(partial, path)
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Keep a command that succeeded. False when it is not kept.
    pub fn record(&mut self, command: &str, folder: &str, now: u64) -> bool {
        let command = command.trim();
        if command.is_empty() || command.len() > 2_000 || secret(command) {
            return false;
        }
        match self.entries.iter_mut().find(|entry| entry.command == command) {
            Some(entry) => {
                entry.uses = entry.uses.saturating_add(1);
                entry.last = now;
                entry.folder = folder.into();
            }
            None => self.entries.push(Entry { command: command.into(), folder: folder.into(), uses: 1, last: now }),
        }
        if self.entries.len() > LIMIT {
            self.entries.sort_by_key(|entry| std::cmp::Reverse(entry.last));
            self.entries.truncate(LIMIT);
        }
        true
    }

    pub fn forget(&mut self, command: &str) {
        self.entries.retain(|entry| entry.command != command);
    }

    fn score(entry: &Entry, folder: &str, now: u64) -> f64 {
        let days = now.saturating_sub(entry.last) as f64 / 86_400.0;
        let here = if !folder.is_empty() && entry.folder.eq_ignore_ascii_case(folder) { 2.0 } else { 0.0 };
        f64::from(entry.uses).ln_1p() + 3.0 / (1.0 + days) + here
    }

    /// The best command that continues what is typed, for an inline suggestion.
    pub fn suggest(&self, typed: &str, folder: &str, now: u64) -> Option<&str> {
        if typed.trim().is_empty() {
            return None;
        }
        self.entries
            .iter()
            // A risky part is only completed once two of its characters are typed: "claude
            // --" finds "--dangerously-skip-permissions", "claude -" does not.
            .filter(|entry| {
                let typed_chars = typed.chars().count();
                entry.command.len() > typed.len()
                    && entry.command.starts_with(typed)
                    && risky_spans(&entry.command).iter().all(|span| typed_chars >= span.start + 2)
            })
            .max_by(|a, b| Self::score(a, folder, now).total_cmp(&Self::score(b, folder, now)))
            .map(|entry| entry.command.as_str())
    }

    /// Commands holding every typed character in order, best first: "clsk" finds
    /// "claude --dangerously-skip-permissions". `folder` keeps only that folder.
    pub fn search(&self, query: &str, folder: Option<&str>, now: u64) -> Vec<&Entry> {
        let query: Vec<char> = query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
        let mut found: Vec<(f64, &Entry)> = self
            .entries
            .iter()
            .filter(|entry| folder.is_none_or(|folder| entry.folder.eq_ignore_ascii_case(folder)))
            .filter_map(|entry| {
                let text = entry.command.to_lowercase();
                let mut letters = query.iter().peekable();
                let mut gaps = 0usize;
                let mut started = false;
                for c in text.chars() {
                    match letters.peek() {
                        Some(next) if **next == c => {
                            letters.next();
                            started = true;
                        }
                        Some(_) if started => gaps += 1,
                        _ => {}
                    }
                }
                letters.peek().is_none().then(|| {
                    let contains = query.is_empty() || text.contains(&query.iter().collect::<String>());
                    let bonus = if contains { 5.0 } else { 0.0 };
                    (Self::score(entry, folder.unwrap_or(""), now) + bonus - gaps as f64 * 0.05, entry)
                })
            })
            .collect();
        found.sort_by(|a, b| b.0.total_cmp(&a.0));
        found.into_iter().map(|(_, entry)| entry).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;

    #[test]
    fn suggestions_continue_the_typed_text_and_prefer_this_folder() {
        let mut history = History::default();
        history.record("cargo test --release", r"C:\a", 10 * DAY);
        history.record("cargo build", r"C:\b", 10 * DAY);
        history.record("cargo build", r"C:\b", 10 * DAY);
        assert_eq!(history.suggest("cargo ", r"C:\a", 10 * DAY), Some("cargo test --release"));
        assert_eq!(history.suggest("cargo ", r"C:\b", 10 * DAY), Some("cargo build"));
        assert_eq!(history.suggest("cargo build", r"C:\b", 10 * DAY), None, "nothing longer to add");
        assert_eq!(history.suggest("", r"C:\a", 10 * DAY), None);
    }

    #[test]
    fn risky_parts_are_completed_only_once_typed_and_secrets_are_not_kept() {
        let mut history = History::default();
        assert!(history.record("claude --dangerously-skip-permissions", "", 0));
        assert_eq!(history.suggest("claude", "", 0), None);
        assert_eq!(history.suggest("claude -", "", 0), None);
        assert_eq!(history.suggest("claude --", "", 0), Some("claude --dangerously-skip-permissions"));
        assert_eq!(history.search("clsk", None, 0)[0].command, "claude --dangerously-skip-permissions");
        assert!(!history.record("curl -H \"Authorization: Bearer abc\" x", "", 0));
        assert!(!history.record("set OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz", "", 0));
        assert!(!history.record("gh auth login --with-token", "", 0));
        assert_eq!(history.entries().len(), 1);
        history.forget("claude --dangerously-skip-permissions");
        assert!(history.entries().is_empty());
    }

    #[test]
    fn risky_parts_cover_their_whole_word() {
        assert_eq!(risky_spans("claude --dangerously-skip-permissions -p hi"), vec![7..37]);
        assert_eq!(risky_spans("git reset --hard HEAD"), vec![4..16]);
        assert!(risky_spans("git status").is_empty());
    }

    #[test]
    fn search_finds_letters_in_order_and_filters_by_folder() {
        let mut history = History::default();
        history.record("docker compose up -d", r"C:\web", 0);
        history.record("git status", r"C:\api", 0);
        let commands = |query: &str, folder: Option<&str>| {
            history.search(query, folder, 0).into_iter().map(|entry| entry.command.as_str()).collect::<Vec<_>>()
        };
        assert_eq!(commands("dcup", None), ["docker compose up -d"]);
        assert_eq!(commands("st", Some(r"C:\api")), ["git status"]);
        assert_eq!(commands("", None).len(), 2);
        assert!(commands("zzz", None).is_empty());
    }

    #[test]
    fn history_survives_a_save_and_load() {
        let path = std::env::temp_dir().join(format!("basalt-history-{}.json", uuid::Uuid::new_v4()));
        let mut history = History::default();
        history.record("ls", "", 1);
        history.save(&path).unwrap();
        assert_eq!(History::load(&path).entries(), history.entries());
        std::fs::remove_file(path).unwrap();
    }
}
