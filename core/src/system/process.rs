//! Controlled non-terminal processes for installation and version checks.

use std::{
    io::Read,
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;
pub use tokio_util::sync::CancellationToken;

use crate::{Error, ErrorKind, Result, profiles::environment::Environment};

pub mod command;
#[cfg(windows)]
mod windows;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Clone)]
pub struct CommandRequest {
    pub program: PathBuf,
    pub arguments: Vec<String>,
    pub cwd: PathBuf,
    pub environment: Environment,
    pub timeout: Duration,
    /// None streams without retaining output; version/metadata checks set a bound.
    pub capture_stdout: Option<usize>,
}

pub trait OutputSink: Send + Sync {
    /// Must return promptly. Failure stops the owned process tree.
    fn write(&self, stream: Stream, bytes: &[u8]) -> Result<()>;
    fn tick(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Default)]
pub struct DiscardOutput;
impl OutputSink for DiscardOutput {
    fn write(&self, _: Stream, _: &[u8]) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct CommandOutcome {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
}

enum Message {
    Bytes(Stream, Vec<u8>),
    Done,
    Error(Stream, std::io::Error),
}

/// Run on a service worker, never the UI thread. The only periodic wake while an
/// active process is quiet checks cancellation/timeout; idle services have no loop.
#[cfg(windows)]
pub fn run(
    request: CommandRequest,
    cancel: &CancellationToken,
    sink: Arc<dyn OutputSink>,
) -> Result<CommandOutcome> {
    validate(&request)?;
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    let deadline = Instant::now()
        .checked_add(request.timeout)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Process timeout is too large"))?;
    let process = windows::spawn(&request)?;
    let windows::OwnedProcess {
        control,
        stdout,
        stderr,
    } = process;
    let (sender, receiver) = mpsc::sync_channel(16);
    let mut readers = Vec::new();
    for (stream, reader) in [(Stream::Stdout, stdout), (Stream::Stderr, stderr)] {
        let sender = sender.clone();
        let worker = thread::Builder::new()
            .name(format!("job-{stream:?}"))
            .spawn(move || read_stream(stream, reader, sender));
        match worker {
            Ok(worker) => readers.push(worker),
            Err(error) => {
                let stop_error = control.terminate(1).err();
                drop(control);
                drop(receiver);
                for worker in readers {
                    let _joined = worker.join();
                }
                let message = match stop_error {
                    Some(stop) => format!("Start output reader: {error}; terminate: {stop}"),
                    None => format!("Start output reader: {error}"),
                };
                return Err(Error::new(ErrorKind::Process, message));
            }
        }
    }
    drop(sender);
    let mut output = Vec::new();
    let mut finished = 0;
    let mut exit = None;
    let mut drain_deadline = None;
    let result = (|| {
        while finished != 2 || exit.is_none() {
            sink.tick()?;
            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            if Instant::now() >= deadline {
                return Err(Error::new(ErrorKind::Process, "Process timed out"));
            }
            if drain_deadline.is_some_and(|limit| Instant::now() >= limit) {
                return Err(Error::new(
                    ErrorKind::Process,
                    "Process output drain timed out",
                ));
            }
            if exit.is_none() {
                exit = control.exit_status().map_err(process_error)?;
                if exit.is_some() {
                    // An exited parent's descendants may still hold output handles.
                    control.terminate(0).map_err(process_error)?;
                    drain_deadline = Some(Instant::now() + Duration::from_secs(10));
                }
            }
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(Message::Bytes(stream, bytes)) => {
                    sink.write(stream, &bytes)?;
                    if stream == Stream::Stdout {
                        if let Some(limit) = request.capture_stdout {
                            if output
                                .len()
                                .checked_add(bytes.len())
                                .is_none_or(|size| size > limit)
                            {
                                return Err(Error::new(
                                    ErrorKind::Process,
                                    "Process stdout exceeded its capture limit",
                                ));
                            }
                            output.extend_from_slice(&bytes);
                        }
                    }
                }
                Ok(Message::Done) => finished += 1,
                Ok(Message::Error(stream, error)) => {
                    return Err(Error::new(
                        ErrorKind::Process,
                        format!("Read {stream:?}: {error}"),
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(mpsc::RecvTimeoutError::Disconnected) if finished == 2 => {
                    control
                        .wait_exit(Duration::from_millis(25))
                        .map_err(process_error)?;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Error::new(
                        ErrorKind::Process,
                        "Output readers ended without completion",
                    ));
                }
            }
        }
        Ok(CommandOutcome {
            exit_code: exit.and_then(|status| status.code()),
            stdout: output,
        })
    })();
    let stop_error = if result.is_err() {
        control.terminate(1).err().map(process_error)
    } else {
        None
    };
    // Closing the final job handle also kills the tree if termination reported
    // an error. Do this before joining any reader blocked on an inherited pipe.
    drop(control);
    drop(receiver);
    let mut join_error = stop_error;
    for reader in readers {
        if reader.join().is_err() {
            join_error = Some(Error::new(ErrorKind::Process, "Output reader panicked"));
        }
    }
    match (result, join_error) {
        (Err(primary), Some(cleanup)) => Err(Error::new(
            ErrorKind::Process,
            format!("{primary}; {cleanup}"),
        )),
        (Err(primary), None) => Err(primary),
        (Ok(_), Some(error)) => Err(error),
        (Ok(value), None) => Ok(value),
    }
}

#[cfg(not(windows))]
pub fn run(
    _: CommandRequest,
    _: &CancellationToken,
    _: Arc<dyn OutputSink>,
) -> Result<CommandOutcome> {
    Err(Error::new(
        ErrorKind::Unsupported,
        "Native job execution is implemented for Windows",
    ))
}

fn validate(request: &CommandRequest) -> Result<()> {
    if !request.program.is_absolute()
        || !request.program.is_file()
        || !request.cwd.is_absolute()
        || !request.cwd.is_dir()
        || request.timeout.is_zero()
        || request.timeout > Duration::from_secs(86400)
        || request
            .capture_stdout
            .is_some_and(|limit| limit > 16 * 1024 * 1024)
        || request.arguments.iter().any(|value| value.contains('\0'))
        || request
            .environment
            .iter()
            .any(|(key, value)| key.is_empty() || key.contains(['=', '\0']) || value.contains('\0'))
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Invalid controlled process request",
        ));
    }
    Ok(())
}

fn read_stream(stream: Stream, mut reader: impl Read, sender: mpsc::SyncSender<Message>) {
    let mut buffer = [0; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _done = sender.send(Message::Done);
                return;
            }
            Ok(length) => {
                if sender
                    .send(Message::Bytes(stream, buffer[..length].to_vec()))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => (),
            Err(error) => {
                let _error = sender.send(Message::Error(stream, error));
                return;
            }
        }
    }
}

fn process_error(error: std::io::Error) -> Error {
    Error::new(ErrorKind::Process, error.to_string())
}
fn cancelled() -> Error {
    Error::new(ErrorKind::Cancelled, "Process cancelled")
}
