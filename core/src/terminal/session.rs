//! Profile-owned native terminals with automatic finalization.
use crate::{
    Error, ErrorKind, Result,
    domain::{ApplicationIdentity, Profile, ProfileId},
    profiles::ProfileRepository,
    profiles::activity::{ActivityKind, ActivityLease},
    profiles::credentials::{CredentialVault, ProfileCredentialLease},
    profiles::environment::{self, Environment},
    profiles::sharing::SharedData,
    storage::DataRoot,
    terminal::pty::{ExitSummary, NativeTerminal, Palette, SpawnRequest},
    terminal::{TerminalSize, TerminalView},
    tools::catalog::{Catalog, Tool},
    tools::install::command::{self, Programs},
};
use parking_lot::{Condvar, Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Weak},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;
mod resources;
use resources::{Member, Resources};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionState {
    Requested,
    Preparing,
    Running,
    Stopping,
    Exited,
    Finalizing,
    Closed,
    Failed,
    FailedCleanup,
}

#[derive(Debug, Clone)]
pub struct SessionStartRequest {
    pub tool_id: String,
    pub profile_id: ProfileId,
    pub program: Option<PathBuf>,
    pub arguments: Vec<String>,
    pub conpty_runtime: Option<PathBuf>,
    pub size: TerminalSize,
    pub history_limit: usize,
    pub palette: Option<Palette>,
    pub host_environment: Option<Environment>,
    pub full_permission: bool,
    pub working_directory: Option<PathBuf>,
}
impl SessionStartRequest {
    pub fn new(tool_id: impl Into<String>, profile_id: ProfileId) -> Self {
        Self {
            tool_id: tool_id.into(),
            profile_id,
            program: None,
            arguments: Vec::new(),
            conpty_runtime: None,
            size: TerminalSize::new(80, 24, 8, 16).expect("valid default size"),
            history_limit: 10_000,
            palette: None,
            host_environment: None,
            full_permission: false,
            working_directory: None,
        }
    }
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: Uuid,
    pub tool_id: String,
    pub profile_id: ProfileId,
    pub profile_name: String,
    pub state: SessionState,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
}
pub struct Session {
    pub id: Uuid,
    pub tool: Tool,
    pub profile: Profile,
    state: Mutex<SessionState>,
    terminal: Arc<NativeTerminal>,
    complete: Mutex<Option<ExitSummary>>,
    done: Condvar,
    member: Mutex<Option<Member>>,
}
impl Session {
    pub fn state(&self) -> SessionState {
        *self.state.lock()
    }
    pub fn is_running(&self) -> bool {
        self.terminal.exit_summary().is_none()
            && matches!(self.state(), SessionState::Running | SessionState::Stopping)
    }
    pub fn input(&self, bytes: Vec<u8>) -> Result<()> {
        self.terminal.input(bytes)
    }
    pub fn resize(&self, size: TerminalSize) -> Result<()> {
        self.terminal.resize(size)
    }
    pub fn scroll(&self, lines: i32) {
        self.terminal.scroll(lines);
    }
    pub fn view(&self) -> Result<TerminalView> {
        Ok(self.terminal.model())
    }
    pub fn title(&self) -> String {
        self.terminal.title()
    }
    /// Poll metadata without copying an unchanged title or exposing terminal ownership.
    pub fn title_update_since(&self, version: u64) -> Option<(u64, String)> {
        self.terminal.title_update_since(version)
    }
    /// Process exit is available before profile finalization has finished.
    /// Use `wait` or `stop` when the caller needs finalization to be complete.
    pub fn exit_summary(&self) -> Option<ExitSummary> {
        self.terminal.exit_summary()
    }
    pub fn wait(&self, timeout: Duration) -> Result<ExitSummary> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Invalid wait duration"))?;
        let mut result = self.complete.lock();
        while result.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::new(
                    ErrorKind::Busy,
                    "Session finalization is not finished",
                ));
            }
            self.done.wait_for(&mut result, remaining);
        }
        Ok(result.clone().expect("completed session"))
    }
    pub fn stop(&self, grace: Duration) -> Result<ExitSummary> {
        if grace > Duration::from_secs(60) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Session grace period exceeds one minute",
            ));
        }
        if let Some(result) = self.complete.lock().clone() {
            return Ok(result);
        }
        if self.terminal.exit_summary().is_none() {
            let mut state = self.state.lock();
            if *state == SessionState::Running {
                *state = SessionState::Stopping;
            }
            drop(state);
            self.terminal.stop(grace)?;
        }
        self.wait(Duration::from_secs(30))
    }
    fn finalize(&self, mut result: ExitSummary) {
        *self.state.lock() = SessionState::Finalizing;
        let mut errors = Vec::new();
        if let Some(member) = self.member.lock().take() {
            errors.extend(member.finish());
        }
        if !errors.is_empty() {
            if let Some(error) = result.error.take() {
                errors.insert(0, error);
            }
            result.error = Some(errors.join("; "));
        }
        *self.state.lock() = if result.error.is_some() {
            SessionState::FailedCleanup
        } else {
            SessionState::Closed
        };
        *self.complete.lock() = Some(result);
        self.done.notify_all();
    }
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id,
            tool_id: self.tool.id.clone(),
            profile_id: self.profile.id.clone(),
            profile_name: self.profile.name.clone(),
            state: self.state(),
            pid: Some(self.terminal.pid()),
            exit_code: self
                .terminal
                .exit_summary()
                .and_then(|result| result.exit_code),
        }
    }
}
pub struct SessionManager {
    root: DataRoot,
    catalog: Catalog,
    repository: Arc<ProfileRepository>,
    shared_data: Arc<SharedData>,
    identity: ApplicationIdentity,
    vault: Option<Arc<dyn CredentialVault>>,
    sessions: Arc<Mutex<BTreeMap<Uuid, Arc<Session>>>>,
    closed: RwLock<bool>,
    groups: Mutex<BTreeMap<(String, ProfileId), Weak<Resources>>>,
}
impl SessionManager {
    pub fn new(
        root: DataRoot,
        catalog: Catalog,
        repository: Arc<ProfileRepository>,
        shared_data: Arc<SharedData>,
        identity: ApplicationIdentity,
        vault: Option<Arc<dyn CredentialVault>>,
    ) -> Self {
        Self {
            root,
            catalog,
            repository,
            shared_data,
            identity,
            vault,
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            closed: RwLock::new(false),
            groups: Mutex::new(BTreeMap::new()),
        }
    }
    pub fn start(&self, request: SessionStartRequest) -> Result<Arc<Session>> {
        let closed = self.closed.read();
        if *closed {
            return Err(Error::new(ErrorKind::Busy, "Backend is shutting down"));
        }
        let tool = self.catalog.get(&request.tool_id)?.clone();
        let initial = self.repository.get(&tool.id, &request.profile_id)?;
        let profile = initial;
        let host = request
            .host_environment
            .unwrap_or_else(|| std::env::vars().collect());
        let mut env = environment::build(&self.root, &tool, &profile, &self.identity, &host)?;
        if let Some(directory) = request.working_directory {
            if !directory.is_absolute() || !directory.is_dir() {
                return Err(Error::new(
                    ErrorKind::NotFound,
                    "Saved working directory is unavailable",
                ));
            }
            for key in ["PWD", "INIT_CWD", "APP_PROFILE_WORKSPACE"] {
                env.values
                    .insert(key.into(), directory.to_string_lossy().into_owned());
            }
            env.cwd = directory;
        }
        let (program, mut arguments) = if let Some(program) = request.program {
            (program, Vec::new())
        } else {
            let resolved = command::resolve(&self.root, &tool, &Programs::discover(&host))?
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::NotFound,
                        format!("{} is not installed", tool.name),
                    )
                })?;
            (resolved.program, resolved.prefix)
        };
        if !program.is_absolute() || !program.is_file() {
            return Err(Error::new(
                ErrorKind::NotFound,
                "Session executable is unavailable",
            ));
        }
        let key = (tool.id.clone(), profile.id.clone());
        let mut groups = self.groups.lock();
        let permission = request.full_permission || profile.settings.full_permission;
        let member = if let Some(group) = groups.get(&key).and_then(Weak::upgrade) {
            match group.join(permission, &mut env.values)? {
                Some(member) => member,
                None => {
                    let group = Resources::create(self, &tool, &profile)?;
                    let member = group.join(permission, &mut env.values)?.expect("new group");
                    groups.insert(key, Arc::downgrade(&group));
                    member
                }
            }
        } else {
            let group = Resources::create(self, &tool, &profile)?;
            let member = group.join(permission, &mut env.values)?.expect("new group");
            groups.insert(key, Arc::downgrade(&group));
            member
        };
        arguments.extend(request.arguments);
        arguments.extend(member.arguments.clone());
        let spawned = NativeTerminal::spawn(SpawnRequest {
            program,
            arguments,
            cwd: env.cwd,
            environment: env.values,
            conpty_runtime: request.conpty_runtime,
            size: request.size,
            history_limit: request.history_limit,
            palette: request.palette.unwrap_or_default(),
        });
        let terminal = match spawned {
            Ok(terminal) => terminal,
            Err(error) => {
                let cleanup = member.finish();
                if cleanup.is_empty() {
                    return Err(error);
                }
                return Err(Error::new(
                    error.kind,
                    format!("{}; {}", error.message, cleanup.join("; ")),
                ));
            }
        };
        member.started();
        drop(groups);
        let session = Arc::new(Session {
            id: Uuid::new_v4(),
            tool,
            profile,
            state: Mutex::new(SessionState::Running),
            terminal: Arc::new(terminal),
            complete: Mutex::new(None),
            done: Condvar::new(),
            member: Mutex::new(Some(member)),
        });
        self.sessions.lock().insert(session.id, session.clone());
        let owner = session.clone();
        let sessions = self.sessions.clone();
        if let Err(error) = thread::Builder::new()
            .name("profile-finalization".into())
            .spawn(move || {
                let result = loop {
                    match owner.terminal.wait(Duration::from_secs(86_400)) {
                        Err(error) if error.kind == ErrorKind::Busy => continue,
                        Ok(result) => break result,
                        Err(error) => {
                            break ExitSummary {
                                exit_code: None,
                                error: Some(error.message),
                            };
                        }
                    }
                };
                owner.finalize(result);
                sessions.lock().remove(&owner.id);
            })
        {
            let result = session
                .terminal
                .stop(Duration::ZERO)
                .unwrap_or_else(|stop| ExitSummary {
                    exit_code: None,
                    error: Some(stop.message),
                });
            session.finalize(result);
            self.sessions.lock().remove(&session.id);
            return Err(Error::new(
                ErrorKind::Process,
                format!("Start session finalizer: {error}"),
            ));
        }
        Ok(session)
    }
    pub fn get(&self, id: &Uuid) -> Option<Arc<Session>> {
        self.sessions.lock().get(id).cloned()
    }
    pub fn list(&self) -> Vec<SessionSummary> {
        self.sessions
            .lock()
            .values()
            .map(|session| session.summary())
            .collect()
    }
    pub fn stop(&self, id: &Uuid, grace: Duration) -> Result<ExitSummary> {
        self.get(id)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "Session not found"))?
            .stop(grace)
    }
    pub fn close_all(&self, grace: Duration) -> Vec<Error> {
        if grace > Duration::from_secs(60) {
            return vec![Error::new(
                ErrorKind::InvalidInput,
                "Grace period cannot exceed 60 seconds",
            )];
        }
        *self.closed.write() = true;
        let sessions: Vec<_> = self.sessions.lock().values().cloned().collect();
        let mut errors = Vec::new();
        for session in sessions {
            match session.stop(grace) {
                Err(error) => errors.push(error),
                Ok(result) => {
                    if let Some(error) = result.error {
                        errors.push(Error::new(ErrorKind::Recovery, error));
                    }
                }
            }
        }
        errors
    }
}
impl Drop for SessionManager {
    fn drop(&mut self) {
        let _ = self.close_all(Duration::ZERO);
    }
}
