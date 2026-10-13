//! The plain terminal's shell: PowerShell with a one-character prompt that turns red
//! after a failed command. Nothing is drawn by cursor movement: text positioned for one
//! terminal width lands in the wrong place once the pane is resized.
use std::path::{Path, PathBuf};

/// PowerShell 7 when it is on PATH, otherwise the Windows PowerShell every system has.
pub fn powershell() -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|folder| folder.join("pwsh.exe"))
            .find(|candidate| candidate.is_file())
    });
    on_path.or_else(|| {
        let system = PathBuf::from(std::env::var_os("SystemRoot")?)
            .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        system.is_file().then_some(system)
    })
}

/// The ConPTY the terminals run on: Microsoft's, bundled in the program so a single
/// `basalt.exe` is complete. A `runtime` folder beside the program or above it (a build
/// inside the project) is used as it is; otherwise the bundled copy is written once to
/// the app's data folder, with its license, and used from there.
pub fn conpty_runtime() -> PathBuf {
    const RELATIVE: &str = "runtime/windows/conpty/x64/conpty.dll";
    const VERSION: &str = "1.24.260710001";
    const FILES: [(&str, &[u8]); 3] = [
        ("conpty.dll", include_bytes!("../../../runtime/windows/conpty/x64/conpty.dll")),
        ("OpenConsole.exe", include_bytes!("../../../runtime/windows/conpty/x64/OpenConsole.exe")),
        ("LICENSE-MIT", include_bytes!("../../../runtime/windows/conpty/LICENSE-MIT")),
    ];
    let beside = std::env::current_exe()
        .ok()
        .and_then(|program| program.ancestors().skip(1).map(|folder| folder.join(RELATIVE)).find(|dll| dll.is_file()));
    if let Some(dll) = beside {
        return dll;
    }
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let folder = local.join("Basalt").join("data").join("runtime").join(format!("conpty-{VERSION}"));
    for (name, bytes) in FILES {
        let path = folder.join(name);
        let current = std::fs::metadata(&path).is_ok_and(|meta| meta.len() == bytes.len() as u64);
        if !current {
            // Written beside and moved into place, so a terminal starting meanwhile never
            // loads half a file.
            let partial = folder.join(format!("{name}.{}.partial", std::process::id()));
            let _ = std::fs::create_dir_all(&folder)
                .and_then(|_| std::fs::write(&partial, bytes))
                .and_then(|_| std::fs::rename(&partial, &path));
            let _ = std::fs::remove_file(&partial);
        }
    }
    folder.join("conpty.dll")
}

/// Variables a Claude Code session gives the programs it runs, naming that session.
/// A terminal here is a session of its own: a CLI started in it must not take itself
/// for a child of whatever started the app.
const PARENT_SESSION: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_HOST_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SSE_PORT",
];

/// The app's environment for a terminal, without another session's markers.
pub fn host_environment() -> std::collections::BTreeMap<String, String> {
    std::env::vars()
        .filter(|(key, _)| !PARENT_SESSION.iter().any(|marker| key.eq_ignore_ascii_case(marker)))
        .collect()
}

/// Arguments that start PowerShell quietly and run `script` after the user's profile,
/// so the prompt below wins over a profile prompt. An encoded command is not subject
/// to the script execution policy.
pub fn arguments(script: &str) -> Vec<String> {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    vec![
        "-NoLogo".into(),
        "-NoExit".into(),
        "-EncodedCommand".into(),
        base64(&bytes),
    ]
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = chunk.iter().enumerate().fold(0u32, |value, (index, byte)| value | u32::from(*byte) << (16 - 8 * index));
        for index in 0..4 {
            text.push(if index <= chunk.len() { TABLE[(value >> (18 - 6 * index) & 63) as usize] as char } else { '=' });
        }
    }
    text
}

/// The session script: the prompt, and the per-terminal `codex` hook proxy if any.
pub fn init_script(codex_proxy: Option<&Path>) -> String {
    let mut script = String::from(PROMPT);
    if let Some(proxy) = codex_proxy {
        let path = proxy.display().to_string().replace('\'', "''");
        script.push_str(&format!("\nfunction global:codex {{ node '{path}' @args }}\n"));
    }
    script
}

// Windows PowerShell 5.1 compatible: no `e escapes or ternaries. Each prompt tells
// the app the folder and whether the last command succeeded, through the file named
// by BASALT_SHELL_STATE; the command line is colored in the theme's tones.
const PROMPT: &str = r#"
function global:prompt {
    $ok = $?
    $e = [char]27
    if ($env:BASALT_SHELL_STATE) {
        try { [IO.File]::WriteAllText($env:BASALT_SHELL_STATE, "$([int]$ok)`n$($PWD.ProviderPath)") } catch {}
    }
    $tint = '164;164;171'
    if (-not $ok) { $tint = '236;138;138' }
    # The Nerd Font angle, bold.
    "$e[1;38;2;${tint}m" + [char]0xF105 + "$e[0m "
}
# A command typed at the prompt that does not exist: one quiet line instead of the
# error. Nothing is offered; `install <name>` looks for it when asked.
$ExecutionContext.InvokeCommand.CommandNotFoundAction = {
    param($name, $eventArgs)
    if ($eventArgs.CommandOrigin -ne 'Runspace' -or $name -like 'get-*') { return }
    $eventArgs.CommandScriptBlock = { Write-Host "$([char]27)[38;2;116;116;124m'$name' was not found$([char]27)[0m" }.GetNewClosure()
    $eventArgs.StopSearch = $true
}
if (Get-Module PSReadLine) {
    $e = [char]27
    try {
        Set-PSReadLineOption -Colors @{
            Command = "$e[38;2;232;232;234m"; Parameter = "$e[38;2;127;209;193m"; String = "$e[38;2;158;206;106m"
            Variable = "$e[38;2;122;162;247m"; Number = "$e[38;2;224;175;104m"; Operator = "$e[38;2;164;164;171m"
            Comment = "$e[38;2;116;116;124m"; Keyword = "$e[38;2;187;154;247m"; Type = "$e[38;2;125;207;255m"
            Member = "$e[38;2;196;204;222m"; Default = "$e[38;2;196;204;222m"
        }
    } catch {}
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_command_is_utf16_base64_of_the_script() {
        let script = init_script(Some(Path::new(r"C:\data\it's\codex.js")));
        assert!(script.contains("function global:prompt"));
        assert!(!script.contains("[s") && !script.contains("G$e"), "no cursor-positioned text in the prompt");
        assert!(script.contains(r"node 'C:\data\it''s\codex.js' @args"));
        let args = arguments(&script);
        assert_eq!(&args[..3], ["-NoLogo", "-NoExit", "-EncodedCommand"]);
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        // UTF-16LE, as PowerShell expects: "ls" -> 6C 00 73 00.
        assert_eq!(arguments("ls")[3], "bABzAA==");
    }
}
