use crate::{
    Error, ErrorKind, Result,
    domain::{ApplicationIdentity, Profile, same_windows_name, windows_component},
    profiles::environment::{self, Environment},
    storage::DataRoot,
    system::process::CommandRequest,
    tools::catalog::Tool,
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};

pub const RECORD: &str = ".runtime.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Launch {
    Executable {
        path: PathBuf,
    },
    Node {
        script: PathBuf,
        #[serde(default)]
        node: Option<PathBuf>,
    },
    Batch {
        path: PathBuf,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRecord {
    pub schema: u32,
    pub tool_id: String,
    pub version: String,
    pub launch: Launch,
    #[serde(default)]
    pub generation: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Programs {
    pub node: Option<PathBuf>,
    pub npm_cli: Option<PathBuf>,
    pub powershell: Option<PathBuf>,
}
impl Programs {
    pub fn discover(host: &Environment) -> Self {
        let get = |name: &str| {
            host.iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        let mut paths = Vec::new();
        if let Some(path) = get("PATH") {
            paths.extend(std::env::split_paths(path));
        }
        for variable in ["PROGRAMFILES", "PROGRAMFILES(X86)"] {
            if let Some(base) = get(variable) {
                paths.push(PathBuf::from(base).join("nodejs"));
            }
        }
        let node = paths
            .iter()
            .map(|path| path.join("node.exe"))
            .find(|path| path.is_absolute() && path.is_file());
        let npm_cli = node
            .as_ref()
            .and_then(|node| node.parent())
            .map(|path| path.join("node_modules/npm/bin/npm-cli.js"))
            .filter(|path| path.is_file());
        let powershell = get("SYSTEMROOT")
            .map(|path| PathBuf::from(path).join("System32/WindowsPowerShell/v1.0/powershell.exe"))
            .filter(|path| path.is_absolute() && path.is_file());
        Self {
            node,
            npm_cli,
            powershell,
        }
    }
}

pub struct Resolved {
    pub program: PathBuf,
    pub prefix: Vec<String>,
    pub version: Option<String>,
    pub entry: PathBuf,
}
impl Resolved {
    pub fn request(
        &self,
        cwd: PathBuf,
        environment: Environment,
        arguments: &[String],
        timeout: Duration,
    ) -> CommandRequest {
        let mut all = self.prefix.clone();
        all.extend_from_slice(arguments);
        CommandRequest {
            program: self.program.clone(),
            arguments: all,
            cwd,
            environment,
            timeout,
            capture_stdout: Some(16384),
        }
    }
}

pub fn resolve(root: &DataRoot, tool: &Tool, programs: &Programs) -> Result<Option<Resolved>> {
    let base = PathBuf::from(&tool.dir);
    if let Some(bytes) = root.read_limited(base.join(RECORD), 1024 * 1024)? {
        let record: RuntimeRecord = serde_json::from_slice(&bytes).map_err(Error::corrupt)?;
        if record.schema != 1 || record.tool_id != tool.id {
            return Err(Error::new(
                ErrorKind::CorruptData,
                "Runtime record identity does not match its CLI",
            ));
        }
        super::release::version_name(&record.version)?;
        return resolve_launch(root, &base, &record.launch, programs, Some(record.version));
    }
    if tool.installer.kind == "npm" {
        return npm_command(root, tool, &base, programs);
    }
    for candidate in tool.executable_candidates(root.path()) {
        let relative = candidate
            .strip_prefix(root.path())
            .map_err(Error::corrupt)?;
        let checked = root.resolve(relative)?;
        if checked.is_file() {
            return Ok(Some(Resolved {
                entry: checked.clone(),
                program: checked,
                prefix: Vec::new(),
                version: None,
            }));
        }
    }
    Ok(None)
}

pub fn resolve_launch(
    root: &DataRoot,
    base: &Path,
    launch: &Launch,
    programs: &Programs,
    version: Option<String>,
) -> Result<Option<Resolved>> {
    let (program, prefix, entry) = match launch {
        Launch::Executable { path } | Launch::Batch { path } => {
            runtime_path(path)?;
            let path = root.resolve(base.join(path))?;
            (path.clone(), Vec::new(), path)
        }
        Launch::Node { script, node } => {
            runtime_path(script)?;
            let entry = root.resolve(base.join(script))?;
            let program = match node {
                Some(node) => {
                    runtime_path(node)?;
                    root.resolve(base.join(node))?
                }
                None => programs.node.clone().ok_or_else(|| {
                    Error::new(ErrorKind::NotFound, "Node.js is required for this CLI")
                })?,
            };
            (
                program,
                vec![
                    entry
                        .to_str()
                        .ok_or_else(|| {
                            Error::new(ErrorKind::InvalidInput, "CLI entry must be Unicode")
                        })?
                        .to_owned(),
                ],
                entry,
            )
        }
    };
    if !entry.is_file() {
        return Ok(None);
    }
    if !program.is_absolute() || !program.is_file() {
        return Err(Error::new(
            ErrorKind::NotFound,
            "CLI runtime interpreter is unavailable",
        ));
    }
    Ok(Some(Resolved {
        program,
        prefix,
        version,
        entry,
    }))
}

pub fn npm_command(
    root: &DataRoot,
    tool: &Tool,
    base: &Path,
    programs: &Programs,
) -> Result<Option<Resolved>> {
    let package = tool
        .installer
        .package
        .as_deref()
        .ok_or_else(|| Error::new(ErrorKind::CorruptData, "npm adapter has no package"))?;
    package_name(package)?;
    let package_root = base.join("node_modules").join(package);
    let Some(bytes) = root.read_limited(package_root.join("package.json"), 1024 * 1024)? else {
        return Ok(None);
    };
    let metadata: serde_json::Value = serde_json::from_slice(&bytes).map_err(Error::corrupt)?;
    if metadata["name"].as_str() != Some(package) {
        return Err(Error::new(
            ErrorKind::CorruptData,
            "Installed npm package identity does not match",
        ));
    }
    let version = metadata["version"]
        .as_str()
        .ok_or_else(|| Error::new(ErrorKind::CorruptData, "npm package has no version"))?
        .to_owned();
    super::release::version_name(&version)?;
    let bin = match &metadata["bin"] {
        serde_json::Value::String(bin) => bin.as_str(),
        serde_json::Value::Object(bins) => bins
            .get(&tool.bin)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::CorruptData,
                    "npm package has no matching CLI bin",
                )
            })?,
        _ => {
            return Err(Error::new(
                ErrorKind::CorruptData,
                "npm package has no CLI entry",
            ));
        }
    };
    let entry = PathBuf::from(bin);
    runtime_path(&entry)?;
    let relative = package_root
        .strip_prefix(base)
        .map_err(Error::corrupt)?
        .join(entry);
    let launch = if relative
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("exe"))
    {
        Launch::Executable { path: relative }
    } else {
        Launch::Node {
            script: relative,
            node: None,
        }
    };
    resolve_launch(root, base, &launch, programs, Some(version))
}

