use super::{
    download::Http,
    release::{Release, version_name},
};
use crate::{
    Error, ErrorKind, Result,
    profiles::ProfileRepository,
    profiles::activity::ActivityKind,
    profiles::environment,
    system::process::CancellationToken,
    tools::catalog::Tool,
    tools::install::{command, service::Installer, version},
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Deserialize)]
struct NpmRelease {
    version: String,
}

pub async fn resolve_release(
    http: &Http,
    tool: &Tool,
    cancel: &CancellationToken,
) -> Result<Release> {
    match tool.installer.kind.as_str() {
        "npm" => {
            let package = tool
                .installer
                .package
                .as_deref()
                .ok_or_else(|| Error::new(ErrorKind::CorruptData, "npm package is missing"))?;
            command::package_name(package)?;
            let mut url = super::download::release_url("https://registry.npmjs.org/")?;
            url.path_segments_mut()
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "Invalid npm registry URL"))?
                .push(package)
                .push("latest");
            let version = http.json::<NpmRelease>(url.as_str(), cancel).await?.version;
            version_name(&version)?;
            Ok(Release {
                version,
                url: url.to_string(),
                checksum: None,
                size: None,
            })
        }
        "agy-release" => {
            http.agy_release(
                tool.installer
                    .manifest
                    .as_deref()
                    .ok_or_else(|| Error::new(ErrorKind::CorruptData, "AGY manifest is missing"))?,
                cancel,
            )
            .await
        }
        "github-release" => {
            let repo = tool.installer.repo.as_deref().ok_or_else(|| {
                Error::new(ErrorKind::CorruptData, "GitHub repository is missing")
            })?;
            if repo.split('/').count() != 2
                || repo.split('/').any(|part| {
                    part.is_empty()
                        || !part
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                        || matches!(part, "." | "..")
                })
            {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Invalid GitHub release repository",
                ));
            }
            let asset = tool.installer.asset.as_deref().ok_or_else(|| {
                Error::new(ErrorKind::CorruptData, "GitHub release asset is missing")
            })?;
            http.github_release(
                &format!("https://api.github.com/repos/{repo}/releases/latest"),
                asset,
                cancel,
            )
            .await
        }
        "cursor-release" => {
            http.cursor_release(
                tool.installer.url.as_deref().ok_or_else(|| {
                    Error::new(ErrorKind::CorruptData, "Cursor bootstrap URL is missing")
                })?,
                if cfg!(target_arch = "aarch64") {
                    super::release::Architecture::Arm64
                } else {
                    super::release::Architecture::X64
                },
                cancel,
            )
            .await
        }
        "powershell-script" => http.aider_release(cancel).await,
        _ => Err(Error::new(
            ErrorKind::Unsupported,
            "CLI has no release resolver",
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checked {
    pub checked_at: i64,
    pub version: Option<String>,
    pub error: bool,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Index {
    schema: u32,
    tools: BTreeMap<String, Checked>,
}

#[derive(Deserialize)]
struct LegacyChecked {
    #[serde(rename = "checkedAt")]
    checked_at: i64,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

struct Cached {
    value: Release,
    until: Instant,
    revision: u64,
}

pub struct Updater {
    repository: Arc<ProfileRepository>,
    installer: Arc<Installer>,
    http: Http,
    releases: Mutex<BTreeMap<String, Cached>>,
    checks: Mutex<Index>,
    errors: Mutex<VecDeque<(String, Error)>>,
    revision: AtomicU64,
    shutdown: CancellationToken,
}

impl Updater {
    pub fn open(
        repository: Arc<ProfileRepository>,
        installer: Arc<Installer>,
        http: Http,
    ) -> Result<Self> {
        let root = repository.root();
        let checks = match root.read_limited("_updates/checks.json", 1024 * 1024)? {
            Some(bytes) => {
                let value: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(Error::corrupt)?;
                let index: Index = if value.get("schema").is_some() {
                    serde_json::from_value(value).map_err(Error::corrupt)?
                } else {
                    let previous: BTreeMap<String, LegacyChecked> =
                        serde_json::from_value(value).map_err(Error::corrupt)?;
                    Index {
                        schema: 1,
                        tools: previous
                            .into_iter()
                            .map(|(key, value)| {
                                (
                                    key,
                                    Checked {
                                        checked_at: value.checked_at,
                                        version: value.version,
                                        error: value.error.is_some(),
                                    },
                                )
                            })
                            .collect(),
                    }
                };
                if index.schema != 1 {
                    return Err(Error::new(
                        ErrorKind::Unsupported,
                        "Update-check index schema is unsupported",
                    ));
                }
                index
            }
            None => Index {
                schema: 1,
                tools: BTreeMap::new(),
            },
        };
        Ok(Self {
            repository,
            installer,
            http,
            releases: Mutex::new(BTreeMap::new()),
            checks: Mutex::new(checks),
            errors: Mutex::new(VecDeque::new()),
            revision: AtomicU64::new(0),
            shutdown: CancellationToken::new(),
        })
    }
    pub fn stop(&self) {
        self.shutdown.cancel();
    }
    pub fn checked(&self, tool_id: &str) -> Option<Checked> {
        self.checks.lock().tools.get(tool_id).cloned()
    }
    pub fn take_errors(&self) -> Vec<(String, Error)> {
        self.errors.lock().drain(..).collect()
    }

    pub async fn latest(
        &self,
        tool: &Tool,
        force: bool,
        cancel: &CancellationToken,
    ) -> Result<Release> {
        if !force {
            if let Some(entry) = self.releases.lock().get(&tool.id) {
                if entry.until > Instant::now() {
                    return Ok(entry.value.clone());
                }
            }
        }
        let request = self.revision.fetch_add(1, Ordering::AcqRel) + 1;
        let release = resolve_release(&self.http, tool, cancel).await?;
        let mut cached = self.releases.lock();
        if let Some(current) = cached.get(&tool.id) {
            if current.revision > request {
                return Ok(current.value.clone());
            }
        }
        cached.insert(
            tool.id.clone(),
            Cached {
                value: release.clone(),
                until: Instant::now() + Duration::from_secs(900),
                revision: request,
            },
        );
        Ok(release)
    }

    pub async fn check_one(
        &self,
        tool_id: &str,
        force: bool,
        cancel: &CancellationToken,
    ) -> Result<Outcome> {
        let tool = self.repository.catalog().get(tool_id)?;
        if self.shutdown.is_cancelled() || cancel.is_cancelled() {
            return Err(Error::new(ErrorKind::Cancelled, "Updater is stopping"));
        }
        if self.repository.activities().is_tool_busy(tool_id) {
            return Ok(Outcome::Busy);
        }
        let profiles = self.repository.list(tool_id)?;
        let profile = profiles
            .iter()
            .find(|profile| profile.id.is_default())
            .or_else(|| profiles.first())
            .cloned();
        let Some(profile) = profile else {
            return Ok(Outcome::NotInstalled);
        };
        let inspection = self.repository.activities().reserve(
            tool,
            Some(&profile.id),
            &profile.settings,
            ActivityKind::Inspect,
        )?;
        let programs = self.installer.programs();
        let Some(command) = command::resolve(self.repository.root(), tool, &programs)? else {
            return Ok(Outcome::NotInstalled);
        };
        let due = {
            let checks = self.checks.lock();
            checks.tools.get(tool_id).is_none_or(|entry| {
                let now = chrono::Utc::now().timestamp_millis();
                force
                    || entry.checked_at > now
                    || now.saturating_sub(entry.checked_at)
                        >= if entry.error { 900000 } else { 14400000 }
            })
        };
        if !due {
            return Ok(Outcome::Cached);
        }
        let env = environment::build(
            self.repository.root(),
            tool,
            &profile,
            self.installer.identity(),
            self.installer.host(),
        )?;
        let request = command.request(
            env.cwd,
            env.values,
            &["--version".into()],
            Duration::from_secs(15),
        );
        let cancellation = cancel.clone();
        let installed = tokio::task::spawn_blocking(move || version::read(request, &cancellation))
            .await
            .map_err(|error| Error::new(ErrorKind::Process, error.to_string()))??;
        let release = self.latest(tool, force, cancel).await?;
        if self.shutdown.is_cancelled() || cancel.is_cancelled() {
            return Err(Error::new(ErrorKind::Cancelled, "Updater is stopping"));
        }
        drop(inspection);
        if self.repository.activities().is_tool_busy(tool_id) {
            return Ok(Outcome::Busy);
        }
        let outcome = if version::newer(&release.version, &installed) {
            let result = match tool.installer.kind.as_str() {
                "npm" => {
                    self.installer
                        .install_npm(tool_id, &profile.id, Some(&release.version), cancel)
                        .await
                }
                _ => {
                    self.installer
                        .install_native(tool_id, &profile.id, Some(release), cancel)
                        .await
                }
            };
            match result {
                Ok(value) => Outcome::Updated(value.version),
                Err(error) if error.kind == ErrorKind::Busy => Outcome::Busy,
                Err(error) => {
                    self.save(tool_id, None, true)?;
                    return Err(error);
                }
            }
        } else {
            Outcome::Current(installed)
        };
        if !matches!(outcome, Outcome::Busy) {
            self.save(
                tool_id,
                Some(match &outcome {
                    Outcome::Updated(value) | Outcome::Current(value) => value.clone(),
                    _ => String::new(),
                }),
                false,
            )?;
        }
        Ok(outcome)
    }

    fn save(&self, tool_id: &str, version: Option<String>, error: bool) -> Result<()> {
        let mut checks = self.checks.lock();
        let next = Checked {
            checked_at: chrono::Utc::now().timestamp_millis(),
            version,
            error,
        };
        let mut updated = checks.clone();
        updated.tools.insert(tool_id.into(), next);
        self.repository
            .root()
            .write_json("_updates/checks.json", &updated)?;
        *checks = updated;
        Ok(())
    }

    pub fn start(self: Arc<Self>) -> Result<tokio::task::JoinHandle<()>> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            Error::new(
                ErrorKind::InvalidInput,
                "Updater requires an asynchronous service runtime",
            )
        })?;
        Ok(runtime.spawn(async move {
            tokio::select! {_ = self.shutdown.cancelled()=>return,_=tokio::time::sleep(Duration::from_secs(20))=>()};
            while !self.shutdown.is_cancelled() {
                for tool in self.repository.catalog().tools() {
                    if self.shutdown.is_cancelled() {
                        break;
                    }
                    if let Err(error)=self.check_one(&tool.id, false, &self.shutdown).await {
                        if error.kind==ErrorKind::Cancelled && self.shutdown.is_cancelled(){break;}
                        let _saved=self.save(&tool.id,None,true);
                        let mut errors=self.errors.lock();errors.push_back((tool.id.clone(),error));
                        while errors.len()>32{errors.pop_front();}
                    }
                }
                tokio::select! {_ = self.shutdown.cancelled()=>break,_=tokio::time::sleep(Duration::from_secs(60))=>()};
            }
        }))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Busy,
    NotInstalled,
    Cached,
    Current(String),
    Updated(String),
}
