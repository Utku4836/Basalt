use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant},
};

use chrono::Utc;
use parking_lot::Mutex;
use regex::Regex;
use serde::Serialize;
use uuid::Uuid;

use crate::{
    Error, ErrorKind, Result,
    domain::{ApplicationIdentity, Profile},
    storage::DataRoot,
    system::process::{OutputSink, Stream},
    tools::catalog::Tool,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Preparing,
    Resolving,
    Downloading,
    Extracting,
    Running,
    Verifying,
    Activating,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub stage: Stage,
    pub percent: u8,
    pub message: String,
    pub log_path: PathBuf,
    pub stderr_tail: VecDeque<String>,
}

pub struct Reporter {
    state: Mutex<State>,
}

struct State {
    progress: Progress,
    log: BufWriter<File>,
    decoders: [Utf8; 2],
    lines: [String; 2],
    last_flush: Instant,
    finished: bool,
}

impl Reporter {
    pub fn new(
        root: &DataRoot,
        tool: &Tool,
        profile: &Profile,
        identity: &ApplicationIdentity,
        job: Uuid,
    ) -> Result<Self> {
        identity.validate()?;
        let directory = tool.profile_relative(profile).join("logs");
        root.create_dir(&directory)?;
        let timestamp = Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ");
        let path = root.resolve(directory.join(format!("install-{timestamp}-{job}.log")))?;
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|error| Error::io("Create installation log", &path, error))?;
        let mut log = BufWriter::with_capacity(64 * 1024, file);
        writeln!(
            log,
            "{} installer log\nTool: {} ({})\nProfile: {} ({})\nStarted: {}\n",
            identity.display_name,
            tool.name,
            tool.id,
            profile.name,
            profile.id.as_str(),
            Utc::now().to_rfc3339()
        )
        .map_err(|error| Error::io("Write installation log header", &path, error))?;
        log.flush()
            .map_err(|error| Error::io("Flush installation log header", &path, error))?;
        Ok(Self {
            state: Mutex::new(State {
                progress: Progress {
                    stage: Stage::Preparing,
                    percent: 0,
                    message: String::new(),
                    log_path: path,
                    stderr_tail: VecDeque::new(),
                },
                log,
                decoders: Default::default(),
                lines: Default::default(),
                last_flush: Instant::now(),
                finished: false,
            }),
        })
    }

    pub fn snapshot(&self) -> Progress {
        self.state.lock().progress.clone()
    }

    pub fn advance(&self, stage: Stage, percent: u8, message: &str) -> Result<()> {
        if percent > 100 || matches!(stage, Stage::Complete | Stage::Failed | Stage::Cancelled) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Use finalization for a terminal installation stage",
            ));
        }
        let mut state = self.state.lock();
        if state.finished {
            return Err(Error::new(
                ErrorKind::Conflict,
                "Installation reporter is finalized",
            ));
        }
        state.progress.stage = stage;
        state.progress.percent = state.progress.percent.max(percent);
        state.progress.message = clean_line(message, 500);
        Ok(())
    }

    pub fn failure(&self, fallback: &str) -> String {
        let state = self.state.lock();
        let meaningful = state.progress.stderr_tail.iter().rev().find(|line| {
            !line.starts_with('+') && !line.starts_with("At ") && !line.starts_with("char:")
        });
        let message = meaningful
            .or_else(|| state.progress.stderr_tail.back())
            .map(String::as_str)
            .unwrap_or_else(|| {
                if fallback.is_empty() {
                    &state.progress.message
                } else {
                    fallback
                }
            });
        clean_line(message, 300)
    }

    pub fn finish(&self, stage: Stage, message: &str) -> Result<()> {
        if !matches!(stage, Stage::Complete | Stage::Failed | Stage::Cancelled) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Finalization needs a terminal installation stage",
            ));
        }
        let mut state = self.state.lock();
        if state.finished {
            return if state.progress.stage == stage {
                Ok(())
            } else {
                Err(Error::new(
                    ErrorKind::Conflict,
                    "Installation already has a different final result",
                ))
            };
        }
        for index in 0..2 {
            let remaining = state.decoders[index].finish();
            state.append(index, &remaining)?;
            state.record_line(index);
        }
        let path = state.progress.log_path.clone();
        writeln!(
            state.log,
            "\nFinished: {}\nResult: {stage:?}\nMessage: {}",
            Utc::now().to_rfc3339(),
            clean_line(message, 500)
        )
        .map_err(|error| Error::io("Finish installation log", &path, error))?;
        state
            .log
            .flush()
            .and_then(|_| state.log.get_ref().sync_all())
            .map_err(|error| Error::io("Persist installation log", &path, error))?;
        state.progress.stage = stage;
        state.progress.message = clean_line(message, 500);
        if stage == Stage::Complete {
            state.progress.percent = 100;
        }
        state.finished = true;
        Ok(())
    }
}