pub fn profile_request(
    root: &DataRoot,
    tool: &Tool,
    profile: &Profile,
    identity: &ApplicationIdentity,
    host: &Environment,
    programs: &Programs,
    arguments: &[String],
) -> Result<Option<CommandRequest>> {
    let Some(command) = resolve(root, tool, programs)? else {
        return Ok(None);
    };
    let env = environment::build(root, tool, profile, identity, host)?;
    Ok(Some(command.request(
        env.cwd,
        env.values,
        arguments,
        Duration::from_secs(15),
    )))
}

pub fn runtime_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Runtime path must be relative",
        ));
    }
    for component in path.components() {
        if let Component::Normal(value) = component {
            windows_component(
                value.to_str().ok_or_else(|| {
                    Error::new(ErrorKind::InvalidInput, "Runtime paths must be Unicode")
                })?,
                255,
            )?;
        }
    }
    let first = path
        .components()
        .find(|component| matches!(component, Component::Normal(_)))
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Empty runtime path"))?;
    if [
        "Profiles",
        "_shared",
        "_profiles",
        "workspace",
        "logs",
        "Temp",
    ]
    .iter()
    .any(|reserved| same_windows_name(&first.as_os_str().to_string_lossy(), reserved))
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Runtime command cannot select private/control data",
        ));
    }
    Ok(())
}

pub fn package_name(value: &str) -> Result<()> {
    let valid = if let Some(scoped) = value.strip_prefix('@') {
        let mut segments = scoped.split('/');
        segments.next().is_some_and(segment)
            && segments.next().is_some_and(segment)
            && segments.next().is_none()
    } else {
        segment(value)
    };
    if !valid {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Invalid npm package name",
        ));
    }
    Ok(())
}
fn segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 214
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        && value != "."
        && value != ".."
}
