use super::{CancelOnDrop, Installed, Installer, cancelled, join_error, verify_version};
use crate::tools::install::{
    archive::{self, Limits},
    command::{self, Launch, RECORD, RuntimeRecord},
    download::Checksum,
    release::{Architecture, Release, version_name},
    reporter::{Reporter, Stage},
    version,
};
use crate::{
    Error, ErrorKind, Result,
    domain::{Profile, ProfileId},
    profiles::activity::ActivityKind,
    profiles::environment,
    storage::{
        DataRoot,
        artifacts::ArtifactTransaction,
        generations::{self, Generation},
    },
    system::process::{self, CancellationToken, CommandRequest},
    tools::catalog::Tool,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

struct Job<'a> {
    root: &'a DataRoot,
    tool: &'a Tool,
    profile: &'a Profile,
    cancel: &'a CancellationToken,
    reporter: Arc<Reporter>,
    scratch: &'a Path,
}

impl Installer {
    pub async fn install(
        &self,
        tool_id: &str,
        profile_id: &ProfileId,
        cancel: &CancellationToken,
    ) -> Result<Installed> {
        let tool = self.repository.catalog().get(tool_id)?;
        if tool.installer.kind == "npm" {
            return self.install_npm(tool_id, profile_id, None, cancel).await;
        }
        self.install_native(tool_id, profile_id, None, cancel).await
    }

