//! Per-terminal lifecycle signals: CLIs that report their turns through hooks run
//! `basalt.exe --agent-hook <provider>`, which writes the terminal's state file.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Signal {
    pub provider: String,
    pub state: String,
    pub session_id: String,
    pub turn_id: String,
    pub event: String,
}
pub struct Bridge {
    pub root: PathBuf,
}
impl Bridge {
    pub fn new(root: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }
    pub fn path(&self, id: Uuid) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }
    pub fn clear(&self, id: Uuid) {
        let path = self.path(id);
        if self.root.is_absolute() && path.starts_with(&self.root) {
            let _ = fs::remove_file(path);
        }
    }
    pub fn cmd_init(&self, id: Uuid) -> Option<PathBuf> {
        let proxy = self.codex_proxy(id)?;
        let path = self.root.join(format!("{id}-init.cmd"));
        // Only this terminal's command alias: keep hooks in its own process
        // environment instead of an already-running shared daemon.
        fs::write(
            &path,
            format!("@doskey codex=node \"{}\" $*\r\n", proxy.display()),
        )
        .ok()?;
        Some(path)
    }
    /// This terminal's `codex` entry point: the installed Codex without its shared
    /// daemon, so lifecycle hooks report to this terminal.
    pub fn codex_proxy(&self, id: Uuid) -> Option<PathBuf> {
        let codex = PathBuf::from(std::env::var_os("APPDATA")?).join("npm/codex.cmd");
        if !codex.is_file() {
            return None;
        }
        let entry = codex
            .parent()?
            .join("node_modules/@openai/codex/bin/codex.js");
        if !entry.is_file() {
            return None;
        }
        let proxy = self.root.join(format!("{id}-codex.js"));
        let code = format!(
            r#"const cp=require('child_process');
const args=process.argv.slice(2);
const commands=['exec','review','login','logout','mcp','plugin','app-server','remote-control','app','completion','update','doctor','sandbox','debug','exec-server','features','help','agents'];
const command=args.find(a=>!a.startsWith('-'));
if(!commands.includes(command)&&!args.includes('--no-daemon'))args.unshift('--no-daemon');
const child=cp.spawn(process.execPath,[{},...args],{{stdio:'inherit'}});
for(const signal of ['SIGINT','SIGTERM'])process.on(signal,()=>child.kill(signal));
child.on('error',()=>process.exit(1));child.on('exit',code=>process.exit(code??1));
"#,
            serde_json::to_string(&entry.to_string_lossy()).ok()?
        );
        fs::write(&proxy, code).ok()?;
        Some(proxy)
    }
    pub fn read(&self, id: Uuid) -> Option<Signal> {
        let value: Signal = serde_json::from_slice(&fs::read(self.path(id)).ok()?).ok()?;
        if !matches!(value.provider.as_str(), "codex" | "claude" | "opencode")
            || !matches!(
                value.state.as_str(),
                "idle" | "working" | "closed" | "waiting" | "completed" | "error"
            )
        {
            return None;
        }
        Some(value)
    }
}
/// The hook command for `provider`: this program, asked to note one event.
pub fn hook_command(program: &Path, provider: &str) -> String {
    format!("\"{}\" --agent-hook {provider}", program.display())
}

/// Whether a hook command is one of ours, from this or an earlier version.
fn ours(command: &str) -> bool {
    command.contains(" --agent-hook ") || command.contains("agent-hook.ps1\"")
}

/// Run as a CLI's hook: read the event from stdin and note the state it means in the
/// terminal's state file. Never blocks the CLI or changes what it does: the answer is
/// always an empty result, whatever happens.
pub fn run_hook(provider: &str) -> i32 {
    use std::io::{Read, Write};
    let mut text = String::new();
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut text);
    if let (Some(path), Ok(event)) = (
        std::env::var_os("BASALT_AGENT_STATE_FILE").map(PathBuf::from),
        serde_json::from_str::<Value>(&text),
    ) {
        let previous = fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<Signal>(&bytes).ok());
        if let Some(signal) = signal_for(provider, &event, previous.as_ref()) {
            if let Ok(bytes) = serde_json::to_vec(&signal) {
                let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
                if fs::write(&temporary, bytes).is_ok() && fs::rename(&temporary, &path).is_err() {
                    let _ = fs::remove_file(&temporary);
                }
            }
        }
    }
    let mut out = std::io::stdout();
    let _ = out.write_all(b"{}
");
    let _ = out.flush();
    0
}

/// The state one hook event means, or None for an event that changes nothing: a
/// subagent's, a compaction restart, or the end of a turn that was already replaced.
pub fn signal_for(provider: &str, event: &Value, previous: Option<&Signal>) -> Option<Signal> {
    let text = |key: &str| match &event[key] {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    if !event["agent_id"].is_null() {
        return None;
    }
    let kind = text("hook_event_name");
    let state = match kind.as_str() {
        "SessionStart" if text("source") != "compact" => "idle",
        "UserPromptSubmit" | "PreToolUse" => "working",
        "PermissionRequest" => "waiting",
        "Stop" => "completed",
        "StopFailure" => "error",
        "Interrupt" => "idle",
        "SessionEnd" => "closed",
        "Notification" => match text("notification_type").as_str() {
            "permission_prompt" => "waiting",
            "idle_prompt" => "idle",
            _ => return None,
        },
        _ => return None,
    };
    let (session_id, turn_id) = (text("session_id"), text("turn_id"));
    let ended_turn = matches!(kind.as_str(), "Stop" | "StopFailure" | "Interrupt");
    if ended_turn
        && previous.is_some_and(|old| {
            old.session_id == session_id && !old.turn_id.is_empty() && !turn_id.is_empty() && old.turn_id != turn_id
        })
    {
        return None;
    }
    Some(Signal { provider: provider.into(), state: state.into(), session_id, turn_id, event: kind })
}

pub fn merge_hooks(mut original: Value, program: &Path, provider: &str) -> Result<Value, String> {
    let events = if provider == "codex" {
        vec![
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "Stop",
            "Interrupt",
            "SessionEnd",
        ]
    } else {
        vec![
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "Stop",
            "StopFailure",
            "SessionEnd",
            "Notification",
        ]
    };
    let command = hook_command(program, provider);
    let root = original
        .as_object_mut()
        .ok_or("Hook configuration must be an object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("Existing hooks must be an object")?;
    for event in events {
        let entries = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or("Existing hook event must be an array")?;
        // A hook of ours left from when the app's folder had another name or place.
        entries.retain(|entry| {
            !entry["hooks"].as_array().is_some_and(|handlers| {
                handlers.iter().any(|handler| {
                    handler["command"].as_str().is_some_and(|other| other != command && ours(other))
                })
            })
        });
        if entries.iter().any(|entry| {
            entry["hooks"].as_array().is_some_and(|handlers| {
                handlers.iter().any(|handler| handler["command"] == command)
            })
        }) {
            continue;
        }
        let mut entry = json!({"hooks":[{"type":"command","command":command,"timeout":10}]});
        if matches!(event, "PreToolUse" | "PermissionRequest") {
            entry["matcher"] = json!(".*");
        }
        if event == "Notification" {
            entry["matcher"] = json!("idle_prompt|permission_prompt");
        }
        entries.push(entry);
    }
    Ok(original)
}
