//! Reinstalling a CLI whose program has gone, asked for by its shim. The shim runs
//! `basalt.exe --repair <id>` in the terminal; that process leaves a request in
//! `_frontend/repair` and waits, and the running app, which owns the data folder,
//! reinstalls the CLI and leaves the outcome beside it.
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Where requests and outcomes are kept, under the data folder.
pub fn folder(data: &Path) -> PathBuf {
    data.join("_frontend").join("repair")
}

/// The terminal's side: ask for `tool` to be reinstalled and wait for the outcome,
/// reporting on the terminal. Returns the process exit code.
pub fn request(tool: &str) -> i32 {
    let mut out = console();
    let id_ok = !tool.is_empty() && tool.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-');
    let data = std::env::var_os("LOCALAPPDATA")
        .map(|local| crate::folders::Folders::under(Path::new(&local), "Basalt").data);
    let (true, Some(data)) = (id_ok, data) else {
        let _ = writeln!(out, "basalt: nothing to repair");
        return 1;
    };
    let folder = folder(&data);
    let outcome = folder.join(format!("{tool}.done"));
    let _ = fs::remove_file(&outcome);
    if fs::create_dir_all(&folder).and_then(|_| fs::write(folder.join(format!("{tool}.request")), b"")).is_err() {
        let _ = writeln!(out, "basalt: could not ask for {tool} to be reinstalled");
        return 1;
    }
    let _ = writeln!(out, "{tool} is missing its program; Basalt is reinstalling it...");
    let started = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(400));
        if let Ok(text) = fs::read_to_string(&outcome) {
            let _ = fs::remove_file(&outcome);
            return match text.strip_prefix("ok") {
                Some(_) => {
                    let _ = writeln!(out, "{tool} is reinstalled.\n");
                    0
                }
                None => {
                    let _ = writeln!(out, "Reinstalling {tool} failed: {}", text.trim());
                    1
                }
            };
        }
        // An app that is running takes the request within a second.
        let waiting = folder.join(format!("{tool}.request")).exists();
        if waiting && started.elapsed() > Duration::from_secs(15) {
            let _ = fs::remove_file(folder.join(format!("{tool}.request")));
            let _ = writeln!(out, "Basalt is not running; open it to reinstall {tool}.");
            return 1;
        }
        if started.elapsed() > Duration::from_secs(30 * 60) {
            let _ = writeln!(out, "Reinstalling {tool} is taking too long; try again later.");
            return 1;
        }
    }
}

/// The app's side: the CLIs asked for since the last call, each taken so it is
/// reinstalled once.
pub fn take(folder: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let tool = name.strip_suffix(".request")?.to_owned();
            fs::rename(entry.path(), folder.join(format!("{tool}.working"))).ok()?;
            Some(tool)
        })
        .collect()
}

/// Leave the outcome of a repair for the terminal waiting on it.
pub fn finish(folder: &Path, tool: &str, result: &Result<String, String>) {
    let text = match result {
        Ok(version) => format!("ok {version}"),
        Err(error) => error.clone(),
    };
    let _ = fs::write(folder.join(format!("{tool}.done")), text);
    let _ = fs::remove_file(folder.join(format!("{tool}.working")));
}

/// The terminal this process was started from. The program is a window program, so it
/// has no console of its own; it borrows its parent's.
fn console() -> Box<dyn Write> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0 {
            if let Ok(file) = fs::OpenOptions::new().write(true).open("CONOUT$") {
                return Box::new(file);
            }
        }
    }
    Box::new(std::io::stdout())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_taken_once_and_its_outcome_left_behind() {
        let folder = std::env::temp_dir().join(format!("basalt-repair-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("claude.request"), b"").unwrap();
        assert_eq!(take(&folder), vec!["claude".to_owned()]);
        assert!(take(&folder).is_empty());
        finish(&folder, "claude", &Ok("2.1.300".into()));
        assert_eq!(fs::read_to_string(folder.join("claude.done")).unwrap(), "ok 2.1.300");
        assert!(!folder.join("claude.working").exists());
        fs::remove_dir_all(folder).unwrap();
    }
}
