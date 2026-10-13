use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{
    Error, ErrorKind, Result,
    domain::{ApplicationIdentity, Profile},
    profiles::prepare_profile,
    storage::DataRoot,
    tools::catalog::Tool,
};

pub type Environment = BTreeMap<String, String>;

pub struct ProfileEnvironment {
    pub cwd: PathBuf,
    pub values: Environment,
}

const PASSTHROUGH: &[&str] = &[
    "BASALT_AGENT_STATE_FILE",
    "ALL_PROXY",
    "COMMONPROGRAMFILES",
    "COMMONPROGRAMFILES(X86)",
    "COMMONPROGRAMW6432",
    "COMSPEC",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "LANG",
    "LC_ALL",
    "NODE_EXTRA_CA_CERTS",
    "NO_PROXY",
    "NUMBER_OF_PROCESSORS",
    "OS",
    "PATH",
    "PATHEXT",
    "PROCESSOR_ARCHITECTURE",
    "PROCESSOR_IDENTIFIER",
    "PROCESSOR_LEVEL",
    "PROCESSOR_REVISION",
    "PROCESSOR_ARCHITEW6432",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "PSMODULEPATH",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
    "SYSTEMDRIVE",
    "SYSTEMROOT",
    "USERNAME",
    "WINDIR",
];

pub fn build(
    root: &DataRoot,
    tool: &Tool,
    profile: &Profile,
    identity: &ApplicationIdentity,
    host: &Environment,
) -> Result<ProfileEnvironment> {
    identity.validate()?;
    let relative = tool.profile_relative(profile);
    prepare_profile(root, &relative)?;
    let home = root.resolve(&relative)?;
    let runtime = tool.directory(root.path());
    let config = home.join(".config");
    let data = home.join(".local/share");
    let cache = home.join(".cache");
    let cwd = home.join("workspace");
    let mut values = safe_host(host);
    set_home(&mut values, &home);
    for (key, path) in [
        ("XDG_CONFIG_HOME", config.clone()),
        ("XDG_DATA_HOME", data.clone()),
        ("XDG_STATE_HOME", home.join(".local/state")),
        ("XDG_CACHE_HOME", cache.clone()),
        ("PWD", cwd.clone()),
        ("INIT_CWD", cwd.clone()),
        ("GIT_CEILING_DIRECTORIES", cwd.clone()),
        ("APP_PROFILE_WORKSPACE", cwd.clone()),
    ] {
        values.insert(key.into(), path_text(&path)?);
    }
    for (key, value) in [
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
        ("FORCE_COLOR", "3"),
        ("CI", "false"),
        ("npm_config_update_notifier", "false"),
        ("TERM_PROGRAM", identity.display_name.as_str()),
        ("TERM_PROGRAM_VERSION", identity.version.as_str()),
    ] {
        values.insert(key.into(), value.into());
    }
    values
        .entry("PROCESSOR_ARCHITECTURE".into())
        .or_insert_with(|| {
            if cfg!(target_arch = "aarch64") {
                "ARM64"
            } else {
                "AMD64"
            }
            .into()
        });
    let paths = tool_variables(&tool.id, &home)
        .ok_or_else(|| Error::new(ErrorKind::Unsupported, "Tool has no environment adapter"))?;
    for (key, path) in paths {
        let relative = path
            .strip_prefix(root.path())
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "Environment path left its root"))?;
        root.create_dir(relative)?;
        values.insert(key.into(), path_text(&path)?);
    }
    if tool.id == "opencode" {
        values.insert("OPENCODE_DISABLE_AUTO_UPDATE".into(), "true".into());
    }
    if tool.id == "aider" {
        values.insert("AIDER_GIT".into(), "false".into());
        values.insert("AIDER_GITIGNORE".into(), "false".into());
        for (key, name) in [
            ("AIDER_INPUT_HISTORY_FILE", "input.history"),
            ("AIDER_CHAT_HISTORY_FILE", "chat.history.md"),
            ("AIDER_LLM_HISTORY_FILE", "llm.history"),
        ] {
            values.insert(key.into(), path_text(&home.join(".aider").join(name))?);
        }
    }
    values.insert("npm_config_prefix".into(), path_text(&runtime)?);
    values.insert(
        "npm_config_cache".into(),
        path_text(&runtime.join(".cache/npm"))?,
    );
    let mut paths = vec![
        runtime.join("node_modules/.bin"),
        runtime.join("bin"),
        runtime.clone(),
    ];
    if let Some(host_path) = values.remove("PATH") {
        paths.extend(std::env::split_paths(&host_path));
    }
    for key in ["PROGRAMFILES", "PROGRAMFILES(X86)"] {
        if let Some(base) = values.get(key) {
            let node = PathBuf::from(base).join("nodejs");
            if node.is_dir() && !paths.contains(&node) {
                paths.push(node);
            }
        }
    }
    let path = std::env::join_paths(paths).map_err(Error::corrupt)?;
    values.insert(
        "PATH".into(),
        path.into_string()
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "PATH must be Unicode"))?,
    );
    Ok(ProfileEnvironment { cwd, values })
}

