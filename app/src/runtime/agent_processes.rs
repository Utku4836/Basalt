//! Which coding agent runs in a terminal, from its processes: the program's name, or
//! for a runtime (node, bun, python, a shell) the script or command it was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    /// The agent's id (`claude`, `copilot`, `pi`, ...), as in [`AGENTS`].
    pub id: String,
    pub name: String,
    /// The process found to be the agent; another pid means the agent restarted.
    pub pid: u32,
}

pub struct Agent {
    pub id: &'static str,
    pub name: &'static str,
    /// Program names it runs under, without `.exe`.
    pub programs: &'static [&'static str],
    /// Package folders its scripts live in, as they appear in a path.
    pub packages: &'static [&'static str],
}

/// Coding agents known by name. Names shared with desktop apps (`cursor`, `kiro`,
/// `qoder`) are left out: those are the editors, not the agents.
pub const AGENTS: &[Agent] = &[
    Agent { id: "claude", name: "Claude", programs: &["claude", "claude-code"], packages: &["@anthropic-ai/claude-code/"] },
    Agent { id: "codex", name: "Codex", programs: &["codex"], packages: &["@openai/codex/"] },
    Agent { id: "copilot", name: "Copilot", programs: &["copilot", "github-copilot", "ghcs"], packages: &["@github/copilot/", "@github/copilot-"] },
    Agent { id: "gemini", name: "Gemini", programs: &["gemini"], packages: &["@google/gemini-cli/"] },
    Agent { id: "opencode", name: "OpenCode", programs: &["opencode", "opencode2", "open-code"], packages: &["/opencode-ai/"] },
    Agent { id: "pi", name: "Pi", programs: &["pi"], packages: &["@earendil-works/pi-coding-agent/", "@mariozechner/pi-coding-agent/", "@oh-my-pi/pi-coding-agent/"] },
    Agent { id: "agy", name: "Antigravity", programs: &["agy", "antigravity-cli"], packages: &[] },
    Agent { id: "cursor-agent", name: "Cursor", programs: &["cursor-agent"], packages: &["/cursor-agent/"] },
    Agent { id: "amp", name: "Amp", programs: &["amp", "amp-local"], packages: &["@ampcode/cli/", "@sourcegraph/amp/"] },
    Agent { id: "goose", name: "Goose", programs: &["goose"], packages: &[] },
    Agent { id: "crush", name: "Crush", programs: &["crush"], packages: &["@charmland/crush/"] },
    Agent { id: "qwen", name: "Qwen", programs: &["qwen", "qwen-code"], packages: &["@qwen-code/qwen-code/"] },
    Agent { id: "kimi", name: "Kimi", programs: &["kimi", "kimi-code"], packages: &["@moonshot-ai/kimi-code/"] },
    Agent { id: "aider", name: "Aider", programs: &["aider"], packages: &["/aider/"] },
    Agent { id: "droid", name: "Droid", programs: &["droid"], packages: &["@factory/cli/"] },
    Agent { id: "devin", name: "Devin", programs: &["devin", "devin-cli"], packages: &[] },
    Agent { id: "cline", name: "Cline", programs: &["cline"], packages: &["/node_modules/cline/"] },
    Agent { id: "kilo", name: "Kilo", programs: &["kilo", "kilo-code", "kilocode"], packages: &["@kilocode/cli/"] },
    Agent { id: "grok", name: "Grok", programs: &["grok", "grok-build"], packages: &["@vibe-kit/grok-cli/"] },
    Agent { id: "hermes", name: "Hermes", programs: &["hermes", "hermes-agent"], packages: &["/hermes-agent/"] },
    Agent { id: "kiro", name: "Kiro", programs: &["kiro-cli"], packages: &[] },
    Agent { id: "letta", name: "Letta", programs: &["letta", "letta-code"], packages: &["@letta-ai/letta-code/"] },
    Agent { id: "maki", name: "Maki", programs: &["maki"], packages: &[] },
    Agent { id: "muse", name: "Muse", programs: &["muse", "muse-code", "muse-cli"], packages: &[] },
    Agent { id: "qodercli", name: "Qoder", programs: &["qodercli", "qoderclicn", "qodercn"], packages: &[] },
    Agent { id: "mastracode", name: "Mastra", programs: &["mastracode", "mastra-code"], packages: &["/mastracode/"] },
];

pub fn agent(id: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|agent| agent.id.eq_ignore_ascii_case(id))
}

/// The agent a program name stands for: `copilot.exe`, `codex.cmd`, `claude`.
pub fn by_program(name: &str) -> Option<&'static Agent> {
    let stem = stem(name);
    AGENTS.iter().find(|agent| agent.programs.contains(&stem.as_str()))
}

/// The agent a terminal runs: the agent process nearest its shell, so an agent's own
/// tools and helpers (or a second agent it starts) do not take its place.
/// `command_line` gives a process's command line, asked only for runtimes and shells.
pub fn detect_agent(
    root: u32,
    processes: &[Process],
    mut command_line: impl FnMut(u32) -> Option<String>,
) -> Option<Activity> {
    let mut level = vec![root];
    let mut seen = vec![root];
    while !level.is_empty() {
        for pid in &level {
            let Some(process) = processes.iter().find(|process| process.pid == *pid) else {
                continue;
            };
            if let Some(agent) = identify(&process.name, || command_line(process.pid)) {
                return Some(Activity { id: agent.id.into(), name: agent.name.into(), pid: process.pid });
            }
        }
        let next: Vec<u32> = processes
            .iter()
            .filter(|process| level.contains(&process.parent) && !seen.contains(&process.pid))
            .map(|process| process.pid)
            .collect();
        seen.extend(&next);
        level = next;
    }
    None
}