impl OutputSink for Reporter {
    fn write(&self, stream: Stream, bytes: &[u8]) -> Result<()> {
        let mut state = self.state.lock();
        if state.finished {
            return Err(Error::new(
                ErrorKind::Conflict,
                "Output arrived after log finalization",
            ));
        }
        let index = usize::from(stream == Stream::Stderr);
        for chunk in bytes.chunks(8192) {
            let text = state.decoders[index].feed(chunk);
            state.append(index, &text)?;
        }
        state.flush_due()
    }
    fn tick(&self) -> Result<()> {
        self.state.lock().flush_due()
    }
}

impl State {
    fn append(&mut self, index: usize, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let label = if index == 0 { "stdout" } else { "stderr" };
        write!(self.log, "[{label}] {text}").map_err(|error| {
            Error::io("Append installation log", &self.progress.log_path, error)
        })?;
        for character in text.chars() {
            if matches!(character, '\r' | '\n') {
                self.record_line(index);
            } else {
                self.lines[index].push(character);
                // Keep the tail by Unicode boundaries, never an unbounded unfinished line.
                if self.lines[index].len() > 8192 {
                    let keep = self.lines[index]
                        .char_indices()
                        .rev()
                        .nth(2047)
                        .map_or(0, |(position, _)| position);
                    self.lines[index].drain(..keep);
                }
            }
        }
        let partial = clean_line(&self.lines[index], 500);
        if !partial.is_empty() {
            self.progress.message = partial;
        }
        Ok(())
    }
    fn record_line(&mut self, index: usize) {
        let line = clean_line(&self.lines[index], 500);
        self.lines[index].clear();
        if line.is_empty() {
            return;
        }
        self.progress.message = line.clone();
        if index == 1 {
            self.progress.stderr_tail.push_back(line);
            while self.progress.stderr_tail.len() > 5 {
                self.progress.stderr_tail.pop_front();
            }
        }
    }
    fn flush_due(&mut self) -> Result<()> {
        if !self.log.buffer().is_empty() && self.last_flush.elapsed() >= Duration::from_millis(120)
        {
            self.log.flush().map_err(|error| {
                Error::io("Flush installation log", &self.progress.log_path, error)
            })?;
            self.last_flush = Instant::now();
        }
        Ok(())
    }
}

#[derive(Default)]
struct Utf8 {
    pending: Vec<u8>,
}
impl Utf8 {
    fn feed(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut output = String::new();
        let mut start = 0;
        while start < self.pending.len() {
            match std::str::from_utf8(&self.pending[start..]) {
                Ok(valid) => {
                    output.push_str(valid);
                    start = self.pending.len();
                }
                Err(error) => {
                    let valid_end = start + error.valid_up_to();
                    output.push_str(
                        std::str::from_utf8(&self.pending[start..valid_end])
                            .expect("validated UTF-8 prefix"),
                    );
                    start = valid_end;
                    match error.error_len() {
                        Some(length) => {
                            output.push('\u{fffd}');
                            start += length;
                        }
                        None => break,
                    }
                }
            }
        }
        self.pending.drain(..start);
        output
    }
    fn finish(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }
}

pub fn clean_line(value: &str, limit: usize) -> String {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    let ansi =
        ANSI.get_or_init(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").expect("constant ANSI pattern"));
    ansi.replace_all(value, "")
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(limit)
        .collect()
}

pub fn latest_log(root: &DataRoot, profile_relative: &Path) -> Result<Option<PathBuf>> {
    let directory = root.resolve(profile_relative.join("logs"))?;
    if !directory.exists() {
        return Ok(None);
    }
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(&directory)
        .map_err(|error| Error::io("List installation logs", &directory, error))?
    {
        let entry =
            entry.map_err(|error| Error::io("Read installation log entry", &directory, error))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("install-")
            && name.ends_with(".log")
            && entry
                .file_type()
                .map_err(|error| Error::io("Inspect installation log", &entry.path(), error))?
                .is_file()
        {
            crate::storage::reject_links(&entry.path())?;
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths.pop())
}
