use super::{
    command::{self, Launch, Programs, RECORD, RuntimeRecord},
    download::Http,
    reporter::{Reporter, Stage},
    version,
};
use crate::{
    Error, ErrorKind, Result,
    domain::{ApplicationIdentity, Profile, ProfileId},
    profiles::ProfileRepository,
    profiles::activity::ActivityKind,
    profiles::environment::{self, Environment},
    storage::{DataRoot, artifacts::ArtifactTransaction},
    system::process::{self, CancellationToken, CommandRequest},
    tools::catalog::Tool,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Notify;
use uuid::Uuid;
mod native;

#[derive(Deserialize)]
struct NpmRelease {
    version: String,
}

#[derive(Clone)]
pub struct Installer {
    repository: Arc<ProfileRepository>,
    identity: ApplicationIdentity,
    host: Environment,
    programs: Programs,
    http: Http,
    jobs: Arc<Mutex<BTreeMap<String, Arc<InstallJob>>>>,
    shutdown: CancellationToken,
}

pub struct InstallJob {
    pub tool_id: String,
    pub profile_id: ProfileId,
    cancel: CancellationToken,
    result: Mutex<Option<Result<Installed>>>,
    done: Notify,
}
impl InstallJob {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
    pub fn completed(&self) -> Option<Result<Installed>> {
        self.result.lock().clone()
    }
    pub async fn wait(&self) -> Result<Installed> {
        loop {
            let notified = self.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(value) = self.completed() {
                return value;
            }
            notified.await;
        }
    }
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct NpmJob<'a> {
    root: &'a DataRoot,
    tool: &'a Tool,
    profile: &'a Profile,
    requested: Option<&'a str>,
    cancel: &'a CancellationToken,
    reporter: Arc<Reporter>,
    scratch: &'a Path,
}

#[derive(Debug, Clone, Serialize)]
pub struct Installed {
    pub tool_id: String,
    pub profile_id: ProfileId,
    pub version: String,
    pub log_path: PathBuf,
    pub warnings: Vec<Error>,
}

impl Installer {
    pub fn identity(&self) -> &ApplicationIdentity {
        &self.identity
    }
    pub fn host(&self) -> &Environment {
        &self.host
    }
    pub fn programs(&self) -> Programs {
        self.programs.clone()
    }
    pub fn new(
        repository: Arc<ProfileRepository>,
        identity: ApplicationIdentity,
        host: Environment,
        programs: Programs,
        http: Http,
    ) -> Result<Self> {
        identity.validate()?;
        Ok(Self {
            repository,
            identity,
            host,
            programs,
            http,
            jobs: Arc::new(Mutex::new(BTreeMap::new())),
            shutdown: CancellationToken::new(),
        })
    }

    pub fn stop(&self) {
        self.shutdown.cancel();
    }

    /// One live installation per tool; joining does not create another CLI copy.
    pub fn start(
        self: &Arc<Self>,
        tool_id: &str,
        profile_id: &ProfileId,
        known: Option<super::release::Release>,
    ) -> Result<(Arc<InstallJob>, bool)> {
        if self.shutdown.is_cancelled() {
            return Err(cancelled());
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            Error::new(
                ErrorKind::InvalidInput,
                "Installer requires an asynchronous service runtime",
            )
        })?;
        let tool = self.repository.catalog().get(tool_id)?.clone();
        let profile = self.repository.get(tool_id, profile_id)?;
        let mut jobs = self.jobs.lock();
        if let Some(existing) = jobs.get(tool_id) {
            return Ok((existing.clone(), true));
        }
        let lease = self.repository.activities().reserve(
            &tool,
            Some(profile_id),
            &profile.settings,
            ActivityKind::Install,
        )?;
        let job = Arc::new(InstallJob {
            tool_id: tool.id.clone(),
            profile_id: profile.id.clone(),
            cancel: self.shutdown.child_token(),
            result: Mutex::new(None),
            done: Notify::new(),
        });
        jobs.insert(tool.id.clone(), job.clone());
        let installer = self.clone();
        let current = job.clone();
        runtime.spawn(async move {
            let work = installer.clone();
            let id = tool.id.clone();
            let token = current.cancel.clone();
            let result = tokio::spawn(async move {
                if tool.installer.kind == "npm" {
                    work.npm_owned(
                        tool.clone(),
                        profile,
                        known.map(|release| release.version),
                        token,
                    )
                    .await
                } else {
                    work.native_owned(tool.clone(), profile, known, token).await
                }
            })
            .await
            .unwrap_or_else(|error| {
                Err(Error::new(
                    ErrorKind::Recovery,
                    format!("Installer task ended unexpectedly: {error}"),
                ))
            });
            if result
                .as_ref()
                .is_err_and(|error| error.kind == ErrorKind::Recovery)
            {
                installer.repository.require_recovery();
            }
            drop(lease);
            *current.result.lock() = Some(result);
            let mut jobs = installer.jobs.lock();
            if jobs
                .get(&id)
                .is_some_and(|active| Arc::ptr_eq(active, &current))
            {
                jobs.remove(&id);
            }
            drop(jobs);
            current.done.notify_waiters();
        });
        Ok((job, false))
    }

    pub async fn install_npm(
        &self,
        tool_id: &str,
        profile_id: &ProfileId,
        requested: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Installed> {
        let tool = self.repository.catalog().get(tool_id)?.clone();
        if tool.installer.kind != "npm" {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "This operation requires an npm CLI adapter",
            ));
        }
        let profile = self.repository.get(tool_id, profile_id)?;
        let lease = self.repository.activities().reserve(
            &tool,
            Some(profile_id),
            &profile.settings,
            ActivityKind::Install,
        )?;
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            Error::new(
                ErrorKind::InvalidInput,
                "Installer requires an asynchronous service runtime",
            )
        })?;
        let cancellation = cancel.child_token();
        let _cancel_on_drop = CancelOnDrop(cancellation.clone());
        let requested = requested.map(String::from);
        let installer = self.clone();
        // The owned task retains its reservation through process/rollback cleanup
        // even if the caller's awaiting future is dropped or aborted.
        runtime
            .spawn(async move {
                let _lease = lease;
                installer
                    .npm_owned(tool, profile, requested, cancellation)
                    .await
            })
            .await
            .map_err(join_error)?
    }

    async fn npm_owned(
        &self,
        tool: Tool,
        profile: Profile,
        requested: Option<String>,
        cancel: CancellationToken,
    ) -> Result<Installed> {
        let root = self.repository.root().clone();
        let job = Uuid::new_v4();
        let reporter = Arc::new(Reporter::new(&root, &tool, &profile, &self.identity, job)?);
        let scratch = tool
            .profile_relative(&profile)
            .join("Temp")
            .join(format!("install-{job}"));
        root.create_new_dir(&scratch)?;
        let result = self
            .npm(NpmJob {
                root: &root,
                tool: &tool,
                profile: &profile,
                requested: requested.as_deref(),
                cancel: &cancel,
                reporter: reporter.clone(),
                scratch: &scratch,
            })
            .await;
        if result
            .as_ref()
            .is_err_and(|error| error.kind == ErrorKind::Recovery)
        {
            self.repository.require_recovery();
        }
        let cleanup = root.remove_owned_tree(&scratch);
        let (stage, message) = match &result {
            Ok(value) => (
                Stage::Complete,
                format!("{} {} is ready", tool.name, value.version),
            ),
            Err(error) if error.kind == ErrorKind::Cancelled => {
                (Stage::Cancelled, error.to_string())
            }
            Err(error) => (Stage::Failed, error.to_string()),
        };
        let finalized = reporter.finish(stage, &message);
        match result {
            Ok(mut value) => {
                if let Err(error) = cleanup {
                    value.warnings.push(error);
                }
                finalized?;
                Ok(value)
            }
            Err(primary) => {
                let mut failures = Vec::new();
                if let Err(error) = cleanup {
                    failures.push(error.to_string());
                }
                if let Err(error) = finalized {
                    failures.push(error.to_string());
                }
                if failures.is_empty() {
                    Err(primary)
                } else {
                    Err(Error::new(
                        ErrorKind::Recovery,
                        format!("{primary}; cleanup: {}", failures.join("; ")),
                    ))
                }
            }
        }
    }

    async fn npm(&self, job: NpmJob<'_>) -> Result<Installed> {
        let NpmJob {
            root,
            tool,
            profile,
            requested,
            cancel,
            reporter,
            scratch,
        } = job;
        let package =
            tool.installer.package.as_deref().ok_or_else(|| {
                Error::new(ErrorKind::CorruptData, "npm adapter package is missing")
            })?;
        command::package_name(package)?;
        let node = self
            .programs
            .node
            .clone()
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "Node.js was not found"))?;
        let npm_cli = self
            .programs
            .npm_cli
            .clone()
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "npm CLI entry was not found"))?;
        reporter.advance(Stage::Resolving, 4, "Resolve package release")?;
        let expected = match requested {
            Some(version) => version.to_owned(),
            None => {
                let mut url = super::download::release_url("https://registry.npmjs.org/")?;
                url.path_segments_mut()
                    .map_err(|_| Error::new(ErrorKind::InvalidInput, "Invalid npm registry URL"))?
                    .push(package)
                    .push("latest");
                self.http
                    .json::<NpmRelease>(url.as_str(), cancel)
                    .await?
                    .version
            }
        };
        super::release::version_name(&expected)?;
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let mut tx = ArtifactTransaction::new(root)?;
        let operation = tx.directory();
        let staged = tx.payload().join("npm");
        let prepared = (async {
            let mut metadata: serde_json::Value = match root
                .read_limited(PathBuf::from(&tool.dir).join("package.json"), 1024 * 1024)?
            {
                Some(bytes) => serde_json::from_slice(&bytes).map_err(Error::corrupt)?,
                None => serde_json::json!({}),
            };
            if !metadata.is_object() {
                return Err(Error::new(
                    ErrorKind::CorruptData,
                    "Runtime package metadata must be an object",
                ));
            }
            metadata["name"] = serde_json::Value::String(format!("cli-{}", tool.id));
            metadata["version"] = serde_json::Value::String("1.0.0".into());
            metadata["private"] = serde_json::Value::Bool(true);
            root.write_json(staged.join("package.json"), &metadata)?;
            if let Some(lock) = root.read_limited(
                PathBuf::from(&tool.dir).join("package-lock.json"),
                16 * 1024 * 1024,
            )? {
                root.write(staged.join("package-lock.json"), &lock)?;
            }
            let staged_path = root.resolve(&staged)?;
            let mut env = environment::install(root, tool, profile, &self.identity, &self.host)?;
            let temp = root.resolve(scratch)?;
            for key in ["TEMP", "TMP"] {
                env.values.insert(key.into(), temp.display().to_string());
            }
            for key in [
                "PWD",
                "INIT_CWD",
                "GIT_CEILING_DIRECTORIES",
                "npm_config_prefix",
            ] {
                env.values
                    .insert(key.into(), staged_path.display().to_string());
            }
            env.values.insert(
                "npm_config_cache".into(),
                temp.join("npm-cache").display().to_string(),
            );
            let mut arguments = vec![
                npm_cli
                    .to_str()
                    .ok_or_else(|| {
                        Error::new(ErrorKind::InvalidInput, "npm entry must be Unicode")
                    })?
                    .to_owned(),
                "install".into(),
                "--prefix".into(),
                staged_path.display().to_string(),
                "--save-exact".into(),
                format!("{package}@{expected}"),
                "--no-fund".into(),
                "--no-audit".into(),
                "--loglevel=http".into(),
                "--fetch-retries=2".into(),
                "--fetch-timeout=60000".into(),
            ];
            arguments.extend(tool.installer.args.clone());
            reporter.advance(Stage::Running, 15, "Install shared CLI package")?;
            let request = CommandRequest {
                program: node,
                arguments,
                cwd: staged_path.clone(),
                environment: env.values,
                timeout: Duration::from_secs(900),
                capture_stdout: None,
            };
            let cancellation = cancel.clone();
            let sink = reporter.clone();
            let outcome =
                tokio::task::spawn_blocking(move || process::run(request, &cancellation, sink))
                    .await
                    .map_err(join_error)??;
            if outcome.exit_code != Some(0) {
                return Err(Error::new(
                    ErrorKind::Process,
                    reporter.failure("npm installer failed"),
                ));
            }
            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            reporter.advance(Stage::Verifying, 90, "Verify candidate CLI")?;
            let candidate =
                command::npm_command(root, tool, &staged, &self.programs)?.ok_or_else(|| {
                    Error::new(
                        ErrorKind::NotFound,
                        "Installed package has no executable CLI entry",
                    )
                })?;
            let env = environment::build(root, tool, profile, &self.identity, &self.host)?;
            let request = candidate.request(
                env.cwd,
                env.values,
                &["--version".into()],
                Duration::from_secs(15),
            );
            let cancellation = cancel.clone();
            let actual = tokio::task::spawn_blocking(move || version::read(request, &cancellation))
                .await
                .map_err(join_error)??;
            verify_version(&expected, &actual)?;
            let entry = candidate
                .entry
                .strip_prefix(&staged_path)
                .map_err(Error::corrupt)?
                .to_owned();
            let launch = if candidate.prefix.is_empty() {
                Launch::Executable { path: entry }
            } else {
                Launch::Node {
                    script: entry,
                    node: None,
                }
            };
            let record = RuntimeRecord {
                schema: 1,
                tool_id: tool.id.clone(),
                version: actual.clone(),
                launch,
                generation: None,
            };
            root.write_json(tx.payload().join(RECORD), &record)?;
            Ok((actual, record))
        })
        .await;
        let (actual, _record) = match prepared {
            Ok(value) => value,
            Err(primary) => {
                return match tx.discard() {
                    Ok(()) => Err(primary),
                    Err(cleanup) => Err(Error::new(
                        ErrorKind::Recovery,
                        format!("{primary}; prepare cleanup: {cleanup}"),
                    )),
                };
            }
        };
        let staging = (|| {
            for name in ["node_modules", "package.json", "package-lock.json"] {
                let path = staged.join(name);
                if root.resolve(&path)?.exists() {
                    tx.replace(PathBuf::from(&tool.dir).join(name), path)?;
                }
            }
            tx.replace(
                PathBuf::from(&tool.dir).join(RECORD),
                tx.payload().join(RECORD),
            )
        })();
        if let Err(primary) = staging {
            return match tx.discard() {
                Ok(()) => Err(primary),
                Err(cleanup) => Err(Error::new(
                    ErrorKind::Recovery,
                    format!("{primary}; stage cleanup: {cleanup}"),
                )),
            };
        }
        reporter.advance(Stage::Activating, 96, "Activate verified CLI")?;
        let root_copy = root.clone();
        let tool_copy = tool.clone();
        let programs = self.programs.clone();
        let identity = self.identity.clone();
        let host = self.host.clone();
        let profile_copy = profile.clone();
        let expected_copy = expected;
        let cancellation = cancel.clone();
        let warnings = tokio::task::spawn_blocking(move || {
            tx.commit_validated(|| {
                if cancellation.is_cancelled() {
                    return Err(cancelled());
                }
                let request = command::profile_request(
                    &root_copy,
                    &tool_copy,
                    &profile_copy,
                    &identity,
                    &host,
                    &programs,
                    &["--version".into()],
                )?
                .ok_or_else(|| Error::new(ErrorKind::NotFound, "Activated command is missing"))?;
                let actual = version::read(request, &cancellation)?;
                verify_version(&expected_copy, &actual)
            })
        })
        .await
        .map_err(|error| {
            Error::new(
                ErrorKind::Recovery,
                format!("Artifact activation worker failed: {error}"),
            )
        })??;
        debug_assert!(!root.resolve(&operation)?.exists() || !warnings.is_empty());
        Ok(Installed {
            tool_id: tool.id.clone(),
            profile_id: profile.id.clone(),
            version: actual,
            log_path: reporter.snapshot().log_path,
            warnings,
        })
    }
}

pub fn verify_version(expected: &str, actual: &str) -> Result<()> {
    if actual.is_empty() || version::newer(expected, actual) || version::newer(actual, expected) {
        return Err(Error::new(
            ErrorKind::Integrity,
            format!("Expected {expected}, CLI reports {actual}"),
        ));
    }
    Ok(())
}
fn cancelled() -> Error {
    Error::new(ErrorKind::Cancelled, "Installation cancelled")
}
fn join_error(error: tokio::task::JoinError) -> Error {
    Error::new(
        ErrorKind::Process,
        format!("Installation worker failed: {error}"),
    )
}
