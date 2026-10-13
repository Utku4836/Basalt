//! What a command accepts, learned from its own `--help`: flags with their
//! descriptions, and subcommands listed under a "Commands:" heading. Help is asked only
//! when the user presses Tab after a flag prefix, never from programs that change the
//! system when started.

use std::{path::Path, process::Stdio, time::Duration};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Help {
    pub flags: Vec<Flag>,
    pub commands: Vec<Flag>,
}

/// Programs whose help is never asked: starting them can change the system.
const NEVER_ASK: [&str; 22] = [
    "shutdown", "format", "del", "erase", "rd", "rmdir", "rm", "diskpart", "reg", "sc", "bcdedit", "cipher",
    "takeown", "icacls", "net", "taskkill", "logoff", "restart-computer", "stop-computer", "remove-item", "mkfs",
    "dd",
];

/// The program a command line starts, if its help may be asked.
pub fn program(line: &str) -> Option<&str> {
    let first = line.split_whitespace().next()?;
    let name = Path::new(first).file_stem()?.to_str()?;
    let plain = first.chars().all(|c| c.is_alphanumeric() || "._-\\/:".contains(c));
    (plain && !NEVER_ASK.contains(&name.to_lowercase().as_str())).then_some(first)
}

/// Whether `program` is a file on disk or on PATH (with any PATHEXT extension), so
/// shell built-ins and PowerShell commands keep their own completion.
pub fn found(program: &str) -> bool {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return path.is_file();
    }
    let extensions: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(str::to_owned)
        .collect();
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|folder| {
            folder.join(program).is_file() && path.extension().is_some()
                || extensions.iter().any(|extension| folder.join(format!("{program}{extension}")).is_file())
        })
    })
}

/// The word being typed at the end of the line.
pub fn current_word(line: &str) -> &str {
    if line.ends_with(char::is_whitespace) {
        return "";
    }
    line.rsplit(char::is_whitespace).next().unwrap_or("")
}

pub fn parse(help: &str) -> Help {
    let mut parsed = Help::default();
    let mut in_commands = false;
    for line in help.lines() {
        let trimmed = line.trim_start();
        let lower = trimmed.to_lowercase();
        if trimmed.is_empty() {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            in_commands = lower.trim_end_matches(':').ends_with("commands") || lower.starts_with("commands:") || lower.starts_with("subcommands");
            continue;
        }
        // The description starts after a run of two or more spaces (or a tab).
        let split = trimmed.find("  ").or_else(|| trimmed.find('\t'));
        let (head, description) = match split {
            Some(at) => (&trimmed[..at], trimmed[at..].trim()),
            None => (trimmed, ""),
        };
        if head.starts_with('-') {
            let names: Vec<&str> = head
                .split([',', ' ', '|'])
                .map(|token| token.split(['=', '[', '<']).next().unwrap_or(""))
                .filter(|token| token.starts_with('-') && token.len() > 1 && !token.contains(char::is_whitespace))
                .collect();
            if let Some(name) = names.iter().copied().find(|name| name.starts_with("--")).or(names.first().copied()) {
                if !parsed.flags.iter().any(|flag| flag.name == name) {
                    parsed.flags.push(Flag { name: name.into(), description: description.into() });
                }
            }
        } else if in_commands && split.is_some() {
            let name = head.split(['|', ',', ' ', '[', '<']).next().unwrap_or("");
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
                if !parsed.commands.iter().any(|command| command.name == name) {
                    parsed.commands.push(Flag { name: name.into(), description: description.into() });
                }
            }
        }
    }
    parsed
}

/// Entries that start with `prefix`, in their help order.
pub fn matching<'a>(entries: &'a [Flag], prefix: &str) -> Vec<&'a Flag> {
    entries.iter().filter(|entry| entry.name.starts_with(prefix)).collect()
}

/// Run `program --help` in `folder` and return what it printed, giving up after a few
/// seconds. The program is found the way the command prompt finds it.
pub fn ask(program: &str, folder: &Path) -> Option<String> {
    let mut command = std::process::Command::new(std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()));
    command.args(["/D", "/C", program, "--help"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if folder.is_dir() {
        command.current_dir(folder);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().ok()?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < Duration::from_secs(5) => std::thread::sleep(Duration::from_millis(30)),
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text.truncate(256 * 1024);
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELP: &str = "Usage: claude [options] [command] [prompt]

Options:
  -d, --debug [filter]              Enable debug mode
  --verbose                         Override verbose mode setting
  -p, --print                       Print response and exit
  --dangerously-skip-permissions    Bypass all permission checks
  --model <model>                   Model for the current session
  -h, --help                        Display help for command

Commands:
  config                            Manage configuration
  mcp                               Configure and manage MCP servers
  update|upgrade                    Check for updates and install if available
";

    #[test]
    fn reads_flags_with_descriptions_and_subcommands() {
        let help = parse(HELP);
        let names: Vec<_> = help.flags.iter().map(|flag| flag.name.as_str()).collect();
        assert_eq!(names, ["--debug", "--verbose", "--print", "--dangerously-skip-permissions", "--model", "--help"]);
        assert_eq!(help.flags[3].description, "Bypass all permission checks");
        let commands: Vec<_> = help.commands.iter().map(|command| command.name.as_str()).collect();
        assert_eq!(commands, ["config", "mcp", "update"]);
        assert_eq!(matching(&help.flags, "--d").len(), 2);
    }

    #[test]
    fn finds_the_program_and_the_word_being_typed() {
        assert_eq!(program("claude --d"), Some("claude"));
        assert_eq!(program(r"C:\tools\gh.exe pr"), Some(r"C:\tools\gh.exe"));
        assert_eq!(program("shutdown -"), None, "never asked");
        assert_eq!(program("rm -"), None);
        assert_eq!(current_word("claude --da"), "--da");
        assert_eq!(current_word("claude "), "");
        assert!(found("cmd"), "found on PATH through PATHEXT");
        assert!(!found("Get-ChildItem"), "PowerShell commands keep their own completion");
    }
}