    /// Known release is supplied by the update resolver; not an arbitrary shell command.
    pub async fn install_native(
        &self,
        tool_id: &str,
        profile_id: &ProfileId,
        known: Option<Release>,
        cancel: &CancellationToken,
    ) -> Result<Installed> {
        let tool = self.repository.catalog().get(tool_id)?.clone();
        if !matches!(
            tool.installer.kind.as_str(),
            "agy-release" | "github-release" | "cursor-release" | "powershell-script"
        ) {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "CLI has no native installer adapter",
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
                "Installer requires an asynchronous runtime",
            )
        })?;
        let cancellation = cancel.child_token();
        let _cancel_on_drop = CancelOnDrop(cancellation.clone());
        let installer = self.clone();
        runtime
            .spawn(async move {
                let _lease = lease;
                installer
                    .native_owned(tool, profile, known, cancellation)
                    .await
            })
            .await
            .map_err(join_error)?
    }

    pub(super) async fn native_owned(
        &self,
        tool: Tool,
        profile: Profile,
        known: Option<Release>,
        cancel: CancellationToken,
    ) -> Result<Installed> {
        let root = self.repository.root().clone();
        let id = uuid::Uuid::new_v4();
        let reporter = Arc::new(Reporter::new(&root, &tool, &profile, &self.identity, id)?);
        let scratch = tool
            .profile_relative(&profile)
            .join("Temp")
            .join(format!("install-{id}"));
        root.create_new_dir(&scratch)?;
        let result = self
            .native_job(
                Job {
                    root: &root,
                    tool: &tool,
                    profile: &profile,
                    cancel: &cancel,
                    reporter: reporter.clone(),
                    scratch: &scratch,
                },
                known,
            )
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
                let failures = [cleanup.err(), finalized.err()]
                    .into_iter()
                    .flatten()
                    .map(|error| error.to_string())
                    .collect::<Vec<_>>();
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

    async fn native_job(&self, job: Job<'_>, known: Option<Release>) -> Result<Installed> {
        let Job {
            root,
            tool,
            profile,
            cancel,
            reporter,
            scratch,
        } = job;
        reporter.advance(Stage::Resolving, 4, "Resolve CLI release")?;
        let release = match known {
            Some(release) => release,
            None => match tool.installer.kind.as_str() {
                "agy-release" => {
                    self.http
                        .agy_release(
                            required(&tool.installer.manifest, "release manifest")?,
                            cancel,
                        )
                        .await?
                }
                "github-release" => {
                    let repo = required(&tool.installer.repo, "release repository")?;
                    repository_name(repo)?;
                    self.http
                        .github_release(
                            &format!("https://api.github.com/repos/{repo}/releases/latest"),
                            required(&tool.installer.asset, "release asset")?,
                            cancel,
                        )
                        .await?
                }
                "cursor-release" => {
                    self.http
                        .cursor_release(
                            required(&tool.installer.url, "Cursor bootstrap")?,
                            architecture(),
                            cancel,
                        )
                        .await?
                }
                "powershell-script" => self.http.aider_release(cancel).await?,
                _ => {
                    return Err(Error::new(
                        ErrorKind::Unsupported,
                        "Unknown native installation kind",
                    ));
                }
            },
        };
        version_name(&release.version)?;
        if tool.id == "agy" && !matches!(release.checksum, Some(Checksum::Sha512(_))) {
            return Err(Error::new(
                ErrorKind::Integrity,
                "AGY release requires its official SHA512",
            ));
        }
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let previous = root
            .read_limited(PathBuf::from(&tool.dir).join(RECORD), 1024 * 1024)?
            .map(|bytes| serde_json::from_slice::<RuntimeRecord>(&bytes).map_err(Error::corrupt))
            .transpose()?;
        let generation = Generation::new(root, tool)?;
        let base = generation.path();
        let preparation = (async {
            reporter.advance(Stage::Downloading, 12, "Download release artifact")?;
            let download = scratch.join("release.partial");
            let receipt = self
                .http
                .download(
                    root,
                    &release.url,
                    &download,
                    1024 * 1024 * 1024,
                    release.checksum.as_ref(),
                    cancel,
                )
                .await?;
            if release
                .size
                .is_some_and(|expected| expected != receipt.bytes)
            {
                return Err(Error::new(
                    ErrorKind::Integrity,
                    "Release artifact size does not match",
                ));
            }
            let launch = if tool.id == "agy" {
                let target = base.join("AppData/Local/agy/bin/agy.exe");
                root.create_dir(target.parent().unwrap())?;
                root.move_path(&download, &target)?;
                Launch::Executable {
                    path: generation.relative().join("AppData/Local/agy/bin/agy.exe"),
                }
            } else {
                reporter.advance(Stage::Extracting, 78, "Extract release package")?;
                let expanded = base.join("unpacked");
                let root_copy = root.clone();
                let archive = download.clone();
                let destination = expanded.clone();
                let cancellation = cancel.clone();
                tokio::task::spawn_blocking(move || {
                    archive::extract_zip(
                        &root_copy,
                        &archive,
                        &destination,
                        Limits::default(),
                        &cancellation,
                    )
                })
                .await
                .map_err(join_error)??;
                match tool.id.as_str() {
                    "goose" => {
                        let name = required(&tool.installer.executable, "release executable")?;
                        command::runtime_path(Path::new(name))?;
                        let source = unique(root, &expanded, name, false)?;
                        let target = base.join("bin").join(name);
                        root.create_dir(target.parent().unwrap())?;
                        root.move_path(source, target)?;
                        Launch::Executable {
                            path: generation.relative().join("bin").join(name),
                        }
                    }
                    "cursor-agent" => {
                        let package = unique(root, &expanded, "dist-package", true)?;
                        let bin = base.join("bin");
                        root.create_dir(&bin)?;
                        let absolute = root.resolve(&package)?;
                        for entry in fs::read_dir(&absolute)
                            .map_err(|error| Error::io("Read Cursor package", &absolute, error))?
                        {
                            let entry = entry.map_err(|error| {
                                Error::io("Read Cursor command", &absolute, error)
                            })?;
                            if entry
                                .file_type()
                                .map_err(|error| {
                                    Error::io("Inspect Cursor command", &entry.path(), error)
                                })?
                                .is_file()
                                && entry
                                    .file_name()
                                    .to_string_lossy()
                                    .starts_with("cursor-agent")
                            {
                                root.copy_new_file(
                                    package.join(entry.file_name()),
                                    bin.join(entry.file_name()),
                                )?;
                            }
                        }
                        let version_dir = bin.join("versions").join(&release.version);
                        root.move_path(package, &version_dir)?;
                        let exe = generation.relative().join("bin/cursor-agent.exe");
                        let batch = generation.relative().join("bin/cursor-agent.cmd");
                        if root.resolve(PathBuf::from(&tool.dir).join(&exe))?.is_file() {
                            Launch::Executable { path: exe }
                        } else if root
                            .resolve(PathBuf::from(&tool.dir).join(&batch))?
                            .is_file()
                        {
                            Launch::Batch { path: batch }
                        } else {
                            return Err(Error::new(
                                ErrorKind::NotFound,
                                "Cursor package has no Windows command",
                            ));
                        }
                    }
                    "aider" => {
                        let uv = unique(root, &expanded, "uv.exe", false)?;
                        let uv_path = base.join("bin/uv.exe");
                        root.create_dir(uv_path.parent().unwrap())?;
                        root.move_path(uv, &uv_path)?;
                        let mut env =
                            environment::install(root, tool, profile, &self.identity, &self.host)?;
                        let home = root.resolve(&base)?;
                        let temp = root.resolve(scratch)?;
                        for (key, relative) in [
                            ("UV_TOOL_DIR", "uv/tools"),
                            ("UV_TOOL_BIN_DIR", "bin"),
                            ("UV_PYTHON_INSTALL_DIR", "uv/python"),
                        ] {
                            env.values
                                .insert(key.into(), home.join(relative).display().to_string());
                        }
                        env.values.insert(
                            "UV_CACHE_DIR".into(),
                            temp.join("uv-cache").display().to_string(),
                        );
                        env.values.insert("UV_NO_MODIFY_PATH".into(), "1".into());
                        for key in ["TEMP", "TMP"] {
                            env.values.insert(key.into(), temp.display().to_string());
                        }
                        reporter.advance(
                            Stage::Running,
                            84,
                            "Install isolated Python tool environment",
                        )?;
                        let request = CommandRequest {
                            program: root.resolve(uv_path)?,
                            arguments: vec![
                                "tool".into(),
                                "install".into(),
                                "--force".into(),
                                "--python".into(),
                                "python3.12".into(),
                                "--with".into(),
                                "pip".into(),
                                format!("aider-chat@{}", release.version),
                            ],
                            cwd: home,
                            environment: env.values,
                            timeout: Duration::from_secs(900),
                            capture_stdout: None,
                        };
                        let cancellation = cancel.clone();
                        let sink = reporter.clone();
                        let outcome = tokio::task::spawn_blocking(move || {
                            process::run(request, &cancellation, sink)
                        })
                        .await
                        .map_err(join_error)??;
                        if outcome.exit_code != Some(0) {
                            return Err(Error::new(
                                ErrorKind::Process,
                                reporter.failure("Python tool installation failed"),
                            ));
                        }
                        Launch::Executable {
                            path: generation.relative().join("bin/aider.exe"),
                        }
                    }
                    _ => {
                        return Err(Error::new(
                            ErrorKind::Unsupported,
                            "Native adapter is not configured",
                        ));
                    }
                }
            };
            if root.resolve(expanded_path(&base))?.exists() {
                root.remove_owned_tree(expanded_path(&base))?;
            }
            reporter.advance(Stage::Verifying, 92, "Verify candidate runtime")?;
            let candidate =
                command::resolve_launch(root, Path::new(&tool.dir), &launch, &self.programs, None)?
                    .ok_or_else(|| {
                        Error::new(ErrorKind::NotFound, "Candidate CLI command is missing")
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
            verify_version(&release.version, &actual)?;
            Ok((actual, launch))
        })
        .await;
        let (actual, launch) = match preparation {
            Ok(value) => value,
            Err(primary) => return discard_generation(generation, primary),
        };
        let record = RuntimeRecord {
            schema: 1,
            tool_id: tool.id.clone(),
            version: actual.clone(),
            launch,
            generation: Some(generation.relative()),
        };
        let mut transaction = match ArtifactTransaction::new(root) {
            Ok(value) => value,
            Err(primary) => return discard_generation(generation, primary),
        };
        let payload = transaction.payload().join(RECORD);
        let staged = (|| {
            root.write_json(&payload, &record)?;
            transaction.replace(PathBuf::from(&tool.dir).join(RECORD), payload)?;
            reporter.advance(Stage::Activating, 96, "Select verified shared runtime")
        })();
        if let Err(primary) = staged {
            let primary = match transaction.discard() {
                Ok(()) => primary,
                Err(cleanup) => Error::new(
                    ErrorKind::Recovery,
                    format!("{primary}; activation preparation cleanup: {cleanup}"),
                ),
            };
            return discard_generation(generation, primary);
        }
        let root_copy = root.clone();
        let tool_copy = tool.clone();
        let profile_copy = profile.clone();
        let programs = self.programs.clone();
        let identity = self.identity.clone();
        let host = self.host.clone();
        let expected = release.version.clone();
        let cancellation = cancel.clone();
        let committed = tokio::task::spawn_blocking(move || {
            transaction.commit_validated(|| {
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
                .ok_or_else(|| Error::new(ErrorKind::NotFound, "Activated CLI is missing"))?;
                verify_version(&expected, &version::read(request, &cancellation)?)
            })
        })
        .await
        .map_err(|error| {
            Error::new(
                ErrorKind::Recovery,
                format!("Artifact activation worker failed: {error}"),
            )
        })?;
        let mut warnings = match committed {
            Ok(warnings) => warnings,
            Err(primary) if primary.kind == ErrorKind::Recovery => return Err(primary),
            Err(primary) => return discard_generation(generation, primary),
        };
        let current = generation.relative();
        warnings.extend(generation.published()?);
        if let Some(old) = previous.and_then(|record| record.generation) {
            if let Err(error) = generations::cleanup_previous(root, tool, &old, &current) {
                warnings.push(error);
            }
        }
        Ok(Installed {
            tool_id: tool.id.clone(),
            profile_id: profile.id.clone(),
            version: actual,
            log_path: reporter.snapshot().log_path,
            warnings,
        })
    }
}

fn expanded_path(base: &Path) -> PathBuf {
    base.join("unpacked")
}
fn discard_generation(generation: Generation, primary: Error) -> Result<Installed> {
    match generation.discard() {
        Ok(()) => Err(primary),
        Err(cleanup) => Err(Error::new(
            ErrorKind::Recovery,
            format!("{primary}; generation cleanup: {cleanup}"),
        )),
    }
}
fn required<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str> {
    value
        .as_deref()
        .ok_or_else(|| Error::new(ErrorKind::CorruptData, format!("Installer has no {name}")))
}
fn architecture() -> Architecture {
    Architecture::current()
}
fn repository_name(value: &str) -> Result<()> {
    let parts: Vec<_> = value.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                || matches!(*part, "." | "..")
        })
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Invalid release repository",
        ));
    }
    Ok(())
}
fn unique(root: &DataRoot, base: &Path, name: &str, directory: bool) -> Result<PathBuf> {
    let mut pending = vec![base.to_owned()];
    let mut matching = Vec::new();
    let mut count = 0;
    while let Some(path) = pending.pop() {
        let absolute = root.resolve(&path)?;
        for entry in fs::read_dir(&absolute)
            .map_err(|error| Error::io("Find release artifact", &absolute, error))?
        {
            let entry =
                entry.map_err(|error| Error::io("Read release artifact", &absolute, error))?;
            count += 1;
            if count > 200000 {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Release package is too large to inspect",
                ));
            }
            let kind = entry
                .file_type()
                .map_err(|error| Error::io("Inspect release artifact", &entry.path(), error))?;
            let child = path.join(entry.file_name());
            root.resolve(&child)?;
            if entry.file_name() == name && kind.is_dir() == directory {
                matching.push(child.clone());
            } else if kind.is_dir() {
                pending.push(child);
            }
        }
    }
    if matching.len() != 1 {
        return Err(Error::new(
            ErrorKind::CorruptData,
            "Release package does not contain one unambiguous expected artifact",
        ));
    }
    Ok(matching.remove(0))
}