pub fn install(
    root: &DataRoot,
    tool: &Tool,
    profile: &Profile,
    identity: &ApplicationIdentity,
    host: &Environment,
) -> Result<ProfileEnvironment> {
    let mut environment = build(root, tool, profile, identity, host)?;
    let relative = PathBuf::from(&tool.dir);
    for directory in [
        "",
        ".config",
        ".local/share",
        ".local/state",
        ".cache",
        "AppData/Roaming",
        "AppData/Local",
        "Temp",
    ] {
        root.create_dir(relative.join(directory))?;
    }
    let runtime = root.resolve(&relative)?;
    set_home(&mut environment.values, &runtime);
    environment.cwd = runtime.clone();
    for key in ["PWD", "INIT_CWD", "GIT_CEILING_DIRECTORIES"] {
        environment.values.insert(key.into(), path_text(&runtime)?);
    }
    environment.values.insert(
        "npm_config_cache".into(),
        path_text(&runtime.join(".cache/npm"))?,
    );
    if tool.id == "aider" {
        for (key, relative) in [
            ("UV_INSTALL_DIR", "bin"),
            ("UV_TOOL_DIR", "uv/tools"),
            ("UV_TOOL_BIN_DIR", "bin"),
            ("UV_PYTHON_INSTALL_DIR", "uv/python"),
            ("UV_CACHE_DIR", ".cache/uv"),
        ] {
            environment
                .values
                .insert(key.into(), path_text(&runtime.join(relative))?);
        }
        environment
            .values
            .insert("UV_NO_MODIFY_PATH".into(), "1".into());
    }
    Ok(environment)
}

fn safe_host(host: &Environment) -> Environment {
    let mut values = Environment::new();
    for (key, value) in host {
        let key = key.to_uppercase();
        if !PASSTHROUGH.contains(&key.as_str()) {
            continue;
        }
        if key == "PATH" {
            values
                .entry(key)
                .and_modify(|existing| {
                    if existing != value {
                        existing.push(';');
                        existing.push_str(value);
                    }
                })
                .or_insert_with(|| value.clone());
        } else {
            values.entry(key).or_insert_with(|| value.clone());
        }
    }
    values
}

fn set_home(values: &mut Environment, home: &Path) {
    let home = home.to_string_lossy().into_owned();
    let drive = home
        .get(..2)
        .filter(|drive| drive.ends_with(':'))
        .unwrap_or("");
    values.insert("HOME".into(), home.clone());
    values.insert("USERPROFILE".into(), home.clone());
    values.insert("HOMEDRIVE".into(), drive.into());
    values.insert("HOMEPATH".into(), home[drive.len()..].into());
    for (key, relative) in [
        ("APPDATA", "AppData/Roaming"),
        ("LOCALAPPDATA", "AppData/Local"),
        ("TEMP", "Temp"),
        ("TMP", "Temp"),
    ] {
        values.insert(
            key.into(),
            PathBuf::from(&home).join(relative).display().to_string(),
        );
    }
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(String::from)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Runtime paths must be Unicode"))
}

