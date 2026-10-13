//! The other places `install <name>` looks besides the catalog and npm: PyPI (tools
//! installed each in its own environment) and winget.

use crate::packages::{Offer, Source};
use serde::Deserialize;
use basalt_core::{system::process::CancellationToken, tools::install::download::Http};

#[derive(Deserialize)]
struct PypiProject {
    info: PypiInfo,
}
#[derive(Deserialize)]
struct PypiInfo {
    name: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    maintainer: Option<String>,
    #[serde(default)]
    author_email: Option<String>,
}
#[derive(Deserialize)]
struct PypiStats {
    data: PypiRecent,
}
#[derive(Deserialize)]
struct PypiRecent {
    last_week: u64,
}

/// PyPI has no search to ask; the project named like the command, and its `-cli`
/// sibling, are looked up directly.
pub async fn pypi(http: &Http, command: &str, cancel: &CancellationToken) -> Vec<Offer> {
    let lookup = |candidate: String| async move {
        let project = http.json::<PypiProject>(&format!("https://pypi.org/pypi/{candidate}/json"), cancel).await.ok()?;
        let info = project.info;
        let name = info.name;
        let by = [info.author, info.maintainer, info.author_email]
            .into_iter()
            .flatten()
            .map(|text| crate::packages::person(&text))
            .find(|text| !text.is_empty())
            .unwrap_or_default();
        let stats = format!("https://pypistats.org/api/packages/{}/recent", name.to_ascii_lowercase());
        let weekly = http.json::<PypiStats>(&stats, cancel).await.ok().map(|stats| stats.data.last_week);
        Some(Offer {
            title: name.clone(),
            package: format!("pypi:{name}"),
            source: Source::Pypi(name.clone()),
            weekly,
            by,
            commands: vec![name.to_ascii_lowercase()],
        })
    };
    let (exact, cli) = tokio::join!(lookup(command.to_owned()), lookup(format!("{command}-cli")));
    exact.into_iter().chain(cli).collect()
}

/// Rows of a `winget search` table: name, id and version. The header's words mark
/// where each column starts, whatever language they are in.
pub fn winget_rows(text: &str) -> Vec<(String, String, String)> {
    // Progress spinners are overwritten with carriage returns; the last write counts.
    let lines: Vec<&str> = text.lines().map(|line| line.rsplit('\r').next().unwrap_or(line)).collect();
    let Some(rule) = lines.iter().position(|line| line.len() >= 10 && line.chars().all(|c| c == '-')) else {
        return Vec::new();
    };
    let Some(header) = rule.checked_sub(1).map(|at| lines[at].chars().collect::<Vec<_>>()) else {
        return Vec::new();
    };
    let starts: Vec<usize> =
        (0..header.len()).filter(|&at| header[at] != ' ' && (at == 0 || header[at - 1] == ' ')).collect();
    if starts.len() < 3 {
        return Vec::new();
    }
    lines[rule + 1..]
        .iter()
        .take_while(|line| !line.trim().is_empty() && !line.starts_with('<'))
        .filter_map(|line| {
            let row: Vec<char> = line.chars().collect();
            let cell = |column: usize| {
                let from = starts[column].min(row.len());
                let to = starts.get(column + 1).copied().unwrap_or(row.len()).min(row.len());
                row[from..to].iter().collect::<String>().trim().to_owned()
            };
            let (name, id, version) = (cell(0), cell(1), cell(2));
            (!id.is_empty() && !id.contains(' ')).then_some((name, id, version))
        })
        .collect()
}

/// winget's own packages for the command. Blocks; gives up after a few seconds.
pub fn winget(command: &str) -> Vec<Offer> {
    use std::io::Read;
    let mut process = std::process::Command::new("winget");
    process
        .args(["search", "--query", command, "--source", "winget", "--count", "8"])
        .args(["--disable-interactivity", "--accept-source-agreements"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        process.creation_flags(0x0800_0000);
    }
    let Ok(mut child) = process.spawn() else {
        return Vec::new();
    };
    let Some(mut stdout) = child.stdout.take() else {
        return Vec::new();
    };
    let reading = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let started = std::time::Instant::now();
    while child.try_wait().ok().flatten().is_none() {
        if started.elapsed() > std::time::Duration::from_secs(12) {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let text = String::from_utf8_lossy(&reading.join().unwrap_or_default()).into_owned();
    winget_rows(&text)
        .into_iter()
        .map(|(name, id, version)| Offer {
            title: name,
            // winget ids start with their publisher: "Anthropic.ClaudeCode".
            by: id.split('.').next().unwrap_or("").to_owned(),
            package: format!("winget:{id}"),
            source: Source::Winget { id, version },
            weekly: None,
            commands: Vec::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winget_tables_are_read_by_their_header() {
        let text = "\r   - \r   \\ \rName             Id                      Version Match\n\
                    -------------------------------------------------------------\n\
                    RipGrep MSVC     BurntSushi.ripgrep.MSVC 15.2.0  Tag: ripgrep\n\
                    ugrep            Genivia.ugrep           7.8.5   Tag: ripgrep\n\
                    <additional entries truncated due to result limit>\n";
        assert_eq!(
            winget_rows(text),
            [
                ("RipGrep MSVC".into(), "BurntSushi.ripgrep.MSVC".into(), "15.2.0".into()),
                ("ugrep".into(), "Genivia.ugrep".into(), "7.8.5".into()),
            ]
        );
        let turkish = "Ad      Kimlik         Sürüm\n------------------------------\nGit     Git.Git        2.51.0\n";
        assert_eq!(winget_rows(turkish), [("Git".into(), "Git.Git".into(), "2.51.0".into())]);
        assert!(winget_rows("No package found matching input criteria.").is_empty());
    }
}
