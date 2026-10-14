//! Catalog-backed choices and profile-owned sessions; no legacy profile imports.
use crate::{agent_bridge, screen};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use basalt_core::{
    tools::catalog::{Catalog, Tool},
    domain::{ApplicationIdentity, ProfileId},
    tools::install::command::{self, Programs},
    terminal::pty::{ExitSummary, NativeTerminal},
    service::BackendService,
    terminal::session::{Session, SessionStartRequest},
    storage::DataRoot,
    terminal::{TerminalSize, TerminalView},
};
use basalt_core::system::process::CancellationToken;

#[derive(Clone)]
pub enum Terminal {
    Shell(Arc<NativeTerminal>),
    Managed {
        session: Arc<Session>,
        view: TerminalView,
        pid: u32,
        cwd: PathBuf,
    },
}

impl Terminal {
    pub fn managed(session: Arc<Session>, root: &DataRoot) -> basalt_core::Result<Self> {
        let view = session.view()?;
        let pid = session.summary().pid.unwrap_or(0);
        let cwd = root.resolve(
            session
                .tool
                .profile_relative(&session.profile)
                .join("workspace"),
        )?;
        Ok(Self::Managed {
            session,
            view,
            pid,
            cwd,
        })
    }
    pub fn model(&self) -> TerminalView {
        match self {
            Self::Shell(t) => t.model(),
            Self::Managed { view, .. } => view.clone(),
        }
    }
    pub fn pid(&self) -> u32 {
        match self {
            Self::Shell(t) => t.pid(),
            Self::Managed { pid, .. } => *pid,
        }
    }
    pub fn input(&self, data: Vec<u8>) -> basalt_core::Result<()> {
        match self {
            Self::Shell(t) => t.input(data),
            Self::Managed { session, .. } => session.input(data),
        }
    }
    pub fn resize(&self, size: TerminalSize) -> basalt_core::Result<()> {
        match self {
            Self::Shell(t) => t.resize(size),
            Self::Managed { session, .. } => session.resize(size),
        }
    }
    pub fn scroll(&self, lines: i32) {
        match self {
            Self::Shell(t) => t.scroll(lines),
            Self::Managed { session, .. } => session.scroll(lines),
        }
    }
    pub fn title(&self) -> String {
        match self {
            Self::Shell(t) => t.title(),
            Self::Managed { session, .. } => session.title(),
        }
    }
    pub fn title_update_since(&self, v: u64) -> Option<(u64, String)> {
        match self {
            Self::Shell(t) => t.title_update_since(v),
            Self::Managed { session, .. } => session.title_update_since(v),
        }
    }
    pub fn exit_summary(&self) -> Option<ExitSummary> {
        match self {
            Self::Shell(t) => t.exit_summary(),
            Self::Managed { session, .. } => session.exit_summary(),
        }
    }
    pub fn stop(&self, grace: Duration) -> basalt_core::Result<ExitSummary> {
        match self {
            Self::Shell(t) => t.stop(grace),
            Self::Managed { session, .. } => session.stop(grace),
        }
    }
}
pub struct Engine {
    root: DataRoot,
    identity: ApplicationIdentity,
    catalog: Catalog,
    service: Mutex<Option<Arc<BackendService>>>,
    /// Held while a CLI is installed, repaired or updated: one at a time.
    installing: Mutex<()>,
}