/// Where each CLI keeps its configuration, under `home`: the variables that point it
/// there. None for a tool without an adapter.
pub fn tool_variables(tool: &str, home: &Path) -> Option<Vec<(&'static str, PathBuf)>> {
    let config = home.join(".config");
    let data = home.join(".local/share");
    let cache = home.join(".cache");
    Some(match tool {
        "claude" => vec![("CLAUDE_CONFIG_DIR", home.join(".claude"))],
        "codex" => vec![("CODEX_HOME", home.join(".codex"))],
        "opencode" => vec![
            ("OPENCODE_CONFIG_DIR", config.join("opencode")),
            ("OPENCODE_DATA_DIR", data.join("opencode")),
            ("OPENCODE_CACHE_DIR", cache.join("opencode")),
        ],
        "agy" => vec![
            ("AGY_HOME", home.join(".agy")),
            ("ANTIGRAVITY_HOME", home.join(".agy")),
            (
                "ANTIGRAVITY_APP_DATA_DIR",
                home.join(".gemini/antigravity-cli"),
            ),
            ("JETSKI_APP_DATA_DIR", home.join(".gemini/antigravity-cli")),
            ("GEMINI_CLI_HOME", home.join(".gemini")),
        ],
        "aider" => vec![
            ("AIDER_CONFIG_DIR", home.join(".aider")),
            ("AIDER_HOME", home.join(".aider")),
        ],
        "copilot" => vec![("COPILOT_HOME", home.join(".copilot"))],
        "cursor-agent" => vec![("CURSOR_CONFIG_DIR", home.join(".cursor"))],
        "amp" => vec![("AMP_CONFIG_DIR", home.join(".amp"))],
        "goose" => vec![
            ("GOOSE_HOME", home.join(".goose")),
            ("GOOSE_CONFIG_DIR", home.join(".goose")),
        ],
        "crush" => vec![
            ("CRUSH_GLOBAL_CONFIG", config.join("crush")),
            ("CRUSH_GLOBAL_DATA", data.join("crush")),
        ],
        "qwen" => vec![("QWEN_CONFIG_DIR", home.join(".qwen"))],
        "kimi" => vec![("KIMI_CODE_HOME", home.join(".kimi-code"))],
        "gemini" => vec![("GEMINI_CLI_HOME", home.join(".gemini"))],
        _ => return None,
    })
}

/// Folder of a session profile: one sub-folder per CLI.
pub fn session_folder(profile: &str) -> PathBuf {
    PathBuf::from("Sessions").join(profile)
}

/// Variables for a plain session terminal: every known CLI is pointed at its own folder
/// inside the session profile, through that CLI's own variable, and the terminal gets
/// the profile's own home (`home`, with its AppData and Temp), so any other program
/// keeps its settings and sign-ins there too instead of in the user's own.
/// `profile` names the profile folder and must be a single plain path segment.
pub fn session(root: &DataRoot, tools: &[&str], profile: &str) -> Result<Environment> {
    let plain = !profile.is_empty()
        && profile.len() <= 64
        && profile
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_');
    if !plain {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Invalid session profile",
        ));
    }
    let base = root.create_dir(session_folder(profile))?;
    let mut values = Environment::new();
    for tool in tools {
        let Some(paths) = tool_variables(tool, &base.join(tool)) else {
            continue;
        };
        for (key, path) in paths {
            let relative = path.strip_prefix(root.path()).map_err(|_| {
                Error::new(ErrorKind::InvalidInput, "Environment path left its root")
            })?;
            root.create_dir(relative)?;
            values.insert(key.into(), path_text(&path)?);
        }
    }
    let home = session_folder(profile).join("home");
    root.create_dir(&home)?;
    for folder in ["AppData/Roaming", "AppData/Local", "Temp"] {
        root.create_dir(home.join(folder))?;
    }
    set_home(&mut values, &root.path().join(&home));
    Ok(values)
}