/// The agent one process is: by its own name, or for a runtime or shell by what it
/// was asked to run.
pub fn identify(name: &str, command_line: impl FnOnce() -> Option<String>) -> Option<&'static Agent> {
    if let Some(agent) = by_program(name) {
        return Some(agent);
    }
    let runtime = stem(name);
    let kind = if matches!(runtime.as_str(), "node" | "bun" | "deno") {
        Runtime::Script
    } else if is_python(&runtime) {
        Runtime::Python
    } else if runtime == "cmd" {
        Runtime::Cmd
    } else if matches!(runtime.as_str(), "powershell" | "pwsh") {
        Runtime::PowerShell
    } else {
        return None;
    };
    let line = command_line()?;
    let words = split(&line);
    let target = match kind {
        Runtime::Script => words.iter().skip(1).find(|word| !word.starts_with('-') && *word != "run"),
        Runtime::Python => {
            if let Some(at) = words.iter().position(|word| word == "-m") {
                let module = words.get(at + 1)?;
                return by_program(module.split('.').next().unwrap_or(module));
            }
            words.iter().skip(1).find(|word| !word.starts_with('-'))
        },
        Runtime::Cmd => after(&words, &["/c", "/k"]),
        Runtime::PowerShell => after(&words, &["-c", "-command", "-file", "-f"]),
    }?;
    by_path(target)
}

enum Runtime {
    Script,
    Python,
    Cmd,
    PowerShell,
}

/// The agent a script or program path belongs to.
fn by_path(path: &str) -> Option<&'static Agent> {
    let normal = path.to_ascii_lowercase().replace('\\', "/");
    AGENTS
        .iter()
        .find(|agent| agent.packages.iter().any(|package| normal.contains(package)))
        .or_else(|| by_program(normal.rsplit('/').next().unwrap_or(&normal)))
}

/// The word after any of `flags`, compared without case.
fn after<'a>(words: &'a [String], flags: &[&str]) -> Option<&'a String> {
    let at = words.iter().position(|word| flags.iter().any(|flag| word.eq_ignore_ascii_case(flag)))?;
    words.get(at + 1)
}

/// A program or script name without its folder and extension, in lower case.
fn stem(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim_matches(['"', '\'']).to_ascii_lowercase();
    for extension in [".exe", ".cmd", ".bat", ".ps1", ".js", ".mjs", ".cjs", ".py"] {
        if let Some(stem) = base.strip_suffix(extension) {
            return stem.to_owned();
        }
    }
    base
}

fn is_python(name: &str) -> bool {
    let rest = name.strip_prefix("pythonw").or_else(|| name.strip_prefix("python"));
    rest.is_some_and(|version| version.chars().all(|ch| ch.is_ascii_digit() || ch == '.'))
}

/// A Windows command line split into words, with quotes removed.
fn split(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut any = false;
    for ch in line.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                any = true;
            },
            ch if ch.is_whitespace() && !quoted => {
                if any {
                    words.push(std::mem::take(&mut word));
                    any = false;
                }
            },
            ch => {
                word.push(ch);
                any = true;
            },
        }
    }
    if any {
        words.push(word);
    }
    words
}

#[cfg(windows)]
pub fn snapshot() -> Vec<Process> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::*,
    };
    unsafe {
        let handle = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if handle == INVALID_HANDLE_VALUE {
            return vec![];
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut processes = vec![];
        let mut ok = Process32FirstW(handle, &mut entry);
        while ok != 0 {
            let end = entry
                .szExeFile
                .iter()
                .position(|v| *v == 0)
                .unwrap_or(entry.szExeFile.len());
            processes.push(Process {
                pid: entry.th32ProcessID,
                parent: entry.th32ParentProcessID,
                name: String::from_utf16_lossy(&entry.szExeFile[..end]),
            });
            ok = Process32NextW(handle, &mut entry);
        }
        CloseHandle(handle);
        processes
    }
}
#[cfg(not(windows))]
pub fn snapshot() -> Vec<Process> {
    vec![]
}

/// A process's command line, or None when it cannot be read (gone, or not ours).
#[cfg(windows)]
pub fn command_line(pid: u32) -> Option<String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQueryInformationProcess(process: HANDLE, class: u32, info: *mut core::ffi::c_void, length: u32, returned: *mut u32) -> i32;
    }
    const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;
    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum: u16,
        buffer: *const u16,
    }
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut needed = 0u32;
        NtQueryInformationProcess(process, PROCESS_COMMAND_LINE_INFORMATION, std::ptr::null_mut(), 0, &mut needed);
        let mut text = None;
        if needed as usize >= std::mem::size_of::<UnicodeString>() && needed < 1 << 20 {
            // u64 words keep the structure at the start aligned.
            let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
            if NtQueryInformationProcess(process, PROCESS_COMMAND_LINE_INFORMATION, buffer.as_mut_ptr().cast(), needed, &mut needed) >= 0 {
                let header = &*(buffer.as_ptr() as *const UnicodeString);
                if !header.buffer.is_null() {
                    let units = std::slice::from_raw_parts(header.buffer, header.length as usize / 2);
                    text = Some(String::from_utf16_lossy(units));
                }
            }
        }
        CloseHandle(process);
        text
    }
}
#[cfg(not(windows))]
pub fn command_line(_: u32) -> Option<String> {
    None
}