/// Self-updates are off for the CLIs Basalt installs: an update that replaces a
/// running program inside Basalt's folder can leave it half installed. Basalt updates
/// them itself (`keep_updated`).
const NO_SELF_UPDATE: &[(&str, &str, &str)] = &[
    ("claude", "DISABLE_AUTOUPDATER", "1"),
    ("copilot", "COPILOT_AUTO_UPDATE", "false"),
    ("opencode", "OPENCODE_DISABLE_AUTOUPDATE", "true"),
];
impl Engine {
    pub fn new(root: DataRoot, identity: ApplicationIdentity) -> Self {
        Self {
            root,
            identity,
            catalog: Catalog::built_in(),
            service: Mutex::new(None),
            installing: Mutex::new(()),
        }
    }
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }
    /// Variables that point every known CLI at the folders of session profile
    /// `profile` (its id), for a plain terminal; HOME stays the user's.
    pub fn identity(&self) -> &ApplicationIdentity {
        &self.identity
    }

    /// Command shims every session terminal finds first on PATH: installed CLIs live in
    /// their own folders, and these name them.
    /// The data folder this engine owns.
    pub fn data_path(&self) -> &std::path::Path {
        self.root.path()
    }
    pub fn shim_folder(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    /// `true_color` tells the CLI it may draw in true color: agent CLIs that cannot
    /// detect it on Windows fall back to 256 colors and lose their own palette.
    fn write_shim(&self, name: &str, program: &std::path::Path, prefix: &[String], true_color: bool) -> Result<(), String> {
        if !crate::packages::plain(name) {
            return Err(format!("Invalid command name {name}"));
        }
        let folder = self.shim_folder();
        std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
        let mut line = format!("@\"{}\"", program.display());
        for argument in prefix {
            line.push_str(&format!(" \"{argument}\""));
        }
        let colors = if true_color { "@set \"FORCE_COLOR=3\"\r\n" } else { "" };
        std::fs::write(folder.join(format!("{name}.cmd")), format!("{colors}{line} %*\r\n")).map_err(|error| error.to_string())
    }

    /// Keep the catalog CLIs Basalt installed up to date, in place of their own
    /// self-updates: each is checked every few hours and updated only while no terminal
    /// runs it (`running` holds the ids of the CLIs running now). Never returns.
    pub fn keep_updated(&self, running: &Mutex<std::collections::HashSet<String>>) {
        use basalt_core::tools::install::{
            download::Http,
            service::Installer,
            updates::{Outcome, Updater},
        };
        std::thread::sleep(Duration::from_secs(60));
        let updater = (|| -> Result<_, String> {
            let service = self.service()?;
            let host = self.host();
            let http = || Http::new(&self.identity, &[], Duration::from_secs(600)).map_err(|error| error.to_string());
            let installer = Installer::new(service.profiles(), self.identity.clone(), host.clone(), Programs::discover(&host), http()?)
                .map_err(|error| error.to_string())?;
            Updater::open(service.profiles(), Arc::new(installer), http()?).map_err(|error| error.to_string())
        })();
        let (Ok(updater), Ok(runtime)) = (
            updater,
            tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build(),
        ) else {
            return;
        };
        let cancel = CancellationToken::new();
        loop {
            for tool in self.catalog.tools() {
                if running.lock().map_or(true, |running| running.contains(&tool.id)) {
                    continue;
                }
                let Ok(_one) = self.installing.try_lock() else {
                    continue;
                };
                let outcome = runtime.block_on(updater.check_one(&tool.id, false, &cancel));
                drop(_one);
                if matches!(outcome, Ok(Outcome::Updated(_))) {
                    self.refresh_shims();
                }
            }
            std::thread::sleep(Duration::from_secs(600));
        }
    }

    /// Install a catalog CLI into its own folder and put its command on PATH.
    pub fn install_recipe(&self, tool_id: &str, cancel: &CancellationToken) -> Result<String, String> {
        let _one = self.installing.lock().map_err(|_| "Install lock failed")?;
        self.install_unlocked(tool_id, cancel)
    }

    fn install_unlocked(&self, tool_id: &str, cancel: &CancellationToken) -> Result<String, String> {
        use basalt_core::tools::install::{download::Http, service::Installer};
        if self.catalog.get(tool_id).is_ok_and(|tool| tool.installer.kind == "npm") {
            self.node()?;
        }
        let service = self.service()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let http = Http::new(&self.identity, &[], Duration::from_secs(600)).map_err(|error| error.to_string())?;
        let host = self.host();
        let installer = Arc::new(
            Installer::new(service.profiles(), self.identity.clone(), host.clone(), Programs::discover(&host), http)
                .map_err(|error| error.to_string())?,
        );
        let installed = runtime
            .block_on(async {
                let (job, _) = installer.start(tool_id, &ProfileId::default_profile(), None)?;
                tokio::select! {
                    result = job.wait() => result,
                    _ = cancel.cancelled() => { job.cancel(); job.wait().await }
                }
            })
            .map_err(|error| error.to_string())?;
        self.refresh_shims();
        Ok(installed.version)
    }

    /// Shims for catalog CLIs installed in their own folders. A CLI whose shim exists but
    /// whose program has gone (a failed update, a deleted file) gets a shim that has it
    /// reinstalled first; its ids are returned.
    pub fn refresh_shims(&self) -> Vec<String> {
        let programs = Programs::discover(&self.host());
        let claude_hooks = self.claude_hooks().ok();
        let mut broken = Vec::new();
        for tool in self.catalog.tools() {
            match command::resolve(&self.root, tool, &programs) {
                Ok(Some(resolved)) => {
                    let mut prefix = resolved.prefix.clone();
                    // Claude started from a terminal here reports what it does to the
                    // agents list, through hooks given for this run only, never the
                    // user's settings.
                    if let (true, Some(hooks)) = (tool.id == "claude", &claude_hooks) {
                        prefix.extend(["--settings".to_owned(), hooks.display().to_string()]);
                    }
                    let _ = self.write_catalog_shim(tool, Some((&resolved.program, &prefix)));
                }
                Ok(None) if self.shim_folder().join(format!("{}.cmd", tool.bin)).is_file() => {
                    let _ = self.write_catalog_shim(tool, None);
                    broken.push(tool.id.clone());
                }
                _ => {}
            }
        }
        broken
    }

    /// A catalog CLI's shim, with its self-update off. If its program is missing when it
    /// runs (or `program` is None), Basalt reinstalls the CLI first (`basalt.exe
    /// --repair`) and the shim then runs again, rewritten. The whole repair is one line:
    /// cmd reads a batch file as it goes, and this one is replaced meanwhile.
    fn write_catalog_shim(&self, tool: &Tool, program: Option<(&std::path::Path, &[String])>) -> Result<(), String> {
        if !crate::packages::plain(&tool.bin) {
            return Err(format!("Invalid command name {}", tool.bin));
        }
        let basalt = std::env::current_exe().map_err(|error| error.to_string())?;
        let repair = format!("\"{}\" --repair {} && cmd /d /c \"\"%~f0\" %*\" & exit /b", basalt.display(), tool.id);
        let mut text = String::new();
        match program {
            None => text.push_str(&format!("@{repair}
")),
            Some((program, prefix)) => {
                text.push_str(&format!("@if not exist \"{}\" {repair}
", program.display()));
                text.push_str("@set \"FORCE_COLOR=3\"
");
                for (_, key, value) in NO_SELF_UPDATE.iter().filter(|(id, _, _)| *id == tool.id) {
                    text.push_str(&format!("@set \"{key}={value}\"
"));
                }
                let mut line = format!("@\"{}\"", program.display());
                for argument in prefix {
                    line.push_str(&format!(" \"{argument}\""));
                }
                text.push_str(&format!("{line} %*
"));
            }
        }
        let folder = self.shim_folder();
        std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
        std::fs::write(folder.join(format!("{}.cmd", tool.bin)), text).map_err(|error| error.to_string())
    }

    /// Reinstall a catalog CLI whose program has gone, unless another repair already
    /// brought it back. Returns its version.
    pub fn repair(&self, tool_id: &str) -> Result<String, String> {
        let tool = self.catalog.get(tool_id).map_err(|error| error.to_string())?;
        let _one = self.installing.lock().map_err(|_| "Install lock failed")?;
        if let Ok(Some(resolved)) = command::resolve(&self.root, tool, &Programs::discover(&self.host())) {
            if resolved.entry.is_file() {
                self.refresh_shims();
                return Ok(resolved.version.unwrap_or_default());
            }
        }
        self.install_unlocked(tool_id, &CancellationToken::new())
    }

    /// A settings file holding only the hooks that report Claude's lifecycle to the
    /// terminal it runs in (through BASALT_AGENT_STATE_FILE).
    fn claude_hooks(&self) -> Result<PathBuf, String> {
        let program = self.hook_program()?;
        let hooks = agent_bridge::merge_hooks(serde_json::json!({}), &program, "claude")?;
        self.root.write_json("_frontend/claude-hooks.json", &hooks).map_err(|error| error.to_string())?;
        self.root.resolve("_frontend/claude-hooks.json").map_err(|error| error.to_string())
    }

    /// The program hooks run: this one, which notes the event and exits at once. The
    /// PowerShell script earlier versions used is removed.
    fn hook_program(&self) -> Result<PathBuf, String> {
        let _ = std::fs::remove_file(self.root.path().join("_frontend/agent-hook.ps1"));
        std::env::current_exe().map_err(|error| error.to_string())
    }

    /// The system's Node.js folder when it is not on the app's PATH yet: installed
    /// after the app started, it is still found where its installer puts it.
    fn runtime_folders(&self, path: &str) -> Vec<PathBuf> {
        let on_path: Vec<PathBuf> = std::env::split_paths(path).collect();
        let host = crate::shell::host_environment();
        Programs::discover(&host)
            .node
            .and_then(|node| node.parent().map(PathBuf::from))
            .filter(|folder| !on_path.contains(folder))
            .into_iter()
            .collect()
    }

    /// A PATH with the installed CLIs first, and Node.js last if it is missing.
    pub fn tool_path(&self, path: &str) -> String {
        let mut folders = vec![self.shim_folder()];
        folders.extend(std::env::split_paths(path));
        folders.extend(self.runtime_folders(path));
        std::env::join_paths(folders).map(|joined| joined.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_owned())
    }

    /// The app's environment with the installed CLIs on PATH.
    fn host(&self) -> BTreeMap<String, String> {
        let mut host = crate::shell::host_environment();
        let path = host.iter().find(|(key, _)| key.eq_ignore_ascii_case("PATH")).map(|(key, value)| (key.clone(), value.clone()));
        let (key, value) = path.unwrap_or_else(|| ("PATH".into(), String::new()));
        host.insert(key, self.tool_path(&value));
        host
    }

    /// The system's Node.js and npm; when there is none, the offer turns to installing
    /// Node.js itself with winget.
    fn node(&self) -> Result<(PathBuf, PathBuf), String> {
        match Programs::discover(&self.host()) {
            Programs { node: Some(node), npm_cli: Some(npm), .. } => Ok((node, npm)),
            _ => Err(crate::packages::needs("OpenJS.NodeJS.LTS", "Node.js")),
        }
    }

    /// The system's Python 3: on PATH, or where its installer puts it. The Microsoft
    /// Store's `python.exe` stand-in only opens the Store, and is passed over.
    fn python(&self) -> Option<PathBuf> {
        let host = self.host();
        let get = |name: &str| host.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.clone());
        let on_path = std::env::split_paths(&get("PATH").unwrap_or_default())
            .filter(|folder| !folder.to_string_lossy().to_ascii_lowercase().contains("windowsapps"))
            .map(|folder| folder.join("python.exe"))
            .find(|candidate| candidate.is_file());
        on_path.or_else(|| {
            let mut found: Vec<(u32, PathBuf)> = [get("LOCALAPPDATA").map(|base| PathBuf::from(base).join("Programs").join("Python")), get("ProgramFiles").map(PathBuf::from)]
                .into_iter()
                .flatten()
                .flat_map(|base| std::fs::read_dir(base).into_iter().flatten().flatten())
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let minor = name.strip_prefix("Python3")?.parse::<u32>().ok()?;
                    let program = entry.path().join("python.exe");
                    program.is_file().then_some((minor, program))
                })
                .collect();
            found.sort();
            found.pop().map(|(_, program)| program)
        })
    }

    /// Run an installer program quietly; its last error line when it fails.
    fn run_quietly(mut command: std::process::Command, what: &str) -> Result<String, String> {
        command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let output = command.output().map_err(|error| error.to_string())?;
        let text = format!("{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        if output.status.success() {
            return Ok(text);
        }
        Err(String::from_utf8_lossy(&output.stderr)
            .lines()
            .chain(String::from_utf8_lossy(&output.stdout).lines())
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{what} failed")))
    }

    /// Install an npm package into its own folder with the system's Node.js, and put
    /// the commands it provides on PATH.
    pub fn install_npm(&self, package: &str) -> Result<Vec<String>, String> {
        basalt_core::tools::install::command::package_name(package).map_err(|error| error.to_string())?;
        let (node, npm) = self.node()?;
        let folder = self.root.path().join("packages").join(package.trim_start_matches('@').replace('/', "+"));
        std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
        let mut command = std::process::Command::new(&node);
        command
            .arg(&npm)
            .args(["install", "--prefix"])
            .arg(&folder)
            .arg(format!("{package}@latest"))
            .args(["--no-audit", "--no-fund", "--loglevel=error"])
            // Install scripts find this Node.js too.
            .env_clear()
            .envs(self.host());
        Self::run_quietly(command, "npm install")?;
        let bins = folder.join("node_modules").join(".bin");
        let mut provided = Vec::new();
        for entry in std::fs::read_dir(&bins).map_err(|error| error.to_string())?.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("cmd")) {
                if let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) {
                    self.write_shim(name, &path, &[], false)?;
                    provided.push(name.to_owned());
                }
            }
        }
        Ok(provided)
    }

    /// Programs in `folder`, by name.
    fn programs_in(folder: &std::path::Path) -> std::collections::BTreeSet<String> {
        std::fs::read_dir(folder)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let program = path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("exe"));
                program.then(|| path.file_stem()?.to_str().map(str::to_owned)).flatten()
            })
            .collect()
    }

    /// Install a PyPI project into an environment of its own: with uv when the system
    /// has it, otherwise with the system's Python and a virtual environment. Without
    /// either, the offer turns to installing Python with winget.
    pub fn install_pypi(&self, project: &str) -> Result<Vec<String>, String> {
        if !crate::packages::plain(project) {
            return Err(format!("Invalid project name {project}"));
        }
        let host = self.host();
        let path = host.iter().find(|(key, _)| key.eq_ignore_ascii_case("PATH")).map(|(_, value)| value.clone()).unwrap_or_default();
        let uv = std::env::split_paths(&path).map(|folder| folder.join("uv.exe")).find(|candidate| candidate.is_file());
        if let Some(uv) = uv {
            let before = Self::programs_in(&self.shim_folder());
            let mut command = std::process::Command::new(uv);
            command
                .args(["tool", "install", "--force", project])
                .env_clear()
                .envs(host)
                .env("UV_TOOL_DIR", self.root.path().join("tools").join("pypi"))
                .env("UV_TOOL_BIN_DIR", self.shim_folder())
                .env("UV_NO_MODIFY_PATH", "1");
            Self::run_quietly(command, "uv tool install")?;
            let added: Vec<String> = Self::programs_in(&self.shim_folder()).difference(&before).cloned().collect();
            return Ok(if added.is_empty() { vec![project.to_ascii_lowercase()] } else { added });
        }
        let python = self.python().ok_or_else(|| crate::packages::needs("Python.Python.3.13", "Python"))?;
        let relative = PathBuf::from("tools").join("pypi").join(project.to_ascii_lowercase());
        self.root.remove_owned_tree(&relative).map_err(|error| error.to_string())?;
        let folder = self.root.path().join(&relative);
        let mut command = std::process::Command::new(python);
        command.args(["-m", "venv"]).arg(&folder);
        Self::run_quietly(command, "python -m venv")?;
        let scripts = folder.join("Scripts");
        let before = Self::programs_in(&scripts);
        let mut command = std::process::Command::new(scripts.join("python.exe"));
        command.args(["-m", "pip", "install", "--disable-pip-version-check", "--no-input", project]);
        Self::run_quietly(command, "pip install")?;
        let mut provided = Vec::new();
        for name in Self::programs_in(&scripts).difference(&before) {
            if crate::packages::plain(name) && !name.to_ascii_lowercase().starts_with("pip") {
                self.write_shim(name, &scripts.join(format!("{name}.exe")), &[], false)?;
                provided.push(name.clone());
            }
        }
        Ok(provided)
    }

    /// Install a winget package with winget. Where its commands land is winget's choice.
    pub fn install_winget(&self, id: &str) -> Result<Vec<String>, String> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c)) {
            return Err(format!("Invalid winget id {id}"));
        }
        let mut command = std::process::Command::new("winget");
        command
            .args(["install", "--id", id, "--exact", "--source", "winget", "--silent"])
            .args(["--disable-interactivity", "--accept-package-agreements", "--accept-source-agreements"]);
        Self::run_quietly(command, "winget install")?;
        Ok(Vec::new())
    }

    /// Skills and MCP servers a CLI keeps in a session profile.
    pub fn profile_contents(&self, profile: &str, tool: &str) -> basalt_core::profiles::sharing::Contents {
        let Ok(tool) = self.catalog.get(tool) else {
            return Default::default();
        };
        let home = basalt_core::profiles::environment::session_folder(profile).join(&tool.id);
        basalt_core::profiles::sharing::contents(&self.root, tool, &home)
    }

    /// Start sharing `category` from a session profile, for every CLI it applies to.
    pub fn join_sharing(
        &self,
        profile: &str,
        category: basalt_core::domain::ShareCategory,
    ) -> Result<(), String> {
        let sharing = basalt_core::profiles::sharing::SharedData::new(self.root.clone());
        for tool in self.catalog.tools() {
            if !tool.paths(category).is_empty() || (category == basalt_core::domain::ShareCategory::SharedMcp && tool.mcp.is_some()) {
                sharing.join_session(tool, profile, category).map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    /// Bring shared data into a session profile (as a terminal starts) or the profile's
    /// changes into the shared data (as a CLI exits).
    pub fn synchronize_sharing(
        &self,
        profile: &str,
        tool: Option<&str>,
        categories: &[basalt_core::domain::ShareCategory],
        persist: bool,
    ) -> Result<(), String> {
        if categories.is_empty() {
            return Ok(());
        }
        let sharing = basalt_core::profiles::sharing::SharedData::new(self.root.clone());
        for each in self.catalog.tools().iter().filter(|each| tool.is_none_or(|tool| each.id == tool)) {
            let result = if persist {
                sharing.persist_session(each, profile, categories)
            } else {
                sharing.hydrate_session(each, profile, categories)
            };
            result.map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn session_environment(&self, profile: &str) -> Result<BTreeMap<String, String>, String> {
        let mut tools: Vec<&str> = self.catalog.tools().iter().map(|tool| tool.id.as_str()).collect();
        tools.push("gemini");
        let mut values = basalt_core::profiles::environment::session(&self.root, &tools, profile)
            .map_err(|error| error.to_string())?;
        // Download caches hold no one's settings or sign-ins: the profile shares the
        // user's, instead of filling the disk with a copy per profile.
        if let Some(local) = crate::shell::host_environment().get("LOCALAPPDATA").map(PathBuf::from) {
            for (key, folder) in [("npm_config_cache", "npm-cache"), ("PIP_CACHE_DIR", "pip/Cache"), ("UV_CACHE_DIR", "uv/cache")] {
                values.insert(key.into(), local.join(folder).display().to_string());
            }
        }
        Ok(values)
    }
    /// The folder one CLI keeps in session profile `profile` (its id); None for an
    /// unknown CLI or a profile id that is not a plain folder name.
    pub fn profile_tool_folder(&self, profile: &str, tool: &str) -> Option<PathBuf> {
        let known = tool == "gemini" || self.catalog.tools().iter().any(|item| item.id == tool);
        let plain = !profile.is_empty() && profile.chars().all(|ch| ch.is_ascii_alphanumeric());
        (known && plain).then(|| {
            self.root
                .path()
                .join(basalt_core::profiles::environment::session_folder(profile))
                .join(tool)
        })
    }
    /// CLIs that have stored anything in session profile `profile` (its id): their
    /// folders hold at least one file.
    pub fn profile_tools(&self, profile: &str) -> Vec<String> {
        fn has_file(folder: &std::path::Path, depth: usize) -> bool {
            let Ok(entries) = std::fs::read_dir(folder) else {
                return false;
            };
            entries.flatten().any(|entry| match entry.file_type() {
                Ok(kind) if kind.is_file() => true,
                Ok(kind) if kind.is_dir() && depth > 0 => has_file(&entry.path(), depth - 1),
                _ => false,
            })
        }
        let base = self.root.path().join(basalt_core::profiles::environment::session_folder(profile));
        let mut tools: Vec<&str> = self.catalog.tools().iter().map(|tool| tool.id.as_str()).collect();
        tools.push("gemini");
        tools
            .into_iter()
            .filter(|tool| has_file(&base.join(tool), 6))
            .map(String::from)
            .collect()
    }
    fn global_program(tool: &Tool, host: &BTreeMap<String, String>) -> Option<PathBuf> {
        let get = |name: &str| {
            host.iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value)
        };
        let mut paths: Vec<_> = get("PATH")
            .map(|p| std::env::split_paths(p).collect())
            .unwrap_or_default();
        if let Some(home) = get("APPDATA") {
            paths.push(PathBuf::from(home).join("npm"));
        }
        if let Some(home) = get("USERPROFILE") {
            paths.extend([
                PathBuf::from(home).join(".local/bin"),
                PathBuf::from(home).join("AppData/Local/agy/bin"),
            ]);
        }
        paths
            .into_iter()
            .flat_map(|p| {
                [".exe", ".cmd", ".bat", ""].map(|ext| p.join(format!("{}{ext}", tool.bin)))
            })
            .find(|p| p.is_absolute() && p.is_file())
    }
    fn program(
        &self,
        tool: &Tool,
        host: &BTreeMap<String, String>,
    ) -> Result<Option<PathBuf>, String> {
        if command::resolve(&self.root, tool, &Programs::discover(host))
            .map_err(|e| e.to_string())?
            .is_some()
        {
            return Ok(None);
        }
        Self::global_program(tool, host).map(Some).ok_or_else(|| {
            format!(
                "{} is not installed. Install it in Profile Settings first.",
                tool.name
            )
        })
    }
    pub fn utility_program(&self, utility: &str) -> Result<PathBuf, String> {
        if utility != "lazygit" {
            return Err("Unknown terminal utility".into());
        }
        let owned = self.root.path().join("tools/lazygit/lazygit.exe");
        if owned.is_file() {
            return Ok(owned);
        }
        if let Some(paths) = std::env::var_os("PATH") {
            for path in std::env::split_paths(&paths) {
                let exe = path.join("lazygit.exe");
                if exe.is_absolute() && exe.is_file() {
                    return Ok(exe);
                }
            }
        }
        Err("LazyGit is not installed. Run scripts/install-lazygit.ps1".into())
    }
    pub fn utility_config(&self, utility: &str) -> Result<PathBuf, String> {
        if utility != "lazygit" {
            return Err("Unknown terminal utility".into());
        }
        self.root
            .create_dir("tools/lazygit/config")
            .map_err(|error| error.to_string())
    }
    fn service(&self) -> Result<Arc<BackendService>, String> {
        let mut slot = self
            .service
            .lock()
            .map_err(|_| "Backend service lock failed")?;
        if let Some(service) = &*slot {
            return Ok(service.clone());
        }
        let service = Arc::new(
            BackendService::open(self.root.clone(), self.identity.clone())
                .map_err(|e| e.to_string())?,
        );
        *slot = Some(service.clone());
        Ok(service)
    }
    pub fn start(
        &self,
        id: &str,
        state_file: Option<PathBuf>,
        fixture: Option<PathBuf>,
    ) -> Result<Terminal, String> {
        self.start_profile(id, &ProfileId::default_profile(), state_file, fixture)
    }
    pub fn start_profile(
        &self,
        id: &str,
        profile_id: &ProfileId,
        state_file: Option<PathBuf>,
        fixture: Option<PathBuf>,
    ) -> Result<Terminal, String> {
        self.start_profile_args(id, profile_id, state_file, fixture, vec![], None)
    }
    pub fn start_workspace_terminal(
        &self,
        id: &str,
        profile_id: &ProfileId,
        tab: uuid::Uuid,
        state_file: Option<PathBuf>,
        fixture: Option<PathBuf>,
        saved: Option<basalt_core::tools::conversation::Conversation>,
    ) -> Result<
        (
            Terminal,
            basalt_core::tools::conversation::Conversation,
            PathBuf,
        ),
        String,
    > {
        let service = self.service()?;
        let tool = self.catalog.get(id).map_err(|e| e.to_string())?;
        let profile = service
            .get_profile(id, profile_id)
            .map_err(|e| e.to_string())?;
        let home = self
            .root
            .resolve(tool.profile_relative(&profile))
            .map_err(|e| e.to_string())?;
        let directory = self
            .root
            .create_dir(
                tool.profile_relative(&profile)
                    .join("workspace/terminals")
                    .join(tab.to_string()),
            )
            .map_err(|e| e.to_string())?;
        let mut conversation = saved
            .unwrap_or_else(|| basalt_core::tools::conversation::Conversation::new(id, directory));
        if conversation.tool != id {
            return Err("Saved conversation belongs to a different CLI".into());
        }
        if conversation.started && conversation.id.is_none() {
            conversation.id = conversation.discover(&home);
        }
        if id == "cursor-agent" && !conversation.started && fixture.is_none() {
            let host = crate::shell::host_environment();
            let env = basalt_core::profiles::environment::build(
                &self.root,
                tool,
                &profile,
                &self.identity,
                &host,
            )
            .map_err(|e| e.to_string())?;
            let resolved = command::resolve(&self.root, tool, &Programs::discover(&host))
                .map_err(|e| e.to_string())?;
            let (program, mut prefix) = if let Some(resolved) = resolved {
                (resolved.program, resolved.prefix)
            } else {
                (
                    Self::global_program(tool, &host).ok_or("Cursor Agent is unavailable")?,
                    vec![],
                )
            };
            prefix.push("create-chat".into());
            let request = basalt_core::system::process::CommandRequest {
                program,
                arguments: prefix,
                cwd: conversation.directory.clone(),
                environment: env.values,
                timeout: Duration::from_secs(30),
                capture_stdout: Some(16384),
            };
            if let Ok(result) = basalt_core::system::process::run(
                request,
                &CancellationToken::new(),
                Arc::new(basalt_core::system::process::DiscardOutput),
            ) {
                if result.exit_code == Some(0) {
                    let value = String::from_utf8_lossy(&result.stdout);
                    if let Some(id) = value
                        .lines()
                        .rev()
                        .map(str::trim)
                        .find(|id| basalt_core::tools::conversation::valid_id(id))
                    {
                        conversation.id = Some(id.into());
                        conversation.started = true;
                    }
                }
            }
        }
        let arguments = conversation.arguments().map_err(|e| e.to_string())?;
        let cwd = conversation.directory.clone();
        let terminal = self.start_profile_args(
            id,
            profile_id,
            state_file,
            fixture,
            arguments,
            Some(cwd.clone()),
        )?;
        conversation.started = true;
        Ok((terminal, conversation, cwd))
    }
    pub fn start_profile_args(
        &self,
        id: &str,
        profile_id: &ProfileId,
        state_file: Option<PathBuf>,
        fixture: Option<PathBuf>,
        arguments: Vec<String>,
        working_directory: Option<PathBuf>,
    ) -> Result<Terminal, String> {
        let tool = self.catalog.get(id).map_err(|e| e.to_string())?;
        let mut host = crate::shell::host_environment();
        host.remove("BASALT_AGENT_STATE_FILE");
        if let Some(path) = state_file {
            host.insert(
                "BASALT_AGENT_STATE_FILE".into(),
                path.to_string_lossy().into_owned(),
            );
        }
        let program = if let Some(fixture) = fixture.clone() {
            Some(fixture)
        } else {
            self.program(tool, &host)?
        };
        let service = self.service()?;
        if id == "opencode" {
            let profile = service
                .get_profile(id, profile_id)
                .map_err(|e| e.to_string())?;
            let plugins = tool.profile_relative(&profile).join(".config/opencode/plugins");
            // The same plugin under the app's earlier name.
            let _ = std::fs::remove_file(self.root.path().join(&plugins).join("omnishell-session.js"));
            self.root
                .write(
                    plugins.join("basalt-session.js"),
                    include_bytes!("hooks/opencode_session.js"),
                )
                .map_err(|e| e.to_string())?;
        }
        if matches!(id, "codex" | "claude") {
            let profile = service
                .get_profile(id, profile_id)
                .map_err(|e| e.to_string())?;
            let program = self.hook_program()?;
            let config = tool.profile_relative(&profile).join(if id == "codex" {
                ".codex/hooks.json"
            } else {
                ".claude/settings.json"
            });
            let before = self
                .root
                .read_json::<serde_json::Value>(&config)
                .map_err(|e| e.to_string())?
                .unwrap_or_else(|| serde_json::json!({}));
            let after = agent_bridge::merge_hooks(before.clone(), &program, id)?;
            if before != after {
                self.root
                    .write_json(config, &after)
                    .map_err(|e| e.to_string())?;
            }
        }
        let mut request = SessionStartRequest::new(id, profile_id.clone());
        request.arguments = arguments;
        request.working_directory = working_directory.clone();
        request.program = program;
        request.host_environment = Some(host);
        request.palette = Some(screen::palette());
        request.conpty_runtime = Some(
            crate::shell::conpty_runtime(),
        );
        if fixture.is_some() {
            request.arguments.insert(0, "echo".into());
        }
        if fixture.is_none() && id == "codex" {
            request.arguments.push("--no-daemon".into());
        }
        let session = service.start_session(request).map_err(|e| e.to_string())?;
        let mut terminal = Terminal::managed(session, &self.root).map_err(|e| e.to_string())?;
        if let (Some(directory), Terminal::Managed { cwd, .. }) = (working_directory, &mut terminal)
        {
            *cwd = directory;
        }
        Ok(terminal)
    }
}
