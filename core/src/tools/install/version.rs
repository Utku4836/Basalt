use crate::{
    Error, ErrorKind, Result,
    system::process::{self, CancellationToken, CommandRequest, OutputSink, Stream},
};
use parking_lot::Mutex;
use regex::Regex;
use std::sync::{Arc, OnceLock};

pub fn newer(latest: &str, installed: &str) -> bool {
    if latest.is_empty() || installed.is_empty() || latest == installed {
        return false;
    }
    let Some(left) = parts(latest) else {
        return latest != installed;
    };
    let Some(right) = parts(installed) else {
        return latest != installed;
    };
    for (a, b) in left.0.iter().zip(right.0.iter()) {
        if a != b {
            return a > b;
        }
    }
    if left.1.is_empty() != right.1.is_empty() {
        return left.1.is_empty();
    }
    if left.0[0] > 2000 {
        return left.1 != right.1;
    }
    let a: Vec<_> = left.1.trim_start_matches('-').split('.').collect();
    let b: Vec<_> = right.1.trim_start_matches('-').split('.').collect();
    for index in 0..a.len().max(b.len()) {
        let (Some(a), Some(b)) = (a.get(index), b.get(index)) else {
            return a.len() > b.len();
        };
        if a == b {
            continue;
        }
        let an = a.parse::<u64>();
        let bn = b.parse::<u64>();
        return match (an, bn) {
            (Ok(a), Ok(b)) => a > b,
            (Ok(_), Err(_)) => false,
            (Err(_), Ok(_)) => true,
            _ => a > b,
        };
    }
    false
}
fn parts(value: &str) -> Option<([u64; 3], String)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^(\d+)\.(\d+)\.(\d+)(.*)$").unwrap());
    let value = value.strip_prefix('v').unwrap_or(value).split('+').next()?;
    let captured = re.captures(value)?;
    Some((
        [
            captured[1].parse().ok()?,
            captured[2].parse().ok()?,
            captured[3].parse().ok()?,
        ],
        captured[4].to_owned(),
    ))
}

#[derive(Default)]
struct VersionOutput(Mutex<String>);
impl OutputSink for VersionOutput {
    fn write(&self, _: Stream, bytes: &[u8]) -> Result<()> {
        let mut value = self.0.lock();
        value.push_str(&String::from_utf8_lossy(bytes));
        if value.len() > 16384 {
            let mut remove = value.len() - 16384;
            while !value.is_char_boundary(remove) {
                remove += 1;
            }
            value.drain(..remove);
        }
        Ok(())
    }
}

pub fn read(request: CommandRequest, cancel: &CancellationToken) -> Result<String> {
    let sink = Arc::new(VersionOutput::default());
    let outcome = process::run(request, cancel, sink.clone())?;
    if outcome.exit_code != Some(0) {
        return Err(Error::new(
            ErrorKind::Process,
            format!("Version command exited with {:?}", outcome.exit_code),
        ));
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\b\d+\.\d+\.\d+(?:[-+][a-zA-Z0-9._-]+)?").unwrap());
    let separated = sink.0.lock().replace(['\r', '\n'], " ");
    let output = super::reporter::clean_line(&separated, 16384);
    re.find(&output)
        .map(|matched| matched.as_str().to_owned())
        .ok_or_else(|| {
            Error::new(
                ErrorKind::CorruptData,
                "CLI did not report an installed version",
            )
        })
}
