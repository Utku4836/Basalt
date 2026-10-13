#![cfg_attr(windows, windows_subsystem = "windows")]
#[path = "app/keyboard.rs"]
mod input;
#[path = "testing/native_smoke.rs"]
mod native_smoke;
#[cfg(windows)]
#[path = "app/native_frame.rs"]
mod window_frame;
use slint::{ComponentHandle, Model, ModelRc, VecModel, winit_030::WinitWindowAccessor};
use std::{
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, mpsc},
    time::Duration,
};
use basalt_core::domain::ProfileId;
use basalt_core::{
    terminal::pty::{NativeTerminal, SpawnRequest},
    terminal::TerminalSize,
};
use uuid::Uuid;
use basalt::launcher::{Engine, Terminal as RuntimeTerminal};
use basalt::{
    DividerItem, DropPreview, MainWindow, PaneItem, TabItem, TerminalFrame, WorkspaceItem,
    commands::{Command, CommandTarget},
    screen,
    split::{Orientation, Rect},
    workspace::{Desktop, DropZone, drop_zone},
};
type SpawnResult = Result<
    (
        RuntimeTerminal,
        Option<basalt_core::tools::conversation::Conversation>,
        Option<PathBuf>,
    ),
    String,
>;
struct Surface {
    terminal: RuntimeTerminal,
    revision: u64,
    title_revision: u64,
    frame: TerminalFrame,
    /// Keeps the frame's line model and rebuilds only the lines that changed.
    renderer: screen::ScreenRenderer,
    /// How the grid was last drawn on the GPU.
    look: Option<basalt::gpu::Look>,
    size: TerminalSize,
    /// Grid requested by the layout but not yet sent to the PTY, and since when.
    pending_size: Option<(TerminalSize, std::time::Instant)>,
    sized: bool,
    focused: bool,
    exited: bool,
}
/// Window drags and sidebar changes request many grids in a row; programs only
/// need the final one, and every resize makes full-screen CLIs repaint.
const RESIZE_SETTLE: Duration = Duration::from_millis(60);
thread_local! {
    /// Runs on the UI thread when a terminal's screen changed.
    static WAKE: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}
/// Wakes the UI thread from a terminal's threads, once until it has run.
fn terminal_waker() -> basalt_core::terminal::Waker {
    static PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    Arc::new(|| {
        if !PENDING.swap(true, std::sync::atomic::Ordering::AcqRel) {
            let _ = slint::invoke_from_event_loop(|| {
                PENDING.store(false, std::sync::atomic::Ordering::Release);
                WAKE.with(|wake| {
                    if let Some(wake) = &*wake.borrow() {
                        wake();
                    }
                });
            });
        }
    })
}
/// Results of work done off the UI thread for an install offer.
enum InstallEvent {
    Found(Uuid, String, Vec<basalt::packages::Offer>),
    /// The commands the installed tool provides.
    Installed(Uuid, Vec<String>),
    Failed(Uuid, String),
}
/// Tools offered for a command that was not found, and where installing stands.
struct InstallOffer {
    tab: Uuid,
    command: String,
    offers: Vec<basalt::packages::Offer>,
    selected: usize,
    /// While installing, and briefly after, as the bar fills.
    progress: Option<basalt::gpu::Progress>,
    failed: Option<String>,
    /// Typed into the terminal once the bar has filled.
    then: Option<String>,
    /// Why a runtime is offered instead ("Node.js is needed first"), and the choice
    /// that needed it, installed once the runtime is.
    reason: Option<String>,
    resume: Option<basalt::packages::Offer>,
}
impl InstallOffer {
    fn new(tab: Uuid, command: String, offers: Vec<basalt::packages::Offer>) -> Self {
        Self { tab, command, offers, selected: 0, progress: None, failed: None, then: None, reason: None, resume: None }
    }
    fn installing(&self) -> bool {
        self.progress.is_some()
    }
}
/// A completion list open in a terminal: flags or subcommands of `program`.
struct Completion {
    tab: Uuid,
    program: String,
    commands: bool,
    selected: usize,
}
/// Slint's device, reported by the rendering notifier once the window renders.
enum GpuSetup {
    Waiting,
    Device(wgpu::Device, wgpu::Queue),
    /// Not rendering with WGPU: panes draw their lines as text.
    Unavailable,
    Attached,
}
fn native_background(window: &MainWindow) -> u32 {
    let color = window.global::<basalt::Theme>().get_base();
    (u32::from(color.red()) << 16) | (u32::from(color.green()) << 8) | u32::from(color.blue())
}
fn synchronize_native_frame(window: &MainWindow) {
    window.window().with_winit_window(|native| {
        native.set_theme(Some(slint::winit_030::winit::window::Theme::Dark));
        #[cfg(windows)]
        if let Ok(width) = window_frame::install(native) {
            window.set_caption_width(width);

            window_frame::set_appearance(native, native_background(window), true);
            window_frame::set_header_height(native, f64::from(window.get_header_height()));
            window_frame::set_control_region(
                native,
                f64::from(window.get_header_control_left()),
                f64::from(window.get_header_control_width()),
            );
        }
    });
}
/// "3 skills · 2 mcp", leaving out what a CLI does not have.
fn contents_label(contents: basalt_core::profiles::sharing::Contents) -> String {
    let mut parts = Vec::new();
    if contents.skills > 0 {
        parts.push(format!("{} skill{}", contents.skills, if contents.skills == 1 { "" } else { "s" }));
    }
    if contents.mcp_servers > 0 {
        parts.push(format!("{} mcp", contents.mcp_servers));
    }
    parts.join(" · ")
}
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
/// Compact age for the sidebar: now, 5m, 3h, 2d, 6w.
fn age_label(then: u64, now: u64) -> String {
    if then == 0 {
        return String::new();
    }
    let seconds = now.saturating_sub(then);
    match seconds {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        86_400..1_209_600 => format!("{}d", seconds / 86_400),
        _ => format!("{}w", seconds / 604_800),
    }
}
struct Controller {
    self_weak: std::rc::Weak<RefCell<Controller>>,
    default_palette: i32,
    default_font: i32,
    default_terminal_font: i32,
    selected_text: HashMap<Uuid, String>,
    clipboard: basalt::clipboard::Clipboard,
    agent_clock: HashMap<Uuid, basalt::agent_summary::Clock>,
    agents_refreshed: std::time::Instant,
    /// Workspace folder → (checked at, branch). Refresh runs per frame; HEAD is re-read at most every few seconds.
    branches: HashMap<PathBuf, (std::time::Instant, String)>,
    /// CLIs used in each session profile, rechecked at most every few seconds.
    profile_tools: HashMap<Uuid, (std::time::Instant, Vec<(String, String)>)>,
    preferences: Option<basalt::preferences::AppearancePreferences>,
    launches: std::collections::BTreeMap<Uuid, basalt::workspace_store::TerminalLaunch>,
    active_workspaces: std::collections::HashSet<Uuid>,
    save_tx: Option<mpsc::SyncSender<Vec<u8>>>,
    save_dirty: bool,
    pane_model: Rc<VecModel<PaneItem>>,
    dock_pane_model: Rc<VecModel<PaneItem>>,
    desktop: Desktop,
    surfaces: HashMap<Uuid, Surface>,
    activities: HashMap<Uuid, basalt::agents::Activity>,
    /// What each agent's screen shows, read as it changes.
    screens: basalt::agent_screen::Readings,
    screen_states: HashMap<Uuid, basalt::agent_screen::Reading>,
    /// Per agent terminal: the state before the last refresh, and whether a turn
    /// ended while you were looking elsewhere (shown as done until you look).
    agent_turns: HashMap<Uuid, (i32, bool)>,
    /// When each terminal's lifecycle signal last changed.
    signal_changed: HashMap<Uuid, std::time::Instant>,
    signals: HashMap<Uuid, basalt::agent_bridge::Signal>,
    signal_invalidated: HashMap<Uuid, std::time::Instant>,
    bridge: Option<basalt::agent_bridge::Bridge>,
    engine: Arc<Engine>,
    tab_tools: HashMap<Uuid, String>,
    tab_profiles: HashMap<Uuid, ProfileId>,
    cli_context: Option<Uuid>,
    fixture_shells: bool,
    failures: HashMap<Uuid, String>,
    pending: Vec<Uuid>,
    pending_input: HashMap<Uuid, Vec<u8>>,
    viewport: Rect,
    dock_width: f64,
    tabbar_height: f64,
    tab_bounds: Rc<RefCell<HashMap<Uuid, (f64, f64)>>>,
    cwd: PathBuf,
    tx: mpsc::Sender<(Uuid, SpawnResult)>,
    /// Draws terminal grids on the GPU, at frame time; `None` when panes draw their
    /// lines as text.
    grids: Option<Rc<RefCell<basalt::gpu::Grids>>>,
    gpu_setup: Rc<RefCell<GpuSetup>>,
    /// Command history per profile folder ("default" for the user's own setup).
    histories: HashMap<String, basalt::history::History>,
    history_folder: Option<PathBuf>,
    /// What accepting each terminal's inline suggestion types.
    suggestions: HashMap<Uuid, String>,
    /// A command sent with Enter, kept once the next prompt says it succeeded.
    running: HashMap<Uuid, (String, std::time::SystemTime)>,
    /// Each terminal's folder, as its prompt last reported it.
    folders: HashMap<Uuid, (std::time::SystemTime, String)>,
    /// The terminal a history search types into, its query, and whether it keeps to
    /// that terminal's folder.
    history_search: Option<Uuid>,
    history_query: String,
    history_here: bool,
    /// The open completion list, and what each program's help offers (`None` while
    /// it is being asked).
    completion: Option<Completion>,
    helps: HashMap<String, Option<basalt::flags::Help>>,
    help_tx: mpsc::Sender<(String, basalt::flags::Help)>,
    /// What was offered for a command that was not found, and the prompt reports
    /// already handled.
    install: Option<InstallOffer>,
    install_tx: mpsc::Sender<InstallEvent>,
    /// Frame pacing: panes read terminals at most once per display refresh.
    panes_refreshed: std::time::Instant,
    panes_scheduled: bool,
    frame_interval: Duration,
}
impl Controller {
    /// Where terminal `tab`'s shell reports its folder and last result.
    fn shell_state(&self, tab: Uuid) -> Option<PathBuf> {
        self.bridge.as_ref().map(|bridge| bridge.root.join(format!("shell-{}.txt", tab.simple())))
    }
    /// The history folder name of the profile terminal `tab` runs in.
    fn history_key(&self, tab: Uuid) -> String {
        self.desktop
            .containers()
            .find(|workspace| workspace.base_pane_for_tab(tab).is_some())
            .and_then(|workspace| workspace.profile)
            .map(|profile| profile.simple().to_string())
            .unwrap_or_else(|| "default".into())
    }
    fn history(&mut self, key: &str) -> &mut basalt::history::History {
        let path = self.history_folder.as_ref().map(|folder| folder.join(format!("{key}.json")));
        self.histories
            .entry(key.to_owned())
            .or_insert_with(|| path.map(|path| basalt::history::History::load(&path)).unwrap_or_default())
    }
    /// What the prompt last reported: whether the last command succeeded, and the folder.
    fn read_shell_state(&mut self, tab: Uuid) -> Option<(std::time::SystemTime, bool)> {
        let path = self.shell_state(tab)?;
        let modified = std::fs::metadata(&path).and_then(|meta| meta.modified()).ok()?;
        let text = std::fs::read_to_string(&path).ok()?;
        let mut lines = text.lines();
        let ok = lines.next()? == "1";
        let folder = lines.next().unwrap_or("").to_owned();
        self.folders.insert(tab, (modified, folder));
        Some((modified, ok))
    }
    /// `install <name>` or a pasted global npm install, entered in terminal `tab`: the
    /// line stays as typed and the offer opens under it.
    fn request_install(&mut self, tab: Uuid, request: basalt::packages::Request) {
        use basalt::packages::{Offer, Request, Source, title};
        match request {
            Request::Search(name) => {
                self.install = Some(InstallOffer::new(tab, name.clone(), Vec::new()));
                let (engine, tx) = (self.engine.clone(), self.install_tx.clone());
                std::thread::spawn(move || {
                    let identity = engine.identity().clone();
                    let offers = basalt::packages::search(engine.catalog(), &identity, &name);
                    let _ = tx.send(InstallEvent::Found(tab, name, offers));
                });
            }
            Request::Package(package) => {
                let offer = Offer {
                    title: title(&package),
                    package: format!("npm:{package}"),
                    by: basalt::packages::scope(&package).unwrap_or_default(),
                    source: Source::Npm(package.clone()),
                    weekly: None,
                    commands: Vec::new(),
                };
                self.install = Some(InstallOffer::new(tab, title(&package), vec![offer]));
            }
        }
    }
    /// Empty terminal `tab`'s command line: to its end, then delete back to its start.
    fn clear_line(&self, tab: Uuid) {
        use slint::platform::Key;
        if let Some(surface) = self.surfaces.get(&tab) {
            let mode = surface.terminal.model().mode();
            let mut bytes = input::encode(&char::from(Key::End).to_string(), false, false, false, mode);
            bytes.extend(input::encode(&char::from(Key::Home).to_string(), true, false, false, mode));
            let _ = surface.terminal.input(bytes);
        }
    }
    fn install_event(&mut self, window: &MainWindow, event: InstallEvent) {
        match event {
            InstallEvent::Found(tab, command, offers) => {
                // Only for the request still open; Esc may have closed it meanwhile.
                if let Some(offer) = self.install.as_mut().filter(|offer| offer.tab == tab && offer.command == command && offer.offers.is_empty()) {
                    if offers.is_empty() {
                        offer.failed = Some(format!("nothing found for '{command}'"));
                    }
                    offer.offers = offers;
                }
            }
            InstallEvent::Installed(tab, provided) => {
                // A runtime that was needed first: now the tool that needed it.
                if let Some(offer) = self.install.as_mut().filter(|offer| offer.tab == tab && offer.resume.is_some()) {
                    offer.offers = offer.resume.take().into_iter().collect();
                    (offer.selected, offer.reason, offer.progress) = (0, None, None);
                    self.start_install();
                    return;
                }
                if let Some(offer) = self.install.as_mut().filter(|offer| offer.tab == tab) {
                    // The new command goes on the line, ready to run.
                    let ready = provided.iter().find(|each| each.eq_ignore_ascii_case(&offer.command)).or(provided.first());
                    offer.then = Some(ready.map(|each| format!("{each} ")).unwrap_or_default());
                    if let Some(progress) = &mut offer.progress {
                        progress.finished = Some(std::time::Instant::now());
                    }
                }
            }
            InstallEvent::Failed(tab, error) => match &mut self.install {
                Some(offer) if offer.tab == tab => {
                    offer.progress = None;
                    // The system lacks a runtime: offer it, then go on with this choice.
                    if let Some((id, name)) = basalt::packages::needed(&error) {
                        offer.resume = offer.offers.get(offer.selected).cloned();
                        offer.offers = vec![basalt::packages::Offer {
                            title: name.clone(),
                            package: format!("winget:{id}"),
                            by: id.split('.').next().unwrap_or("").to_owned(),
                            source: basalt::packages::Source::Winget { id, version: String::new() },
                            weekly: None,
                                    commands: Vec::new(),
                        }];
                        offer.selected = 0;
                        offer.reason = Some(format!("{name} is needed first"));
                        return;
                    }
                    let error: String = error.lines().next().unwrap_or("").chars().take(72).collect();
                    offer.failed = Some(format!("Failed · {error}"));
                }
                _ => window.set_message(error.into()),
            },
        }
    }
    /// An install whose bar has filled: close the offer and go on in the terminal.
    fn finish_install(&mut self) -> bool {
        let filled = self.install.as_ref().and_then(|offer| offer.progress).and_then(|progress| progress.finished);
        if filled.is_none_or(|finished| finished.elapsed() < Duration::from_millis(450)) {
            return false;
        }
        let Some(offer) = self.install.take() else {
            return false;
        };
        self.clear_line(offer.tab);
        if let (Some(then), Some(surface)) = (offer.then.filter(|then| !then.is_empty()), self.surfaces.get(&offer.tab)) {
            let _ = surface.terminal.input(then.into_bytes());
        }
        true
    }
    /// Install the selected offer in the background.
    fn start_install(&mut self) {
        let Some(offer) = &mut self.install else {
            return;
        };
        let Some(chosen) = offer.offers.get(offer.selected).cloned() else {
            return;
        };
        offer.failed = None;
        offer.progress = Some(basalt::gpu::Progress::new(std::time::Instant::now(), 8.0));
        let tab = offer.tab;
        let (engine, tx) = (self.engine.clone(), self.install_tx.clone());
        std::thread::spawn(move || {
            let result = match &chosen.source {
                basalt::packages::Source::Recipe(tool) => engine
                    .install_recipe(tool, &basalt_core::system::process::CancellationToken::new())
                    .map(|_| chosen.commands.clone()),
                basalt::packages::Source::Npm(package) => engine.install_npm(package),
                basalt::packages::Source::Pypi(project) => engine.install_pypi(project),
                basalt::packages::Source::Winget { id, .. } => engine.install_winget(id),
            };
            let _ = tx.send(match result {
                Ok(provided) => InstallEvent::Installed(tab, provided),
                Err(error) => InstallEvent::Failed(tab, error),
            });
        });
    }
    /// Fill the history search with the commands that match its query.
    fn search_history(&mut self, ui: &MainWindow) {
        let Some(tab) = self.history_search else {
            return;
        };
        let key = self.history_key(tab);
        let folder = self.history_here.then(|| self.folders.get(&tab).map(|(_, folder)| folder.clone()).unwrap_or_default());
        let query = self.history_query.clone();
        let now = unix_now();
        let items: Vec<_> = self
            .history(&key)
            .search(&query, folder.as_deref(), now)
            .into_iter()
            .take(200)
            .map(|entry| {
                let place = std::path::Path::new(&entry.folder)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| entry.folder.clone());
                let ago = age_label(entry.last, now);
                basalt::PaletteItem {
                    id: entry.command.clone().into(),
                    title: entry.command.clone().into(),
                    detail: [place, ago].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" · ").into(),
                }
            })
            .collect();
        ui.set_history_scope(if self.history_here { "this folder" } else { "all folders" }.into());
        ui.set_history_items(ModelRc::new(VecModel::from(items)));
    }
    /// The inline suggestion and risky parts for terminal `tab`'s command line; also
    /// keeps the last command once its prompt reports success.
    fn command_overlay(
        &mut self,
        tab: Uuid,
        focused: bool,
        line: Option<basalt::command_line::CommandLine>,
        (faint, amber, danger, list_colors): ([u8; 3], [u8; 3], [u8; 3], [[u8; 3]; 3]),
    ) -> Option<basalt::gpu::Overlay> {
        self.suggestions.remove(&tab);
        let line = line?;
        let state = self.read_shell_state(tab);
        if let (Some((_, sent_at)), Some((reported, ok))) = (self.running.get(&tab).cloned(), state) {
            if reported >= sent_at {
                let (command, _) = self.running.remove(&tab).unwrap();
                // A program used for a while was meant, even if it ended with an error
                // (an agent CLI left with Ctrl+C); a quick failure is likely a typo.
                let used = reported.duration_since(sent_at).is_ok_and(|ran| ran >= Duration::from_secs(5));
                if ok || used {
                    let key = self.history_key(tab);
                    let folder = self.folders.get(&tab).map(|(_, folder)| folder.clone()).unwrap_or_default();
                    let path = self.history_folder.as_ref().map(|folder| folder.join(format!("{key}.json")));
                    let history = self.history(&key);
                    if history.record(&command, &folder, unix_now()) {
                        if let Some(path) = path {
                            let _ = history.save(&path);
                        }
                    }
                }
            }
        }
        if !focused {
            return None;
        }
        let risky: Vec<_> = basalt::history::risky_spans(&line.text)
            .into_iter()
            .map(|span| line.start + span.start..line.start + span.end)
            .collect();
        // An open completion list follows what is typed, and closes when nothing matches.
        let mut list = Vec::new();
        let (mut selected, mut list_column) = (0, 0);
        let (mut question, mut question_color) = (String::new(), faint);
        let mut progress = None;
        // Said right after the typed request, until the list is there.
        let mut status = String::new();
        if let Some(offer) = self.install.as_ref().filter(|offer| offer.tab == tab && offer.offers.is_empty()) {
            status = match &offer.failed {
                Some(failed) => format!("  {failed}"),
                None => "  searching…".into(),
            };
        }
        if let Some(offer) = self.install.as_ref().filter(|offer| offer.tab == tab && !offer.offers.is_empty()) {
            list = offer
                .offers
                .iter()
                .map(|each| basalt::gpu::ListItem {
                    // The catalog's own tools are checked by hand.
                    name: match each.source {
                        basalt::packages::Source::Recipe(_) => format!("{} ✓", each.title),
                        _ => each.title.clone(),
                    },
                    by: if each.by.is_empty() { String::new() } else { format!("by {}", each.by) },
                    detail: each.package.clone(),
                    note: each.note(),
                })
                .collect();
            selected = offer.selected.min(list.len().saturating_sub(1));
            list_column = line.start;
            progress = offer.progress;
            let chosen = &offer.offers[selected];
            let adds = if offer.command.is_empty() || chosen.provides(&offer.command) || chosen.commands.is_empty() {
                String::new()
            } else {
                format!(" · adds {}", chosen.commands.join(", "))
            };
            (question, question_color) = match (&offer.failed, &offer.reason) {
                (Some(failed), _) => (failed.clone(), danger),
                (None, Some(reason)) => (format!("{reason} · install? [Y/n]"), amber),
                _ if chosen.well_known() => (format!("Install? [Y/n]{adds}"), amber),
                _ => (format!("Less known · install? [Y/n]{adds}"), amber),
            };
        }
        if let Some(completion) = self.completion.as_mut().filter(|completion| completion.tab == tab && list.is_empty()) {
            let word = basalt::flags::current_word(&line.text);
            let same_program = basalt::flags::program(&line.text) == Some(completion.program.as_str());
            match self.helps.get(&completion.program) {
                Some(Some(help)) if same_program && line.at_end() => {
                    let entries = if completion.commands { &help.commands } else { &help.flags };
                    list = basalt::flags::matching(entries, word)
                        .into_iter()
                        .map(|entry| basalt::gpu::ListItem {
                            name: entry.name.clone(),
                            by: String::new(),
                            detail: entry.description.clone(),
                            note: String::new(),
                        })
                        .collect();
                    if list.is_empty() {
                        self.completion = None;
                    } else {
                        completion.selected = completion.selected.min(list.len() - 1);
                        selected = completion.selected;
                        list_column = line.start + line.text.chars().count() - word.chars().count();
                    }
                }
                Some(None) if same_program => {}
                _ => self.completion = None,
            }
        }
        let mut ghost = String::new();
        if line.at_end() && list.is_empty() && self.install.as_ref().is_none_or(|offer| offer.tab != tab) {
            // `install cl` completes to a known tool before history is asked.
            let typed = line.text.strip_prefix("install ").filter(|typed| basalt::packages::plain(typed));
            if let Some(rest) = typed.and_then(|typed| basalt::packages::complete(self.engine.catalog(), typed).map(|bin| bin[typed.len()..].to_owned())) {
                ghost = rest;
            } else {
                let key = self.history_key(tab);
                let folder = self.folders.get(&tab).map(|(_, folder)| folder.clone()).unwrap_or_default();
                if let Some(command) = self.history(&key).suggest(&line.text, &folder, unix_now()) {
                    ghost = command[line.text.len()..].to_owned();
                }
            }
        }
        if ghost.is_empty() && risky.is_empty() && list.is_empty() && status.is_empty() {
            return None;
        }
        let hint = if ghost.is_empty() { String::new() } else { "→ accept".to_owned() };
        if !ghost.is_empty() {
            self.suggestions.insert(tab, ghost.clone());
        } else {
            ghost = status;
        }
        Some(basalt::gpu::Overlay {
            row: line.row,
            column: line.start + line.text.chars().count(),
            hint,
            ghost,
            color: faint,
            risky,
            risky_color: amber,
            list,
            selected,
            list_column,
            list_colors,
            question,
            question_color,
            progress,
        })
    }
    /// A CLI in terminal `tab` exited: its profile's shared data takes what it changed.
    fn persist_sharing(&self, tab: Uuid, cli: &str) {
        let Some(profile) = self
            .desktop
            .containers()
            .find(|workspace| workspace.base_pane_for_tab(tab).is_some())
            .and_then(|workspace| workspace.profile)
            .and_then(|profile| self.desktop.profiles.iter().find(|each| each.id == profile))
        else {
            return;
        };
        let categories = profile.shared();
        let tool = self.engine.catalog().tools().iter().find(|tool| tool.name.eq_ignore_ascii_case(cli) || tool.id.eq_ignore_ascii_case(cli));
        if let Some(tool) = tool {
            let (engine, profile, tool) = (self.engine.clone(), profile.id.simple().to_string(), tool.id.clone());
            std::thread::spawn(move || {
                let _ = engine.synchronize_sharing(&profile, Some(&tool), &categories, true);
            });
        }
    }
    fn palette_current(&self) -> basalt::palette::Current {
        basalt::palette::Current {
            palette: self.default_palette,
            interface_percent: self.default_font,
            terminal_percent: self.default_terminal_font,
        }
    }
    fn set_palette(&mut self, ui: &MainWindow, palette: i32) {
        if !(0..basalt::preferences::PALETTE_COUNT).contains(&palette) {
            return;
        }
        if let Some(settings) = &self.preferences {
            if let Err(error) = settings.set_color_palette(palette) {
                ui.set_message(error.to_string().into());
                return;
            }
        }
        self.default_palette = palette;
        self.refresh(ui);
    }
    fn set_interface_font(&mut self, ui: &MainWindow, percent: i32) {
        let percent = basalt::typography::normalize_percent(percent);
        if let Some(settings) = &self.preferences {
            if let Err(error) = settings.set_font_percent(percent) {
                ui.set_message(error.to_string().into());
                return;
            }
        }
        self.default_font = percent;
        self.refresh(ui);
    }
    fn set_terminal_font(&mut self, ui: &MainWindow, percent: i32) {
        let percent = basalt::typography::normalize_percent(percent);
        if let Some(settings) = &self.preferences {
            if let Err(error) = settings.set_terminal_font_percent(percent) {
                ui.set_message(error.to_string().into());
                return;
            }
        }
        self.default_terminal_font = percent;
        self.refresh(ui);
    }
    /// Run a command palette entry by id.
    fn run_palette(&mut self, ui: &MainWindow, id: &str) {
        use basalt::palette::step;
        match id {
            "close-terminal" => {
                let pane = self.desktop.focused_container().focused_view_pane();
                if let Some(tab) = self.desktop.focused_container().selected_tab() {
                    let retired = self.desktop.close_surface(pane, tab);
                    self.retire(retired);
                    self.ensure_terminals();
                    self.refresh(ui);
                }
            }
            "toggle-sidebar" => ui.set_sidebar_visible(!ui.get_sidebar_visible()),
            "new-profile" => ui.invoke_profile_new(),
            "terminal-larger" => self.set_terminal_font(ui, step(self.default_terminal_font, true)),
            "terminal-smaller" => self.set_terminal_font(ui, step(self.default_terminal_font, false)),
            "terminal-reset" => self.set_terminal_font(ui, 100),
            "interface-larger" => self.set_interface_font(ui, step(self.default_font, true)),
            "interface-smaller" => self.set_interface_font(ui, step(self.default_font, false)),
            "interface-reset" => self.set_interface_font(ui, 100),
            "reset-appearance" => ui.invoke_reset_workspace_appearance(),
            _ => {
                if let Some(palette) = id.strip_prefix("theme:").and_then(|index| index.parse().ok()) {
                    self.set_palette(ui, palette);
                } else if let Some(command) = Command::from_id(id) {
                    self.dispatch(ui, command, CommandTarget::Focused);
                }
            }
        }
    }
    pub fn visible_layout(&self, scale: f64) -> Option<basalt::split::Layout> {
        if self.desktop.selected.is_nil() {
            return Some(basalt::split::Layout::default());
        }
        let mut layout = self.desktop.current().layout(self.viewport, scale).unwrap_or_default();
        if let Some(dock) = self.desktop.dock.as_ref().filter(|_| self.desktop.dock_visible) {
            let dock_layout =
                dock.layout(Desktop::dock_viewport(self.viewport, self.dock_width), scale)?;
            layout.panes.extend(dock_layout.panes);
            layout.dividers.extend(dock_layout.dividers);
        }
        Some(layout)
    }
    /// Hand Slint's device to the grid renderer once it exists, or fall back to text.
    /// True when panes must draw again.
    fn take_gpu(&mut self) -> bool {
        let setup = std::mem::replace(&mut *self.gpu_setup.borrow_mut(), GpuSetup::Attached);
        match setup {
            GpuSetup::Device(device, queue) => {
                if let Some(grids) = &self.grids {
                    grids.borrow_mut().attach(&device, &queue);
                }
            }
            GpuSetup::Unavailable => self.grids = None,
            setup => {
                *self.gpu_setup.borrow_mut() = setup;
                return false;
            }
        }
        for surface in self.surfaces.values_mut() {
            surface.revision = u64::MAX;
        }
        true
    }
    /// Read changed terminals into the panes, at most once per display refresh: output
    /// that arrives sooner waits for the next one.
    fn paced_refresh(&mut self, window: &MainWindow) {
        let since = self.panes_refreshed.elapsed();
        if since >= self.frame_interval {
            self.refresh_panes(window);
        } else if !self.panes_scheduled {
            self.panes_scheduled = true;
            let (state, weak) = (self.self_weak.clone(), window.as_weak());
            slint::Timer::single_shot(self.frame_interval - since, move || {
                if let (Some(state), Some(window)) = (state.upgrade(), weak.upgrade()) {
                    if let Ok(mut owner) = state.try_borrow_mut() {
                        owner.panes_scheduled = false;
                        owner.refresh_panes(&window);
                    }
                }
            });
        }
    }
    fn clear_selection(&mut self, window: &MainWindow) {
        self.selected_text.clear();
        window.set_selection_epoch(window.get_selection_epoch().wrapping_add(1));
    }
    fn clipboard_action(
        &mut self,
        window: &MainWindow,
        tab: Uuid,
        paste: bool,
    ) -> Result<(), String> {
        if paste {
            let text = self.clipboard.read()?;
            if text.len() > 4 * 1024 * 1024 {
                return Err("Paste exceeds the 4 MB limit".into());
            }
            let mode = self
                .surfaces
                .get(&tab)
                .map(|surface| surface.terminal.model().mode())
                .unwrap_or(0);
            let bytes = basalt::clipboard::paste_bytes(&text, mode);
            if let Some(surface) = self.surfaces.get(&tab) {
                surface.terminal.input(bytes).map_err(|e| e.to_string())?;
            } else if self.pending.contains(&tab) {
                let queued = self.pending_input.entry(tab).or_default();
                if queued.len().saturating_add(bytes.len()) <= 64 * 1024 {
                    queued.extend(bytes);
                } else {
                    return Err("Paste is too large while terminal starts".into());
                }
            } else {
                return Err("Terminal is not running".into());
            }
            self.selected_text.remove(&tab);
        } else {
            let text = if let Some(text) = self.selected_text.get(&tab) {
                text.clone()
            } else {
                let snapshot = self
                    .surfaces
                    .get(&tab)
                    .ok_or("Terminal is not running")?
                    .terminal
                    .model()
                    .snapshot();
                basalt::clipboard::selected_text(
                    &snapshot,
                    (0, 0),
                    (
                        snapshot.columns.saturating_sub(1),
                        snapshot.rows.saturating_sub(1),
                    ),
                )
            };
            self.clipboard.write(text)?;
        }
        self.clear_selection(window);
        Ok(())
    }
    fn select_terminal_text(&mut self, tab: Uuid, a: i32, b: i32, c: i32, d: i32) {
        if a < 0 || b < 0 || c < 0 || d < 0 {
            self.selected_text.remove(&tab);
            return;
        }
        if let Some(surface) = self.surfaces.get(&tab) {
            let text = basalt::clipboard::selected_text(
                &surface.terminal.model().snapshot(),
                (a as usize, b as usize),
                (c as usize, d as usize),
            );
            self.selected_text.insert(tab, text);
        }
    }
    fn application_appearance(&self) -> (i32, i32) {
        // Appearance is application-wide; legacy workspace overrides are ignored.
        (self.default_palette, self.default_font)
    }
    fn save_workspaces(&mut self) {
        if !self.save_dirty {
            return;
        }
        let Some(tx) = &self.save_tx else {
            return;
        };
        let bytes = serde_json::to_vec_pretty(
            &serde_json::json!({"version":1,"desktop":&self.desktop,"terminals":&self.launches}),
        );
        if let Ok(bytes) = bytes {
            if tx.try_send(bytes).is_ok() {
                self.save_dirty = false;
            }
        }
    }
    /// Create the chooser's terminal as a new pane of the current session, splitting it.
    fn open_chosen_terminal(&mut self) -> Option<Uuid> {
        let target = self.cli_context.map(CommandTarget::Pane).unwrap_or(CommandTarget::Focused);
        let tab = self.desktop.execute(Command::NewPaneAutoLayout, target);
        self.desktop.current_mut().last_active = unix_now();
        tab
    }
    /// Open a terminal beside the targeted pane, or as the first terminal of a new
    /// session when none is selected.
    fn open_terminal(&mut self, ui: &MainWindow, target: CommandTarget) {
        self.cli_context = match target {
            CommandTarget::Focused => None,
            CommandTarget::Pane(id) => Some(id),
        };
        if self.desktop.workspaces.is_empty() || self.desktop.selected.is_nil() {
            self.desktop.new_workspace();
        }
        self.active_workspaces.insert(self.desktop.selected);
        self.open_chosen_terminal();
        self.cli_context = None;
        self.ensure_terminals();
        self.refresh(ui);
    }
    fn update_pane_model(model: &VecModel<PaneItem>, items: Vec<PaneItem>) {
        // Keep view instances alive while geometry changes during a gesture.
        for (index, item) in items.into_iter().enumerate() {
            if index < model.row_count() {
                if model.row_data(index).as_ref() != Some(&item) {
                    model.set_row_data(index, item);
                }
            } else {
                model.push(item);
            }
        }
    }
    fn dispatch(&mut self, window: &MainWindow, command: Command, target: CommandTarget) {
        if command == Command::NewWorkspace {
            self.desktop.begin_session();
            self.open_terminal(window, CommandTarget::Focused);
            return;
        }
        if !self.fixture_shells
            && matches!(
                command,
                Command::NewPaneAutoLayout | Command::NewTerminalTab
            )
        {
            self.open_terminal(window, target);
            return;
        }
        let viewport = self.viewport;
        self.mutate(window, |desktop| {
            desktop.execute_at(command, target, viewport);
        });
    }
    fn drop_target(&self, x: f64, y: f64) -> Option<(Uuid, DropZone, DropPreview)> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let layout = self
            .desktop
            .visible_layout(self.viewport, 1.0, self.dock_width)?;
        let frame = layout.panes.iter().find(|frame| {
            x >= frame.bounds.x
                && y >= frame.bounds.y
                && x < frame.bounds.x + frame.bounds.width
                && y < frame.bounds.y + frame.bounds.height
        })?;
        let bounds = frame.bounds;
        let workspace = self.desktop.owner_of_pane(frame.id)?;
        if y < bounds.y + self.tabbar_height {
            let pane = workspace.projected_pane(frame.id)?;
            let geometry = self.tab_bounds.borrow();
            let index = pane
                .tabs
                .iter()
                .position(|tab| {
                    geometry
                        .get(&tab.id)
                        .is_some_and(|(left, width)| x < left + width / 2.0)
                })
                .unwrap_or(pane.tabs.len());
            let left = pane
                .tabs
                .get(index)
                .and_then(|tab| geometry.get(&tab.id).map(|(x, _)| *x))
                .or_else(|| {
                    pane.tabs
                        .last()
                        .and_then(|tab| geometry.get(&tab.id).map(|(x, width)| x + width))
                })
                .unwrap_or(bounds.x);
            return Some((
                frame.id,
                DropZone::Tab(index),
                DropPreview {
                    visible: true,
                    left: left as f32,
                    top: bounds.y as f32,
                    width: 2.0,
                    height: self.tabbar_height as f32,
                },
            ));
        }
        let zone = drop_zone(x - bounds.x, y - bounds.y, bounds.width, bounds.height);
        let mut preview = bounds;
        match zone {
            DropZone::Left => preview.width /= 2.0,
            DropZone::Right => {
                preview.x += preview.width / 2.0;
                preview.width /= 2.0;
            }
            DropZone::Top => preview.height /= 2.0,
            DropZone::Bottom => {
                preview.y += preview.height / 2.0;
                preview.height /= 2.0;
            }
            _ => {}
        }
        Some((
            frame.id,
            zone,
            DropPreview {
                visible: true,
                left: preview.x as f32,
                top: preview.y as f32,
                width: preview.width as f32,
                height: preview.height as f32,
            },
        ))
    }
    fn ensure_terminals(&mut self) {
        if !self.desktop.selected.is_nil() && self.desktop.dock_visible {
            if let Some(dock) = &self.desktop.dock {
                self.active_workspaces.insert(dock.id);
            }
        }
        let tabs: Vec<_> = self
            .desktop
            .containers()
            .filter(|workspace| self.active_workspaces.contains(&workspace.id))
            .flat_map(|workspace| {
                workspace.tree.pane_ids().into_iter().flat_map(|pane| {
                    workspace
                        .tree
                        .pane(pane)
                        .unwrap()
                        .tabs
                        .iter()
                        .map(|tab| tab.id)
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for id in tabs {
            if self.surfaces.contains_key(&id)
                || self.failures.contains_key(&id)
                || self.pending.contains(&id)
            {
                continue;
            }
            self.pending.push(id);
            // The session profile its terminal starts with (read once: changing the
            // profile later affects only terminals opened after that).
            let session_profile = self
                .desktop
                .containers()
                .find(|workspace| workspace.base_pane_for_tab(id).is_some())
                .and_then(|workspace| workspace.profile);
            let shared = session_profile
                .and_then(|profile| self.desktop.profiles.iter().find(|each| each.id == profile))
                .map(|profile| profile.shared())
                .unwrap_or_default();
            let project_cwd = self
                .desktop
                .containers()
                .find(|workspace| workspace.base_pane_for_tab(id).is_some())
                .and_then(|workspace| workspace.repository_path.clone())
                .unwrap_or_else(|| self.cwd.clone());
            let descriptor = self
                .launches
                .entry(id)
                .or_insert_with(|| basalt::workspace_store::TerminalLaunch {
                    tool: self.tab_tools.get(&id).cloned(),
                    profile: self
                        .tab_profiles
                        .get(&id)
                        .map(|p| p.as_str())
                        .unwrap_or("default")
                        .into(),
                    cwd: project_cwd,
                    conversation: None,
                    utility: None,
                })
                .clone();
            let cwd = descriptor.cwd.clone();
            let tx = self.tx.clone();
            let engine = self.engine.clone();
            let tool = self.tab_tools.get(&id).cloned();
            let profile = self
                .tab_profiles
                .get(&id)
                .cloned()
                .unwrap_or_else(ProfileId::default_profile);
            let cli_fixture = std::env::args()
                .any(|arg| {
                    matches!(
                        arg.as_str(),
                        "--verify-cli-catalog" | "--verify-workspace-storage"
                    )
                })
                .then(|| {
                    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("../target/debug/pty-probe.exe")
                });
            let state_file = self.bridge.as_ref().map(|bridge| bridge.path(id));
            let shell_state = self.shell_state(id);
            // Plain terminals run PowerShell with the session prompt; native smoke runs
            // keep cmd, whose scripted input they rely on.
            let verifying = std::env::args().any(|arg| arg.starts_with("--verify"));
            let powershell = (!verifying).then(basalt::shell::powershell).flatten();
            let init_script = self
                .bridge
                .as_ref()
                .filter(|_| powershell.is_none())
                .and_then(|bridge| bridge.cmd_init(id));
            let codex_proxy = self
                .bridge
                .as_ref()
                .filter(|_| powershell.is_some())
                .and_then(|bridge| bridge.codex_proxy(id));
            let fixture_mode = (cfg!(debug_assertions)
                && std::env::args().any(|arg| arg == "--verify-ui"))
            .then(|| {
                if std::env::args().any(|arg| arg == "--verify-glyphs") {
                    Some("glyphs")
                } else if std::env::args().any(|arg| arg == "--verify-titles") {
                    Some("titles")
                } else {
                    None
                }
            })
            .flatten();
            std::thread::spawn(move || {
                let result = (|| -> Result<_, Box<dyn std::error::Error>> {
                    if descriptor.utility.as_deref() == Some("lazygit") {
                        let program = engine.utility_program("lazygit")?;
                        let config = engine.utility_config("lazygit")?;
                        let terminal = NativeTerminal::spawn(SpawnRequest {
                            program,
                            arguments: vec![
                                "--use-config-dir".into(),
                                config.to_string_lossy().into_owned(),
                            ],
                            cwd,
                            environment: basalt::shell::host_environment(),
                            conpty_runtime: Some(
                                basalt::shell::conpty_runtime(),
                            ),
                            size: TerminalSize::new(100, 32, 9, 23)?,
                            history_limit: 10_000,
                            palette: screen::palette(),
                        })?;
                        return Ok((RuntimeTerminal::Shell(Arc::new(terminal)), None, None));
                    }
                    if let Some(tool) = tool {
                        return engine
                            .start_workspace_terminal(
                                &tool,
                                &profile,
                                id,
                                state_file,
                                cli_fixture,
                                descriptor.conversation,
                            )
                            .map(|(terminal, conversation, cwd)| {
                                (terminal, Some(conversation), Some(cwd))
                            })
                            .map_err(Into::into);
                    }
                    let program = if fixture_mode.is_some() {
                        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                            .join("../target/debug/pty-probe.exe")
                    } else if let Some(powershell) = &powershell {
                        powershell.clone()
                    } else {
                        PathBuf::from(std::env::var_os("COMSPEC").ok_or("COMSPEC is missing")?)
                    };
                    let terminal = NativeTerminal::spawn(SpawnRequest {
                        program,
                        conpty_runtime: Some(
                            basalt::shell::conpty_runtime(),
                        ),
                        arguments: if let Some(mode) = fixture_mode {
                            vec![mode.into()]
                        } else if powershell.is_some() {
                            basalt::shell::arguments(&basalt::shell::init_script(
                                codex_proxy.as_deref(),
                            ))
                        } else if let Some(init) = init_script {
                            vec!["/K".into(), init.to_string_lossy().into_owned()]
                        } else {
                            vec![]
                        },
                        cwd,
                        environment: {
                            let mut env = basalt::shell::host_environment();
                            env.remove("BASALT_AGENT_STATE_FILE");
                            // Programs choose their colors from these: true color, as
                            // every terminal of this kind announces it.
                            env.insert("TERM".into(), "xterm-256color".into());
                            env.insert("COLORTERM".into(), "truecolor".into());
                            env.insert("TERM_PROGRAM".into(), engine.identity().display_name.clone());
                            env.insert("TERM_PROGRAM_VERSION".into(), engine.identity().version.clone());
                            if let Some(profile) = session_profile {
                                let profile = profile.simple().to_string();
                                env.extend(engine.session_environment(&profile)?);
                                // Shared data a sibling changed arrives before the CLI reads it.
                                engine.synchronize_sharing(&profile, None, &shared, false)?;
                            }
                            if let Some(path) = state_file {
                                env.insert(
                                    "BASALT_AGENT_STATE_FILE".into(),
                                    path.to_string_lossy().into_owned(),
                                );
                            }
                            if let Some(path) = shell_state {
                                env.insert("BASALT_SHELL_STATE".into(), path.to_string_lossy().into_owned());
                            }
                            // Installed CLIs come first on PATH.
                            let path = env.iter().find(|(key, _)| key.eq_ignore_ascii_case("PATH")).map(|(key, value)| (key.clone(), value.clone()));
                            let (key, value) = path.unwrap_or_else(|| ("PATH".into(), String::new()));
                            env.insert(key, engine.tool_path(&value));
                            env
                        },
                        size: TerminalSize::new(80, 24, 9, 23)?,
                        history_limit: 10_000,
                        palette: screen::palette(),
                    })?;
                    Ok((RuntimeTerminal::Shell(Arc::new(terminal)), None, None))
                })()
                .map_err(|error| error.to_string());
                // An abandoned/closed UI must never leave a spawned shell running.
                if let Err(mpsc::SendError((_, Ok((terminal, _, _))))) = tx.send((id, result)) {
                    let _ = terminal.stop(Duration::from_millis(100));
                }
            });
        }
    }
    /// The current session's profile changed: its plain terminals waiting at a prompt
    /// start again with the new profile's accounts, in the folder they were in. Ones
    /// running a program keep going as they are; how many is returned.
    fn restart_idle_shells(&mut self) -> usize {
        let tabs: Vec<Uuid> = self
            .desktop
            .current()
            .tree
            .pane_ids()
            .into_iter()
            .filter_map(|pane| self.desktop.current().tree.pane(pane).map(|pane| pane.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>()))
            .flatten()
            .filter(|tab| self.surfaces.contains_key(tab) && !self.tab_tools.contains_key(tab))
            .collect();
        let mut busy = 0;
        for tab in tabs {
            let idle = self
                .grids
                .as_ref()
                .is_some_and(|grids| grids.borrow().screen(&tab).and_then(basalt::command_line::read).is_some());
            if !idle {
                busy += 1;
                continue;
            }
            if let (Some(launch), Some((_, folder))) = (self.launches.get_mut(&tab), self.folders.get(&tab)) {
                if std::path::Path::new(folder).is_dir() {
                    launch.cwd = PathBuf::from(folder);
                }
            }
            if let Some(grids) = &self.grids {
                grids.borrow_mut().remove(&tab);
            }
            self.failures.remove(&tab);
            self.running.remove(&tab);
            if let Some(surface) = self.surfaces.remove(&tab) {
                std::thread::spawn(move || {
                    let _ = surface.terminal.stop(Duration::from_millis(200));
                });
            }
        }
        self.ensure_terminals();
        busy
    }
    fn retire(&mut self, ids: impl IntoIterator<Item = Uuid>) {
        for id in ids {
            self.launches.remove(&id);
            self.save_dirty = true;
            self.pending_input.remove(&id);
            self.tab_bounds.borrow_mut().remove(&id);
            self.failures.remove(&id);
            self.signals.remove(&id);
            self.selected_text.remove(&id);
            self.agent_clock.remove(&id);
            self.screens.forget(id);
            self.screen_states.remove(&id);
            self.agent_turns.remove(&id);
            self.signal_changed.remove(&id);
            self.signal_invalidated.remove(&id);
            self.tab_tools.remove(&id);
            self.tab_profiles.remove(&id);
            if let Some(grids) = &self.grids {
                grids.borrow_mut().remove(&id);
            }
            if let Some(surface) = self.surfaces.remove(&id) {
                std::thread::spawn(move || {
                    let _ = surface.terminal.stop(Duration::from_millis(200));
                });
            }
        }
    }
    /// Terminal frames and pane geometry: everything terminal output changes. False
    /// when nothing is laid out.
    fn refresh_panes(&mut self, window: &MainWindow) -> bool {
        let Some(layout) = self.visible_layout(f64::from(window.window().scale_factor())) else {
            return false;
        };
        self.panes_refreshed = std::time::Instant::now();
        let scale = f64::from(window.window().scale_factor());
        // On the GPU the font's whole-pixel cell decides the grid.
        let mut grid_cell = None;
        if let Some(grids) = &self.grids {
            let mut grids = grids.borrow_mut();
            let font_size = window.global::<basalt::TerminalStyle>().get_font_size() * scale as f32;
            if grids.set_font(
                &window.global::<basalt::Appearance>().get_mono(),
                basalt::gpu::FontSize::from_px(font_size),
            ) {
                for surface in self.surfaces.values_mut() {
                    surface.revision = u64::MAX;
                }
            }
            let (width, height) = grids.cell();
            grid_cell = Some((f64::from(width) / scale, f64::from(height) / scale));
        }
        let background = {
            let color = window.global::<basalt::Theme>().get_terminal();
            [color.red(), color.green(), color.blue()]
        };
        let mut drew = false;
        let mut command_lines = Vec::new();
        let mut panes = Vec::new();
        let mut dock_panes = Vec::new();
        for frame in &layout.panes {
            let workspace = self.desktop.owner_of_pane(frame.id).unwrap();
            let pane = workspace.projected_pane(frame.id).unwrap();
            if pane.tabs.is_empty() {
                continue;
            }
            let focused = frame.id == self.desktop.focused_container().focused_view_pane();
            let mut terminal_frame = TerminalFrame::default();
            let mut status = String::new();
            if let Some(id) = pane.selected {
                if let Some(surface) = self.surfaces.get_mut(&id) {
                    let cell_width = f64::from(
                        window
                            .global::<basalt::TerminalStyle>()
                            .get_cell_width(),
                    )
                    .max(1.0);
                    let cell_height = f64::from(
                        window
                            .global::<basalt::TerminalStyle>()
                            .get_cell_height(),
                    )
                    .max(1.0);
                    let inner = basalt::viewport::panel_inner_rect(frame.bounds);
                    let (cell_width, cell_height) = grid_cell.unwrap_or((cell_width, cell_height));
                    let typography = basalt::typography::PaneTypography {
                        font_size: f64::from(
                            window
                                .global::<basalt::TerminalStyle>()
                                .get_font_size(),
                        ),
                        cell_width,
                        cell_height,
                    };
                    // Explicit terminal magnification is uniform across panes.
                    // Smaller panes wrap/reduce their grid instead of shrinking the font.
                    let requested = basalt::viewport::terminal_size_with_header(
                        inner,
                        typography.cell_width,
                        typography.cell_height,
                        f64::from(window.window().scale_factor()),
                        0.0,
                    );
                    if let Some(size) = requested {
                        if surface.size == size {
                            surface.pending_size = None;
                        } else {
                            let now = std::time::Instant::now();
                            let settled = match surface.pending_size {
                                Some((pending, since)) if pending == size => now.duration_since(since) >= RESIZE_SETTLE,
                                _ => {
                                    surface.pending_size = Some((size, now));
                                    false
                                }
                            };
                            // A fresh terminal takes its first grid immediately.
                            if (settled || !surface.sized) && surface.terminal.resize(size).is_ok() {
                                surface.size = size;
                                surface.sized = true;
                                surface.pending_size = None;
                                surface.revision = u64::MAX;
                            }
                        }
                    }
                    let revision = surface.terminal.model().revision();
                    let look = basalt::gpu::Look {
                        background,
                        cursor: screen::cursor_rgb(),
                        cursor_height: (2.0 * scale).round() as f32,
                        focused,
                    };
                    if revision != surface.revision
                        || surface.focused != focused
                        || (self.grids.is_some() && surface.look != Some(look))
                    {
                        let model = surface.terminal.model();
                        if let Some(grids) = &self.grids {
                            let mut grids = grids.borrow_mut();
                            let image = grids.update(id, |screen| model.read_screen(screen), look);
                            let line = grids.screen(&id).and_then(basalt::command_line::read);
                            if let Some(screen) = grids.screen(&id) {
                                surface.frame = screen::screen_frame(screen, focused);
                            }
                            command_lines.push((id, focused, line));
                            surface.frame.grid = image.unwrap_or_default();
                            surface.look = Some(look);
                            drew = true;
                        } else {
                            surface.frame = surface.renderer.update(|screen| model.read_screen(screen), focused);
                        }
                        surface.revision = revision;
                        surface.focused = focused;
                    } else if let Some(grids) = &self.grids {
                        // The same content, but what is drawn over its command line may
                        // have changed: an offer arrived, or a list selection moved.
                        let line = grids.borrow().screen(&id).and_then(basalt::command_line::read);
                        command_lines.push((id, focused, line));
                    }
                    surface.frame.font_size = typography.font_size as f32;
                    surface.frame.cell_width = typography.cell_width as f32;
                    surface.frame.cell_height = typography.cell_height as f32;
                    if requested.is_some() {
                        terminal_frame = surface.frame.clone();
                    } else {
                        status = "Enlarge this pane to show the terminal".into();
                    }
                    if let Some(exit) = surface.terminal.exit_summary() {
                        surface.exited = true;
                        status = exit.error.unwrap_or_else(|| {
                            format!("Process exited ({})", exit.exit_code.unwrap_or(0))
                        });
                    }
                } else {
                    status = self
                        .failures
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| "Starting terminal…".into());
                }
            }
            let item = PaneItem {
                zoomed: workspace.zoomed == Some(frame.id),
                selected_title: pane
                    .tabs
                    .iter()
                    .find(|tab| Some(tab.id) == pane.selected)
                    .map(|tab| tab.compact_title().into())
                    .unwrap_or_default(),
                selected_tab: pane
                    .selected
                    .map(|id| id.to_string().into())
                    .unwrap_or_default(),
                full_title: pane
                    .selected_tab()
                    .map(|tab| tab.detail_title().into())
                    .unwrap_or_default(),
                selected_pinned: pane
                    .tabs
                    .iter()
                    .find(|tab| Some(tab.id) == pane.selected)
                    .is_some_and(|tab| tab.pinned),
                id: frame.id.to_string().into(),
                left: frame.bounds.x as f32,
                top: frame.bounds.y as f32,
                width: frame.bounds.width as f32,
                height: frame.bounds.height as f32,
                tabs: basalt::projection::sync(
                    (if self
                        .desktop
                        .dock
                        .as_ref()
                        .is_some_and(|dock| dock.id == workspace.id)
                    {
                        self.dock_pane_model.row_data(dock_panes.len())
                    } else {
                        self.pane_model.row_data(panes.len())
                    })
                    .map(|p| p.tabs)
                    .unwrap_or_default(),
                    pane.tabs
                        .iter()
                        .map(|tab| TabItem {
                            id: tab.id.to_string().into(),
                            title: tab.compact_title().into(),
                            selected: pane.selected == Some(tab.id),
                            pinned: tab.pinned,
                            tool: self.tab_tool(tab.id).into(),
                        })
                        .collect::<Vec<_>>(),
                ),
                frame: terminal_frame,
                focused,
                status: status.into(),
            };
            if self
                .desktop
                .dock
                .as_ref()
                .is_some_and(|dock| dock.id == workspace.id)
            {
                dock_panes.push(item);
            } else {
                panes.push(item);
            }
        }
        let pane_count = panes.len();
        let dock_count = dock_panes.len();
        Self::update_pane_model(&self.pane_model, panes);
        while self.pane_model.row_count() > pane_count {
            self.pane_model.remove(pane_count);
        }
        Self::update_pane_model(&self.dock_pane_model, dock_panes);
        while self.dock_pane_model.row_count() > dock_count {
            self.dock_pane_model.remove(dock_count);
        }
        window.set_empty_workspace(pane_count == 0);
        window.set_has_workspace(!self.desktop.selected.is_nil());
        window.set_panes(ModelRc::from(self.pane_model.clone()));
        window.set_dock_panes(ModelRc::from(self.dock_pane_model.clone()));
        window.set_dividers(basalt::projection::sync(
            window.get_dividers(),
            layout
                .dividers
                .iter()
                .filter(|divider| divider.bounds.x < self.viewport.x + self.viewport.width + 1.0)
                .map(|divider| DividerItem {
                    id: divider.id.to_string().into(),
                    left: divider.bounds.x as f32,
                    top: divider.bounds.y as f32,
                    width: divider.bounds.width as f32,
                    height: divider.bounds.height as f32,
                    horizontal: divider.orientation == Orientation::Horizontal,
                })
                .collect::<Vec<_>>(),
        ));
        window.set_dock_dividers(basalt::projection::sync(
            window.get_dock_dividers(),
            layout
                .dividers
                .iter()
                .filter(|divider| divider.bounds.x >= self.viewport.x + self.viewport.width + 1.0)
                .map(|divider| DividerItem {
                    id: divider.id.to_string().into(),
                    left: divider.bounds.x as f32,
                    top: divider.bounds.y as f32,
                    width: divider.bounds.width as f32,
                    height: divider.bounds.height as f32,
                    horizontal: divider.orientation == Orientation::Horizontal,
                })
                .collect::<Vec<_>>(),
        ));
        let colors = {
            let theme = window.global::<basalt::Theme>();
            let rgb = |color: slint::Color| [color.red(), color.green(), color.blue()];
            (
                rgb(theme.get_faint()),
                rgb(theme.get_amber()),
                rgb(theme.get_danger()),
                [rgb(theme.get_selected_text()), rgb(theme.get_muted()), rgb(theme.get_accent())],
            )
        };
        for (id, focused, line) in command_lines {
            let overlay = self.command_overlay(id, focused, line, colors);
            if let Some(grids) = &self.grids {
                drew |= grids.borrow_mut().set_overlay(&id, overlay);
            }
        }
        // A grid drawn into the same texture is not a property change Slint can see.
        if drew {
            window.window().request_redraw();
        }
        true
    }
    fn refresh(&mut self, window: &MainWindow) {
        let (palette, font) = self.application_appearance();
        if window.global::<basalt::Theme>().get_palette() != palette {
            window
                .global::<basalt::Theme>()
                .set_palette(palette);
            synchronize_native_frame(window);
        }
        window
            .global::<basalt::Appearance>()
            .set_font_percent(font);
        window.global::<basalt::Appearance>().set_terminal_font_percent(self.default_terminal_font);
        self.save_dirty = true;
        self.tabbar_height = f64::from(
            window
                .global::<basalt::Metrics>()
                .get_tabbar_height(),
        );
        window
            .window()
            .with_winit_window(|native| window.set_window_maximized(native.is_maximized()));
        window.set_dock_visible(self.desktop.dock_visible && !self.desktop.selected.is_nil());
        window.set_dock_focused(self.desktop.dock_focused);
        window.set_canvas_mode(
            self.desktop.current().layout_mode == basalt::canvas::LayoutMode::Canvas,
        );
        if !self.refresh_panes(window) {
            return;
        }
        let now = std::time::Instant::now();
        let wall = unix_now();
        let mut items = Vec::with_capacity(self.desktop.workspaces.len());
        let titles = self.desktop.session_titles();
        for (item, (title, placeholder)) in self.desktop.workspaces.iter().zip(titles) {
            let folder = item
                .selected_tab()
                .and_then(|id| self.launches.get(&id))
                .map(|launch| launch.cwd.clone())
                .or_else(|| item.repository_path.clone())
                .unwrap_or_else(|| self.cwd.clone());
            let branch = match self.branches.get(&folder) {
                Some((checked, branch)) if now.duration_since(*checked) < Duration::from_secs(3) => branch.clone(),
                _ => {
                    let branch = basalt::git::branch(&folder).unwrap_or_default();
                    self.branches.insert(folder.clone(), (now, branch.clone()));
                    branch
                }
            };
            let first_tab = item
                .tree
                .pane_ids()
                .into_iter()
                .filter_map(|id| item.tree.pane(id))
                .find_map(|pane| pane.tabs.first().map(|tab| tab.id));
            items.push(WorkspaceItem {
                id: item.id.to_string().into(),
                title: title.into(),
                placeholder,
                directory: folder.display().to_string().into(),
                age: age_label(item.last_active, wall).into(),
                tool: first_tab.map(|id| self.tab_tool(id)).unwrap_or_default().into(),
                branch: branch.into(),
                profile: item
                    .profile
                    .and_then(|id| self.desktop.profiles.iter().find(|profile| profile.id == id))
                    .map(|profile| profile.title())
                    .unwrap_or_else(|| "default".into())
                    .into(),
            });
        }
        window.set_workspaces(basalt::projection::sync(window.get_workspaces(), items));
        window.set_selected_workspace(self.desktop.selected.to_string().into());
        // The profiles box: default (the user's own setup) first, then each profile with
        // how many sessions use it and which CLIs have stored anything in it.
        let in_use = |profile: Option<Uuid>| {
            self.desktop.workspaces.iter().filter(|workspace| workspace.profile == profile).count() as i32
        };
        let mut profile_items = vec![basalt::SessionProfileItem {
            id: "".into(),
            name: "default".into(),
            sessions: in_use(None),
            ..Default::default()
        }];
        for profile in &self.desktop.profiles {
            let tools = match self.profile_tools.get(&profile.id) {
                Some((checked, tools)) if now.duration_since(*checked) < Duration::from_secs(5) => tools.clone(),
                _ => {
                    let folder = profile.id.simple().to_string();
                    let tools: Vec<_> = self
                        .engine
                        .profile_tools(&folder)
                        .into_iter()
                        .map(|tool| {
                            let label = contents_label(self.engine.profile_contents(&folder, &tool));
                            (tool, label)
                        })
                        .collect();
                    self.profile_tools.insert(profile.id, (now, tools.clone()));
                    tools
                }
            };
            profile_items.push(basalt::SessionProfileItem {
                id: profile.id.to_string().into(),
                name: profile.title().into(),
                sessions: in_use(Some(profile.id)),
                tools: ModelRc::new(VecModel::from(
                    tools
                        .iter()
                        .map(|(tool, detail)| basalt::ProfileTool { id: tool.into(), detail: detail.into() })
                        .collect::<Vec<_>>(),
                )),
                share_mcp: profile.share_mcp,
                share_skills: profile.share_skills,
            });
        }
        self.profile_tools.retain(|id, _| self.desktop.profiles.iter().any(|profile| profile.id == *id));
        window.set_session_profiles(basalt::projection::sync(window.get_session_profiles(), profile_items));
        let current = self
            .desktop
            .session_profile()
            .and_then(|id| self.desktop.profiles.iter().find(|profile| profile.id == id));
        window.set_session_profile_id(current.map(|profile| profile.id.to_string()).unwrap_or_default().into());

        window.set_workspace_title(if self.desktop.selected.is_nil() {
            "".into()
        } else {
            self.desktop.current().session_title().into()
        });
        self.refresh_agents(window);
    }
    fn refresh_agents(&mut self, window: &MainWindow) {
        self.agents_refreshed = std::time::Instant::now();
        let now = self.agents_refreshed;
        // Terminals on screen while the window has focus count as looked at.
        let focused = window.window().with_winit_window(|native| native.has_focus()).unwrap_or(true);
        let mut viewed = Vec::new();
        if focused {
            let dock = self.desktop.dock.as_ref().filter(|_| self.desktop.dock_visible);
            for workspace in std::iter::once(self.desktop.current()).chain(dock) {
                if workspace.layout_mode == basalt::canvas::LayoutMode::Canvas {
                    viewed.extend(workspace.canvas.panes.iter().map(|pane| pane.selected));
                } else {
                    viewed.extend(workspace.tree.pane_ids().into_iter().filter_map(|pane| workspace.tree.pane(pane)?.selected));
                }
            }
        }
        let mut agents = vec![];
        for workspace in self.desktop.containers() {
            for pane_id in workspace.tree.pane_ids() {
                let pane = workspace.tree.pane(pane_id).unwrap();
                for tab in &pane.tabs {
                    let tool = self
                        .tab_tools
                        .get(&tab.id)
                        .and_then(|id| self.engine.catalog().get(id).ok());
                    let activity = self.activities.get(&tab.id);
                    if tool.is_none() && activity.is_none() {
                        continue;
                    }
                    let name = tool
                        .map(|tool| tool.name.as_str())
                        .or_else(|| activity.map(|a| a.name.as_str()))
                        .unwrap_or("CLI");
                    let provider = tool.map(|tool| tool.id.as_str()).or_else(|| activity.map(|a| a.id.as_str())).unwrap_or(name);
                    let signal = self
                        .signals
                        .get(&tab.id)
                        .filter(|signal| signal.provider.eq_ignore_ascii_case(provider));
                    let failed = self.failures.contains_key(&tab.id);
                    let exited = self
                        .surfaces
                        .get(&tab.id)
                        .is_some_and(|surface| surface.exited);
                    // Two layers: what the screen shows plainly (work, a question,
                    // the agent's own idle prompt) decides; the lifecycle signal
                    // decides where the screen shows nothing either way, and a
                    // signal from the last moments bridges the screen catching up.
                    let fresh = self
                        .signal_changed
                        .get(&tab.id)
                        .is_some_and(|changed| now.saturating_duration_since(*changed) < Duration::from_secs(3));
                    let base = if self.pending.contains(&tab.id) {
                        7
                    } else if failed || exited {
                        basalt::agent_summary::state(signal, exited, failed)
                    } else {
                        use basalt::agent_screen::State;
                        let hook = signal.map(|signal| signal.state.as_str());
                        let screen = self.screen_states.get(&tab.id);
                        let unsure = screen.is_none_or(|reading| reading.plain);
                        match screen.map(|reading| reading.state) {
                            Some(State::Blocked) => 3,
                            Some(State::Working) => 1,
                            _ if hook == Some("waiting") && (fresh || unsure) => 3,
                            _ if hook == Some("working") && (fresh || unsure) => 1,
                            _ if hook == Some("error") => 5,
                            _ => 0,
                        }
                    };
                    // A turn that ends while you look elsewhere stays done until you look.
                    let looking = viewed.contains(&tab.id);
                    let (before, done) = self.agent_turns.entry(tab.id).or_insert((base, false));
                    if base != 0 || looking {
                        *done = false;
                    } else if *before == 1 {
                        *done = true;
                    }
                    *before = base;
                    let state = if base == 0 && *done { 4 } else { base };
                    let clock = self.agent_clock.entry(tab.id).or_default();
                    clock.update(state, self.agents_refreshed);
                    let elapsed = clock.label(self.agents_refreshed);
                    agents.push(basalt::AgentItem {
                        workspace: if self
                            .desktop
                            .dock
                            .as_ref()
                            .is_some_and(|dock| dock.id == workspace.id)
                        {
                            self.desktop.selected
                        } else {
                            workspace.id
                        }
                        .to_string()
                        .into(),
                        pane: if workspace.layout_mode
                            == basalt::canvas::LayoutMode::Canvas
                        {
                            workspace
                                .canvas
                                .panes
                                .iter()
                                .find(|group| group.panels.contains(&tab.id))
                                .map(|group| group.id)
                                .unwrap_or(pane_id)
                        } else {
                            pane_id
                        }
                        .to_string()
                        .into(),
                        tab: tab.id.to_string().into(),
                        name: name.into(),
                        detail: elapsed.into(),
                        workspace_title: workspace.title.clone().into(),
                        running: state == 1,
                        state,
                        active: Some(tab.id) == self.desktop.focused_container().selected_tab(),
                    });
                }
            }
        }
        let waiting = agents.iter().filter(|agent| agent.state == 3).count();
        let errors = agents.iter().filter(|agent| agent.state == 5).count();
        let running = agents.iter().filter(|agent| agent.state == 1).count();
        window.set_agent_summary(
            if waiting > 0 {
                format!("{waiting} waiting")
            } else if errors > 0 {
                format!("{errors} errors")
            } else if running > 0 {
                format!("{running} working")
            } else {
                String::new()
            }
            .into(),
        );
        if window.get_agents().iter().collect::<Vec<_>>() != agents {
            window.set_agents(basalt::projection::sync(
                window.get_agents(),
                agents,
            ));
        }
    }
    /// Catalog id of a terminal's CLI, "_lazygit" for utilities, or "" for a shell.
    fn tab_tool(&self, tab: Uuid) -> String {
        self.tab_tools
            .get(&tab)
            .cloned()
            .or_else(|| {
                self.launches
                    .get(&tab)
                    .and_then(|launch| launch.utility.as_ref())
                    .map(|utility| format!("_{utility}"))
            })
            .unwrap_or_default()
    }
    fn mutate(&mut self, window: &MainWindow, action: impl FnOnce(&mut Desktop)) {
        action(&mut self.desktop);
        if !self.desktop.selected.is_nil() {
            self.active_workspaces.insert(self.desktop.selected);
            self.desktop.current_mut().last_active = unix_now();
        }
        self.ensure_terminals();
        self.refresh(window);
    }
}
fn main() {
    // A CLI's hook: note the event and leave at once, before anything else starts.
    let arguments: Vec<String> = std::env::args().collect();
    if let Some(at) = arguments.iter().position(|argument| argument == "--agent-hook") {
        let provider = arguments.get(at + 1).map_or("", String::as_str);
        std::process::exit(basalt::agent_bridge::run_hook(provider));
    }
    if let Err(error) = run() {
        eprintln!("{error}");
        #[cfg(windows)]
        if !std::env::args().any(|arg| arg.starts_with("--verify")) {
            show_fatal_error(&error.to_string());
        }
        std::process::exit(1);
    }
}
#[cfg(windows)]
fn show_fatal_error(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let wide = |text: &str| {
        text.encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let text = wide(message);
    let caption = wide("Startup error");
    // SAFETY: both buffers are NUL-terminated UTF-16 and outlive the call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let software_baseline = std::env::args().any(|a| a == "--verify-responsiveness")
        && std::env::args().any(|a| a == "--software-renderer-baseline");
    let selector = slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name(
            if software_baseline {
                "skia-software"
            } else {
                "skia"
            }
            .into(),
        );
    let selector = if software_baseline {
        selector
    } else {
        // Skia on the same Direct3D 12 device the terminal grid renders with. The grid
        // blends glyphs per subpixel and its textures can span a whole wide screen.
        let mut settings = slint::wgpu_26::WGPUSettings::default();
        settings.device_required_features = wgpu::Features::DUAL_SOURCE_BLENDING;
        settings.device_required_limits = wgpu::Limits::default();
        selector.require_wgpu_26(slint::wgpu_26::WGPUConfiguration::Automatic(settings))
    };
    selector
        .with_winit_window_attributes_hook(|attributes| attributes.with_transparent(true))
        .select()?;
    basalt::typography::embed_font();
    let window = MainWindow::new()?;
    window
        .global::<basalt::Appearance>()
        .set_mono(basalt::typography::preferred_mono().into());
    let identity = basalt_core::domain::ApplicationIdentity::from_toml(include_str!(
        "../../application.toml"
    ))?;
    let smoke = std::env::args().any(|arg| arg.starts_with("--verify"));
    let folders = std::env::var_os("LOCALAPPDATA")
        .filter(|_| !smoke)
        .map(|local| basalt::folders::Folders::under(std::path::Path::new(&local), &identity.application_id));
    let preference_path = if smoke {
        std::env::args()
            .position(|arg| arg == "--verify-preferences")
            .and_then(|index| std::env::args().nth(index + 1))
            .map(PathBuf::from)
    } else {
        folders.as_ref().map(|folders| folders.settings.clone())
    };
    let preferences = preference_path.clone().and_then(
        |path| match basalt_core::storage::DataRoot::new(path) {
            Ok(root) => Some(basalt::preferences::AppearancePreferences::new(
                root,
            )),
            Err(error) => {
                window.set_message(format!("Appearance preferences unavailable: {error}").into());
                None
            }
        },
    );
    if let Some(settings) = &preferences {
        match settings.font_percent() {
            Ok(percent) => window
                .global::<basalt::Appearance>()
                .set_font_percent(percent),
            Err(error) => window
                .set_message(format!("Appearance preferences could not be loaded: {error}").into()),
        }
    }
    if let Some(settings) = &preferences {
        match settings.color_palette() {
            Ok(palette) => window
                .global::<basalt::Theme>()
                .set_palette(palette),
            Err(error) => window.set_message(format!("Color could not be loaded: {error}").into()),
        }
    }
    if let Some(settings) = &preferences {
        match settings.terminal_font_percent() {
            Ok(percent) => window.global::<basalt::Appearance>().set_terminal_font_percent(percent),
            Err(error) => window.set_message(format!("Terminal font could not be loaded: {error}").into()),
        }
    }
    window.set_app_title(identity.display_name.into());
    let (tx, rx) = mpsc::channel();
    let (help_tx, help_rx) = mpsc::channel::<(String, basalt::flags::Help)>();
    let (install_tx, install_rx) = mpsc::channel::<InstallEvent>();
    let bridge_root = if smoke {
        std::env::temp_dir().join(format!("basalt-agent-test-{}", Uuid::new_v4()))
    } else {
        folders
            .as_ref()
            .map_or_else(std::env::temp_dir, |folders| folders.settings.clone())
            .join("agent-sessions")
            .join(Uuid::new_v4().to_string())
    };
    let backend_root = if std::env::args().any(|arg| arg == "--verify-workspace-storage") {
        preferences
            .as_ref()
            .ok_or("Isolated workspace test preferences are required")?
            .data_root()
            .path()
            .join("backend-fixture")
    } else if smoke {
        std::env::temp_dir().join(format!("basalt-engine-test-{}", Uuid::new_v4()))
    } else {
        folders
            .as_ref()
            .map_or_else(|| std::env::temp_dir().join(&identity.application_id), |folders| folders.data.clone())
    };
    let engine = Arc::new(Engine::new(
        basalt_core::storage::DataRoot::new(backend_root)?,
        basalt_core::domain::ApplicationIdentity::from_toml(include_str!(
            "../../application.toml"
        ))?,
    ));
    let controller = Rc::new(RefCell::new(Controller {
        default_palette: window.global::<basalt::Theme>().get_palette(),
        default_font: window
            .global::<basalt::Appearance>()
            .get_font_percent(),
        default_terminal_font: window.global::<basalt::Appearance>().get_terminal_font_percent(),
        selected_text: HashMap::new(),
        clipboard: basalt::clipboard::Clipboard::new(smoke),
        agent_clock: HashMap::new(),
        agents_refreshed: std::time::Instant::now(),
        preferences,
        self_weak: std::rc::Weak::new(),
        pane_model: Rc::new(VecModel::default()),
        dock_pane_model: Rc::new(VecModel::default()),
        desktop: if smoke && !std::env::args().any(|arg| arg == "--verify-workspace-storage") {
            Desktop::demo()
        } else {
            Desktop::default()
        },
        launches: std::collections::BTreeMap::new(),
        active_workspaces: std::collections::HashSet::new(),
        save_tx: None,
        save_dirty: false,
        surfaces: HashMap::new(),
        activities: HashMap::new(),
        screens: Default::default(),
        screen_states: HashMap::new(),
        agent_turns: HashMap::new(),
        signal_changed: HashMap::new(),
        signals: HashMap::new(),
        signal_invalidated: HashMap::new(),
        bridge: basalt::agent_bridge::Bridge::new(bridge_root).ok(),
        engine,
        tab_tools: HashMap::new(),
        tab_profiles: HashMap::new(),
        cli_context: None,
        branches: HashMap::new(),
        profile_tools: HashMap::new(),
        fixture_shells: smoke,
        failures: HashMap::new(),
        pending: vec![],
        pending_input: HashMap::new(),
        viewport: Rect {
            x: 0.0,
            y: 0.0,
            width: 859.0,
            height: 692.0,
        },
        dock_width: 320.0,
        tabbar_height: 28.0,
        tab_bounds: Rc::new(RefCell::new(HashMap::new())),
        cwd: std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .filter(|home| home.is_dir())
            .map_or_else(std::env::current_dir, Ok)?,
        tx,
        grids: if software_baseline {
            None
        } else {
            basalt::gpu::Grids::new(
                &window.global::<basalt::Appearance>().get_mono(),
                basalt::gpu::FontSize::from_px(
                    window.global::<basalt::TerminalStyle>().get_font_size() * window.window().scale_factor(),
                ),
            )
            .ok()
            .map(|grids| Rc::new(RefCell::new(grids)))
        },
        gpu_setup: Rc::new(RefCell::new(GpuSetup::Waiting)),
        histories: HashMap::new(),
        history_folder: preference_path.as_ref().map(|path| path.join("history")),
        suggestions: HashMap::new(),
        running: HashMap::new(),
        folders: HashMap::new(),
        history_search: None,
        history_query: String::new(),
        history_here: false,
        completion: None,
        helps: HashMap::new(),
        help_tx,
        install: None,
        install_tx,
        panes_refreshed: std::time::Instant::now(),
        panes_scheduled: false,
        frame_interval: Duration::from_micros(1_000_000 / 180),
    }));
    {
        // Grids are drawn right before Slint renders a frame, so output that arrives
        // between frames costs one draw; while anything moves, every frame comes.
        let setup = controller.borrow().gpu_setup.clone();
        let grids = controller.borrow().grids.clone();
        let weak = window.as_weak();
        let _ = window.window().set_rendering_notifier(move |state, api| match state {
            slint::RenderingState::RenderingSetup => {
                *setup.borrow_mut() = match api {
                    slint::GraphicsAPI::WGPU26 { device, queue, .. }
                        if device.features().contains(wgpu::Features::DUAL_SOURCE_BLENDING) =>
                    {
                        GpuSetup::Device(device.clone(), queue.clone())
                    }
                    _ => GpuSetup::Unavailable,
                };
            }
            slint::RenderingState::BeforeRendering => {
                let moving = grids
                    .as_ref()
                    .and_then(|grids| grids.try_borrow_mut().ok())
                    .is_some_and(|mut grids| grids.render(std::time::Instant::now()));
                if moving {
                    if let Some(window) = weak.upgrade() {
                        window.window().request_redraw();
                    }
                }
            }
            _ => {}
        });
    }
    // Shims for CLIs installed in their own folders, so every terminal finds them.
    {
        let engine = controller.borrow().engine.clone();
        std::thread::spawn(move || engine.refresh_shims());
    }
    // Terminal output wakes the panes at once instead of waiting for the next tick.
    {
        let (state, weak) = (Rc::downgrade(&controller), window.as_weak());
        WAKE.with(|wake| {
            *wake.borrow_mut() = Some(Box::new(move || {
                if let (Some(state), Some(window)) = (state.upgrade(), weak.upgrade()) {
                    if let Ok(mut owner) = state.try_borrow_mut() {
                        owner.paced_refresh(&window);
                    }
                }
            }));
        });
    }
    controller.borrow_mut().self_weak = Rc::downgrade(&controller);
    if smoke && !controller.borrow().desktop.selected.is_nil() {
        let id = controller.borrow().desktop.selected;
        controller.borrow_mut().active_workspaces.insert(id);
    }
    let (save_errors_tx, save_errors_rx) = mpsc::channel();
    let store_root = controller
        .borrow()
        .preferences
        .as_ref()
        .map(|settings| settings.data_root());
    let save_worker = if let Some(root) = store_root {
        if !smoke || std::env::args().any(|arg| arg == "--verify-workspace-storage") {
            match basalt::workspace_store::WorkspaceDocument::load(&root) {
                Ok(Some(document)) => {
                    let mut owner = controller.borrow_mut();
                    owner.desktop = document.desktop;
                    owner.launches = document.terminals;
                    owner.active_workspaces.clear();
                    if !owner.desktop.selected.is_nil() {
                        let selected = owner.desktop.selected;
                        owner.active_workspaces.insert(selected);
                    }
                    for (id, launch) in owner.launches.clone() {
                        if let Some(tool) = launch.tool {
                            owner.tab_tools.insert(id, tool);
                        }
                        if let Ok(profile) = ProfileId::try_from(launch.profile) {
                            owner.tab_profiles.insert(id, profile);
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if let Some(bytes) = root.read_limited("workspaces.json", 8 * 1024 * 1024)? {
                        root.write(
                            format!("workspaces.rejected-{}.json", Uuid::new_v4()),
                            &bytes,
                        )?;
                    }
                    window.set_message(
                        format!("Workspace file was preserved for recovery: {error}").into(),
                    );
                }
            }
            let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(1);
            controller.borrow_mut().save_tx = Some(tx);
            Some(std::thread::spawn(move || {
                while let Ok(bytes) = rx.recv() {
                    if let Err(error) = root.write("workspaces.json", &bytes) {
                        let _ = save_errors_tx.send(error.to_string());
                    }
                }
            }))
        } else {
            None
        }
    } else {
        None
    };
    let save_timer = slint::Timer::default();
    {
        let state = controller.clone();
        let weak = window.as_weak();
        save_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(1),
            move || {
                if let Some(window) = weak.upgrade() {
                    while let Ok(error) = save_errors_rx.try_recv() {
                        window.set_message(format!("Workspace save failed: {error}").into());
                    }
                }
                state.borrow_mut().save_workspaces();
            },
        );
    }
    {
        let state = Rc::downgrade(&controller);
        window.window().on_close_requested(move || {
            let _ = state.upgrade();
            let _ = slint::quit_event_loop();
            slint::CloseRequestResponse::HideWindow
        });
    }
    // A callback can arrive while the controller is busy: a popup shown from a key
    // handler calls back from its init. It then runs right after, never panics.
    macro_rules! bind {
        ($method:ident, |$owner:ident, $ui:ident $(, $arg:ident)*| $body:block) => {{
            let state = controller.clone(); let weak = window.as_weak();
            window.$method(move |$($arg),*| {
                let Some($ui) = weak.upgrade() else { return };
                match state.try_borrow_mut() {
                    #[allow(unused_mut)]
                    Ok(mut $owner) => $body,
                    Err(_) => {
                        let (state, weak) = (state.clone(), weak.clone());
                        drop($ui);
                        slint::Timer::single_shot(Duration::ZERO, move || {
                            if let (Some($ui), Ok(mut $owner)) = (weak.upgrade(), state.try_borrow_mut()) {
                                #[allow(unused_mut, unused_variables)]
                                let _ = &mut $owner;
                                $body
                            }
                        });
                    }
                }
            });
        }};
    }
    {
        let geometry = controller.borrow().tab_bounds.clone();
        window.on_tab_geometry(move |tab, x, width| {
            if let Ok(tab) = Uuid::parse_str(&tab) {
                geometry.borrow_mut().insert(tab, (x.into(), width.into()));
            }
        });
    }
    bind!(on_tab_drag, |owner, ui, source, tab, x, y, finished| {
        let target = owner.drop_target(x.into(), y.into());
        if finished {
            ui.set_drop_preview(DropPreview::default());
            if let (Ok(source), Ok(tab), Some((destination, zone, _))) =
                (Uuid::parse_str(&source), Uuid::parse_str(&tab), target)
            {
                owner.mutate(&ui, |desktop| {
                    let source_owner = desktop.owner_of_pane(source).map(|workspace| workspace.id);
                    let destination_owner = desktop
                        .owner_of_pane(destination)
                        .map(|workspace| workspace.id);
                    if source_owner.is_some() && source_owner == destination_owner {
                        desktop.focus_pane(destination);
                        desktop.owner_of_pane_mut(source).unwrap().move_tab(
                            source,
                            destination,
                            tab,
                            zone,
                        );
                    }
                });
            }
        } else {
            ui.set_drop_preview(target.map(|(_, _, preview)| preview).unwrap_or_default());
        }
    });
    bind!(on_layout, |owner, ui, width, height| {
        owner.dock_width = f64::from(ui.get_dock_width()).max(0.0);
        owner.viewport.width = f64::from(width).max(0.0);
        owner.viewport.height = f64::from(height).max(0.0);
        if owner.desktop.current().layout_mode == basalt::canvas::LayoutMode::Canvas {
            let viewport = owner.viewport;
            let workspace = owner.desktop.current_mut();
            workspace.canvas.resize_viewport(viewport);
            workspace
                .canvas
                .reveal(workspace.focused_view_pane(), viewport);
        }
        owner.refresh(&ui);
    });
    {
        let weak = window.as_weak();
        window.on_chrome_metrics(move |height| {
            #[cfg(windows)]
            if let Some(window) = weak.upgrade() {
                window.window().with_winit_window(|native| {
                    window_frame::set_header_height(native, f64::from(height))
                });
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_header_controls(move |left, width| {
            #[cfg(windows)]
            if let Some(window) = weak.upgrade() {
                window.window().with_winit_window(|native| {
                    window_frame::set_control_region(native, left.into(), width.into())
                });
            }
        });
    }
    bind!(on_workspace_select, |owner, ui, id| {
        if let Ok(id) = Uuid::parse_str(&id) {
            owner.mutate(&ui, |desktop| desktop.select(id));
        }
    });
    bind!(on_workspace_rename, |owner, ui, id, title| {
        if let Ok(id) = Uuid::parse_str(&id) {
            let name = title.trim();
            if !name.is_empty() && name.chars().count() <= 64 {
                if let Some(workspace) = owner
                    .desktop
                    .workspaces
                    .iter_mut()
                    .find(|workspace| workspace.id == id)
                {
                    // A name the user chose stays; the terminal no longer renames the session.
                    workspace.title = name.into();
                    workspace.renamed = true;
                    owner.save_dirty = true;
                }
            }
            owner.refresh(&ui);
        }
    });
    bind!(on_workspace_duplicate, |owner, ui, id| {
        if let Ok(id) = Uuid::parse_str(&id) {
            if let Some((copy, ids)) = owner.desktop.duplicate_workspace(id) {
                for (old, new) in ids {
                    if let Some(launch) = owner.launches.get(&old).cloned() {
                        owner.launches.insert(new, launch);
                    }
                    if let Some(tool) = owner.tab_tools.get(&old).cloned() {
                        owner.tab_tools.insert(new, tool);
                    }
                    if let Some(profile) = owner.tab_profiles.get(&old).cloned() {
                        owner.tab_profiles.insert(new, profile);
                    }
                }
                let _ = copy;
                owner.save_dirty = true;
                owner.refresh(&ui);
            }
        }
    });
    bind!(on_workspace_move, |owner, ui, id, direction| {
        if let Ok(id) = Uuid::parse_str(&id) {
            owner.desktop.move_workspace(id, direction);
            owner.refresh(&ui);
        }
    });
    bind!(on_workspace_close, |owner, ui, id| {
        if let Ok(id) = Uuid::parse_str(&id) {
            let retired = owner.desktop.close_workspace(id);
            owner.retire(retired);
            owner.ensure_terminals();
            owner.refresh(&ui);
        }
    });
    bind!(on_chrome_action, |owner, ui, action| {
        match action {
            2 => owner.dispatch(&ui, Command::NewWorkspace, CommandTarget::Focused),
            7 => {
                let pane = owner.desktop.focused_container().focused_view_pane();
                if let Some(tab) = owner.desktop.focused_container().selected_tab() {
                    let retired = owner.desktop.close_surface(pane, tab);
                    owner.retire(retired);
                    owner.refresh(&ui);
                }
            }
            6 => owner.dispatch(&ui, Command::NewPaneAutoLayout, CommandTarget::Focused),
            4 | 5 => owner.mutate(&ui, |desktop| desktop.navigate(action == 5)),
            1 => ui.set_message("No notifications".into()),
            3 => ui.invoke_show_workspace_menu(),
            _ => {}
        }
    });
    // Session profiles: the session's idle terminals start again with the new one.
    bind!(on_profile_select, |owner, ui, id| {
        let profile = Uuid::parse_str(&id).ok();
        if owner.desktop.session_profile() != profile && owner.desktop.set_session_profile(profile) {
            owner.save_dirty = true;
            let busy = owner.restart_idle_shells();
            if busy > 0 {
                ui.set_message(
                    format!("{busy} terminal{} running a program keep{} the previous profile until restarted", if busy == 1 { "" } else { "s" }, if busy == 1 { "s" } else { "" })
                        .into(),
                );
            }
            owner.refresh(&ui);
        }
    });
    bind!(on_profile_new, |owner, ui| {
        if let Some(id) = owner.desktop.add_profile() {
            owner.desktop.set_session_profile(Some(id));
            owner.save_dirty = true;
            owner.refresh(&ui);
        }
    });
    bind!(on_profile_rename, |owner, ui, id, name| {
        if let Ok(id) = Uuid::parse_str(&id) {
            if owner.desktop.rename_profile(id, &name) {
                owner.save_dirty = true;
                owner.refresh(&ui);
            }
        }
    });
    bind!(on_profile_delete, |owner, ui, id| {
        if let Ok(id) = Uuid::parse_str(&id) {
            if owner.desktop.delete_profile(id) {
                owner.save_dirty = true;
                owner.refresh(&ui);
            }
        }
    });
    // A CLI inside a session profile: open its folder, or sign out by clearing it (the
    // next terminal recreates it empty).
    bind!(on_profile_tool, |owner, ui, profile, tool, action| {
        let Ok(profile) = Uuid::parse_str(&profile) else {
            return;
        };
        let Some(folder) = owner.engine.profile_tool_folder(&profile.simple().to_string(), &tool) else {
            return;
        };
        match action.as_str() {
            "open" => {
                if let Err(error) = std::process::Command::new("explorer.exe").arg(&folder).spawn() {
                    ui.set_message(format!("Could not open the folder: {error}").into());
                }
            }
            "reset" => {
                if folder.is_dir() {
                    if let Err(error) = std::fs::remove_dir_all(&folder) {
                        ui.set_message(format!("Could not sign out of {tool}: {error}").into());
                    }
                }
                owner.profile_tools.remove(&profile);
                owner.refresh(&ui);
            }
            _ => {}
        }
    });
    bind!(on_profile_share, |owner, ui, id, kind, on| {
        use basalt_core::domain::ShareCategory;
        let Ok(id) = Uuid::parse_str(&id) else {
            return;
        };
        let category = if kind == "mcp" { ShareCategory::SharedMcp } else { ShareCategory::SharedSkills };
        if on {
            if let Err(error) = owner.engine.join_sharing(&id.simple().to_string(), category) {
                ui.set_message(format!("Could not share: {error}").into());
                return;
            }
        }
        if let Some(profile) = owner.desktop.profiles.iter_mut().find(|profile| profile.id == id) {
            match category {
                ShareCategory::SharedMcp => profile.share_mcp = on,
                _ => profile.share_skills = on,
            }
            owner.save_dirty = true;
        }
        owner.profile_tools.remove(&id);
        owner.refresh(&ui);
    });
    bind!(on_new_session, |owner, ui| {
        owner.dispatch(&ui, Command::NewWorkspace, CommandTarget::Focused);
    });
    bind!(on_command_execute, |owner, ui, id| {
        if let Some(command) = Command::from_id(&id) {
            owner.dispatch(&ui, command, CommandTarget::Focused);
        }
    });
    bind!(on_reset_workspace_appearance, |owner, ui| {
        if let Some(settings) = &owner.preferences {
            if let Err(error) = settings.reset() { ui.set_message(error.to_string().into()); return; }
        }
        owner.default_palette = 0; owner.default_font = 100; owner.default_terminal_font = 100;
        owner.refresh(&ui);
    });
    bind!(on_font_change, |owner, ui, percent| {
        owner.set_interface_font(&ui, percent);
    });
    bind!(on_terminal_font_change, |owner, ui, percent| {
        owner.set_terminal_font(&ui, percent);
    });
    bind!(on_color_change, |owner, ui, palette| {
        owner.set_palette(&ui, palette);
    });
    bind!(on_palette_query, |owner, ui, query| {
        let entries = basalt::palette::matching(basalt::palette::entries(owner.palette_current()), &query);
        ui.set_palette_items(ModelRc::new(VecModel::from(
            entries
                .into_iter()
                .map(|entry| basalt::PaletteItem { id: entry.id.into(), title: entry.title.into(), detail: entry.detail.into() })
                .collect::<Vec<_>>(),
        )));
    });
    bind!(on_palette_run, |owner, ui, id| {
        owner.run_palette(&ui, &id);
    });
    bind!(on_history_query, |owner, ui, query| {
        owner.history_query = query.into();
        owner.search_history(&ui);
    });
    bind!(on_history_forget, |owner, ui, command| {
        // A command forgotten here is no longer suggested or found.
        let Some(tab) = owner.history_search else {
            return;
        };
        let key = owner.history_key(tab);
        let path = owner.history_folder.as_ref().map(|folder| folder.join(format!("{key}.json")));
        let history = owner.history(&key);
        history.forget(&command);
        if let Some(path) = path {
            let _ = history.save(&path);
        }
        owner.search_history(&ui);
    });
    bind!(on_history_scope_switch, |owner, ui| {
        owner.history_here = !owner.history_here;
        owner.search_history(&ui);
    });
    bind!(on_history_run, |owner, _ui, command| {
        // Replace what is typed with the chosen command, without running it.
        let Some(tab) = owner.history_search.take() else {
            return;
        };
        if let Some(surface) = owner.surfaces.get(&tab) {
            use slint::platform::Key;
            let mode = surface.terminal.model().mode();
            let mut bytes = input::encode(&char::from(Key::End).to_string(), false, false, false, mode);
            bytes.extend(input::encode(&char::from(Key::Home).to_string(), true, false, false, mode));
            bytes.extend(command.as_bytes());
            let _ = surface.terminal.input(bytes);
        }
    });
    bind!(on_terminal_selection, |owner, _ui, pane, a, b, c, d| {
        if let Ok(pane) = Uuid::parse_str(&pane) {
            if let Some(tab) = owner
                .desktop
                .owner_of_pane(pane)
                .and_then(|workspace| workspace.projected_pane(pane))
                .and_then(|pane| pane.selected)
            {
                owner.select_terminal_text(tab, a, b, c, d);
            }
        }
    });
    bind!(on_pane_action, |owner, ui, id, action| {
        if action == 1 {
            ui.set_message("The embedded browser isn't available yet".into());
            return;
        }
        if let Ok(id) = Uuid::parse_str(&id) {
            if action == 10 || action == 11 {
                if let Some(tab) = owner
                    .desktop
                    .owner_of_pane(id)
                    .and_then(|workspace| workspace.projected_pane(id))
                    .and_then(|pane| pane.selected)
                {
                    if let Err(error) = owner.clipboard_action(&ui, tab, action == 11) {
                        ui.set_message(error.into());
                    }
                }
                return;
            }
            if action == 13 {
                owner.mutate(&ui, |desktop| {
                    if let Some(workspace) = desktop.owner_of_pane_mut(id) {
                        workspace.toggle_zoom(id);
                    }
                });
                return;
            }
            let command = match action {
                0 | 2 | 3 => Command::NewPaneAutoLayout,
                _ => return,
            };
            owner.dispatch(&ui, command, CommandTarget::Pane(id));
        }
    });
    bind!(on_tab_select, |owner, ui, pane, tab| {
        if let (Ok(pane), Ok(tab)) = (Uuid::parse_str(&pane), Uuid::parse_str(&tab)) {
            owner.mutate(&ui, |desktop| {
                if !desktop.focus_pane(pane) {
                    return;
                }
                let workspace = desktop.owner_of_pane_mut(pane).unwrap();
                workspace.select_view_tab(pane, tab);
            });
        }
    });
    bind!(on_tab_close, |owner, ui, pane, tab| {
        if let (Ok(pane), Ok(tab)) = (Uuid::parse_str(&pane), Uuid::parse_str(&tab)) {
            let retired = owner.desktop.close_surface(pane, tab);
            owner.retire(retired);
            owner.ensure_terminals();
            owner.refresh(&ui);
        }
    });
    bind!(on_pane_focus, |owner, ui, pane| {
        if let Ok(pane) = Uuid::parse_str(&pane) {
            owner.mutate(&ui, |desktop| {
                desktop.focus_pane(pane);
            });
        }
    });
    bind!(on_divider_move, |owner, ui, id, x, y| {
        if let Ok(id) = Uuid::parse_str(&id) {
            let viewport = owner.viewport;
            let dock_width = owner.dock_width;
            owner
                .desktop
                .resize_divider(id, x.into(), y.into(), viewport, dock_width);
            owner.refresh(&ui);
        }
    });
    bind!(on_pane_scroll, |owner, ui, pane, delta| {
        if let Ok(pane) = Uuid::parse_str(&pane) {
            if let Some(id) = owner
                .desktop
                .owner_of_pane(pane)
                .and_then(|workspace| workspace.projected_pane(pane))
                .and_then(|pane| pane.selected)
            {
                if let Some(surface) = owner.surfaces.get(&id) {
                    surface.terminal.scroll((delta / 20.0).round() as i32);
                    if let Some(grids) = &owner.grids {
                        grids.borrow_mut().scrolled(&id);
                    }
                }
            }
        }
        owner.refresh(&ui);
    });
    bind!(on_dock_focus, |owner, ui| {
        if owner.desktop.dock_visible && owner.desktop.dock.is_some() {
            owner.desktop.dock_focused = true;
            owner.refresh(&ui);
        }
    });
    bind!(on_canvas_pan, |owner, ui, x, y| {
        if owner.desktop.current().layout_mode == basalt::canvas::LayoutMode::Canvas {
            owner
                .desktop
                .current_mut()
                .canvas
                .pan_by(x.into(), y.into());
            owner.refresh(&ui);
        }
    });
    bind!(on_key_input, |owner,
                         ui,
                         text,
                         control,
                         shift,
                         alt,
                         meta| {
        let text: slint::SharedString =
            basalt::clipboard::key_text(&text, control).into();
        let lower = text.to_lowercase();
        if let Some(tab) = owner
            .desktop
            .focused_container()
            .selected_tab()
        {
            let paste = (control || meta) && lower == "v"
                || shift && text == char::from(slint::platform::Key::Insert).to_string();
            let copy = (control || meta)
                && lower == "c"
                && (shift || owner.selected_text.contains_key(&tab))
                || control && text == char::from(slint::platform::Key::Insert).to_string();
            if !alt && (paste || copy) {
                if let Err(error) = owner.clipboard_action(&ui, tab, paste) {
                    ui.set_message(error.into());
                }
                return;
            }
        }
        if alt && !control && !shift && !meta {
            use slint::platform::Key;
            let direction = [
                (Key::LeftArrow, (-1.0, 0.0)),
                (Key::RightArrow, (1.0, 0.0)),
                (Key::UpArrow, (0.0, -1.0)),
                (Key::DownArrow, (0.0, 1.0)),
            ]
            .into_iter()
            .find(|(key, _)| text == char::from(*key).to_string())
            .map(|(_, direction)| direction);
            if let Some((dx, dy)) = direction {
                if let Some(pane) = owner.desktop.neighbor_pane(owner.viewport, owner.dock_width, dx, dy) {
                    owner.mutate(&ui, |desktop| {
                        desktop.focus_pane(pane);
                    });
                }
                return;
            }
        }
        if control && shift && !alt && text == char::from(slint::platform::Key::Return).to_string() {
            let pane = owner.desktop.focused_container().focused_view_pane();
            owner.mutate(&ui, |desktop| {
                if let Some(workspace) = desktop.owner_of_pane_mut(pane) {
                    workspace.toggle_zoom(pane);
                }
            });
            return;
        }
        if control && shift && !alt && lower == "w" {
            let workspace = owner.desktop.focused_container();
            let pane = workspace.focused_view_pane();
            if let Some(tab) = workspace.selected_tab() {
                let retired = owner.desktop.close_surface(pane, tab);
                owner.retire(retired);
                owner.ensure_terminals();
                owner.refresh(&ui);
            }
            return;
        }
        // Terminal text size, as in other terminals: Ctrl+= or Ctrl++, Ctrl+-, Ctrl+0.
        if control && !alt && !meta {
            let percent = match text.as_str() {
                "=" | "+" => Some(basalt::palette::step(owner.default_terminal_font, true)),
                "-" if !shift => Some(basalt::palette::step(owner.default_terminal_font, false)),
                "0" if !shift => Some(100),
                _ => None,
            };
            if let Some(percent) = percent {
                owner.set_terminal_font(&ui, percent);
                return;
            }
        }
        if (meta || (control && shift)) && !alt {
            match lower.as_str() {
                "n" => {
                    owner.dispatch(&ui, Command::NewWorkspace, CommandTarget::Focused);
                    return;
                }
                "t" => {
                    owner.dispatch(&ui, Command::NewPaneAutoLayout, CommandTarget::Focused);
                    return;
                }
                "d" => {
                    owner.dispatch(&ui, Command::NewPaneAutoLayout, CommandTarget::Focused);
                    return;
                }
                "b" => {
                    ui.set_sidebar_visible(!ui.get_sidebar_visible());
                    return;
                }
                "p" => {
                    ui.invoke_show_palette();
                    return;
                }
                "e" if control && shift => {
                    owner.dispatch(&ui, Command::NewPaneAutoLayout, CommandTarget::Focused);
                    return;
                }
                digit
                    if digit.len() == 1 && digit.as_bytes()[0].is_ascii_digit() && digit != "0" =>
                {
                    let index = lower.parse::<usize>().unwrap() - 1;
                    if let Some(workspace) = owner.desktop.workspaces.get(index) {
                        let id = workspace.id;
                        owner.mutate(&ui, |desktop| desktop.select(id));
                    }
                    return;
                }
                _ => {}
            }
        }
        if let Some(id) = owner.desktop.focused_container().selected_tab() {
            use slint::platform::Key;
            let key = |key: Key| text == char::from(key).to_string();
            // An install offer: arrows choose, Enter or Y installs, N or Esc dismisses.
            if owner.install.as_ref().is_some_and(|offer| offer.tab == id) && !control && !alt {
                let installing = owner.install.as_ref().is_some_and(InstallOffer::installing);
                let count = owner.install.as_ref().map_or(0, |offer| offer.offers.len());
                if key(Key::Escape) || (!installing && lower == "n") {
                    owner.install = None;
                    owner.clear_line(id);
                    owner.refresh_panes(&ui);
                    return;
                }
                if installing {
                    return;
                }
                if count > 0 && (key(Key::UpArrow) || key(Key::DownArrow)) {
                    if let Some(offer) = &mut owner.install {
                        offer.selected = if key(Key::UpArrow) { (offer.selected + count - 1) % count } else { (offer.selected + 1) % count };
                        offer.failed = None;
                    }
                    owner.refresh_panes(&ui);
                    return;
                }
                if count > 0 && (key(Key::Return) || lower == "y") {
                    owner.start_install();
                    owner.refresh_panes(&ui);
                    return;
                }
                owner.install = None;
            }
            // An open completion list takes the keys that move in it and choose from it.
            if owner.completion.as_ref().is_some_and(|completion| completion.tab == id) && !control && !alt {
                let line = owner
                    .grids
                    .as_ref()
                    .and_then(|grids| grids.borrow().screen(&id).and_then(basalt::command_line::read));
                let entries: Vec<String> = match (&owner.completion, &line) {
                    (Some(completion), Some(line)) => match owner.helps.get(&completion.program) {
                        Some(Some(help)) => basalt::flags::matching(
                            if completion.commands { &help.commands } else { &help.flags },
                            basalt::flags::current_word(&line.text),
                        )
                        .into_iter()
                        .map(|entry| entry.name.clone())
                        .collect(),
                        _ => Vec::new(),
                    },
                    _ => Vec::new(),
                };
                if key(Key::Escape) {
                    owner.completion = None;
                    owner.refresh_panes(&ui);
                    return;
                }
                if !entries.is_empty() && (key(Key::UpArrow) || key(Key::DownArrow)) {
                    if let Some(completion) = &mut owner.completion {
                        let count = entries.len();
                        completion.selected = if key(Key::UpArrow) { (completion.selected + count - 1) % count } else { (completion.selected + 1) % count };
                    }
                    owner.refresh_panes(&ui);
                    return;
                }
                if !entries.is_empty() && (key(Key::Tab) || key(Key::Return) || key(Key::RightArrow)) {
                    let selected = owner.completion.as_ref().map_or(0, |completion| completion.selected).min(entries.len() - 1);
                    let word = line.as_ref().map(|line| basalt::flags::current_word(&line.text).to_owned()).unwrap_or_default();
                    let typed = format!("{} ", entries[selected].get(word.len()..).unwrap_or(""));
                    owner.completion = None;
                    if let Some(surface) = owner.surfaces.get(&id) {
                        let _ = surface.terminal.input(typed.into_bytes());
                    }
                    return;
                }
            }
            // Tab after a flag prefix, or after the program name, offers what its help lists.
            if key(Key::Tab) && !control && !alt && !shift && owner.completion.is_none() {
                let line = owner
                    .grids
                    .as_ref()
                    .and_then(|grids| grids.borrow().screen(&id).and_then(basalt::command_line::read))
                    .filter(|line| line.at_end());
                if let Some(line) = line {
                    let word = basalt::flags::current_word(&line.text).to_owned();
                    let words = line.text.split_whitespace().count();
                    let flags = word.starts_with('-');
                    let commands = !flags && ((words == 1 && word.is_empty()) || (words == 2 && !word.is_empty()));
                    let program = basalt::flags::program(&line.text)
                        .filter(|program| (flags || commands) && basalt::flags::found(program));
                    if let Some(program) = program {
                        let program = program.to_owned();
                        match owner.helps.get(&program) {
                            Some(Some(help)) => {
                                let entries = if flags { &help.flags } else { &help.commands };
                                let found = basalt::flags::matching(entries, &word);
                                if found.len() == 1 {
                                    let typed = format!("{} ", found[0].name.get(word.len()..).unwrap_or(""));
                                    if let Some(surface) = owner.surfaces.get(&id) {
                                        let _ = surface.terminal.input(typed.into_bytes());
                                    }
                                    return;
                                }
                                if found.len() > 1 {
                                    owner.completion = Some(Completion { tab: id, program, commands: !flags, selected: 0 });
                                    owner.refresh_panes(&ui);
                                    return;
                                }
                            }
                            Some(None) => return,
                            None => {
                                owner.helps.insert(program.clone(), None);
                                owner.completion = Some(Completion { tab: id, program: program.clone(), commands: !flags, selected: 0 });
                                let folder = owner.folders.get(&id).map(|(_, folder)| PathBuf::from(folder)).unwrap_or_else(|| owner.cwd.clone());
                                let tx = owner.help_tx.clone();
                                std::thread::spawn(move || {
                                    let help = basalt::flags::ask(&program, &folder)
                                        .map(|text| basalt::flags::parse(&text))
                                        .unwrap_or_default();
                                    let _ = tx.send((program, help));
                                });
                                return;
                            }
                        }
                    }
                }
            }
            if let Some(suggestion) = owner.suggestions.get(&id).cloned() {
                let accept = if !control && !alt && !shift && (key(Key::RightArrow) || key(Key::End)) {
                    Some(suggestion)
                } else if control && !alt && !shift && key(Key::RightArrow) {
                    // One word: the spaces before it and the word itself.
                    let start = suggestion.len() - suggestion.trim_start().len();
                    let end = suggestion[start..].find(char::is_whitespace).map_or(suggestion.len(), |at| start + at);
                    Some(suggestion[..end].to_owned())
                } else {
                    None
                };
                if let Some(typed) = accept {
                    owner.suggestions.remove(&id);
                    if let Some(surface) = owner.surfaces.get(&id) {
                        let _ = surface.terminal.input(typed.into_bytes());
                    }
                    return;
                }
            }
            // Ctrl+R at the prompt searches the profile's history; elsewhere programs keep it.
            if control && !alt && !shift && lower == "r" {
                let at_prompt = owner
                    .grids
                    .as_ref()
                    .is_some_and(|grids| grids.borrow().screen(&id).and_then(basalt::command_line::read).is_some());
                if at_prompt {
                    owner.history_search = Some(id);
                    ui.invoke_show_history();
                    return;
                }
            }
            // `install <name>`, or a pasted global npm install: the app's own installer
            // answers under the line instead of the shell.
            if !control && !alt && key(Key::Return) {
                let line = owner
                    .grids
                    .as_ref()
                    .and_then(|grids| grids.borrow().screen(&id).and_then(basalt::command_line::read));
                if let Some(request) = line.as_ref().and_then(|line| basalt::packages::request(&line.text)) {
                    owner.request_install(id, request);
                    owner.refresh_panes(&ui);
                    return;
                }
            }
            if !control && !alt && (key(Key::Return) || text == "\n") {
                let line = owner
                    .grids
                    .as_ref()
                    .and_then(|grids| grids.borrow().screen(&id).and_then(basalt::command_line::read));
                if let Some(line) = line.filter(|line| !line.text.trim().is_empty()) {
                    owner.running.insert(id, (line.text, std::time::SystemTime::now()));
                }
            }
            let mode = owner
                .surfaces
                .get(&id)
                .map(|surface| surface.terminal.model().mode())
                .unwrap_or(0);
            let bytes = input::encode(&text, control, shift, alt, mode);
            if !bytes.is_empty() {
                owner.clear_selection(&ui);
            }
            if let Some(surface) = owner.surfaces.get(&id) {
                if let Err(error) = surface.terminal.input(bytes) {
                    ui.set_message(error.to_string().into());
                }
            } else if owner.pending.contains(&id) {
                let queued = owner.pending_input.entry(id).or_default();
                if queued.len().saturating_add(bytes.len()) <= 64 * 1024 {
                    queued.extend(bytes);
                } else {
                    ui.set_message("Terminal is still starting; input queue is full".into());
                }
            }
        }
    });
    // One event filter for production and smoke runs: a smoke input filter must
    // never replace DPI processing. Defer refresh until winit/Slint apply scale.
    {
        let weak = window.as_weak();
        let state = controller.clone();
        let test_input_isolated = std::env::args().any(|arg| arg == "--verify-ui");
        window.window().on_winit_window_event(move |_, event| {
            use slint::winit_030::{EventResult, winit::event::WindowEvent};
            if let Some(window) = weak.upgrade() {
                match event {
                    WindowEvent::Resized(_) | WindowEvent::Focused(true) => {
                        if matches!(event, WindowEvent::Focused(true)) {
                            window
                                .global::<basalt::Appearance>()
                                .set_active(true);
                        }
                        let weak = window.as_weak();
                        // The backend may replace/create its HWND while processing
                        // this event. Bind the frame after that operation completes.
                        slint::Timer::single_shot(Duration::ZERO, move || {
                            if let Some(window) = weak.upgrade() {
                                synchronize_native_frame(&window);
                            }
                        });
                    }
                    WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                        if basalt::display::apply_scale(&window, *scale_factor) {
                            let weak = window.as_weak();
                            let state = state.clone();
                            slint::Timer::single_shot(Duration::ZERO, move || {
                                if let Some(window) = weak.upgrade() {
                                    #[cfg(windows)]
                                    window.window().with_winit_window(|native| {
                                        if let Ok(width) = window_frame::install(native) {
                                            window.set_caption_width(width);
                                            window_frame::set_control_region(
                                                native,
                                                f64::from(window.get_header_control_left()),
                                                f64::from(window.get_header_control_width()),
                                            );
                                            window_frame::set_appearance(
                                                native,
                                                native_background(&window),
                                                true,
                                            );
                                            window_frame::set_header_height(
                                                native,
                                                f64::from(window.get_header_height()),
                                            );
                                        }
                                    });
                                    state.borrow_mut().refresh(&window);
                                }
                            });
                        }
                    }
                    WindowEvent::Focused(active) => window
                        .global::<basalt::Appearance>()
                        .set_active(*active),
                    _ => {}
                }
            }
            if test_input_isolated
                && matches!(
                    event,
                    WindowEvent::MouseInput { .. }
                        | WindowEvent::CursorMoved { .. }
                        | WindowEvent::MouseWheel { .. }
                        | WindowEvent::KeyboardInput { .. }
                )
            {
                EventResult::PreventDefault
            } else {
                EventResult::Propagate
            }
        });
    }
    let (activity_tx, activity_requests) = mpsc::channel::<Vec<(Uuid, u32)>>();
    let (activity_results, activity_rx) = mpsc::channel();
    std::thread::spawn(move || {
        // Command lines of runtimes and shells, read once per process.
        let mut lines: HashMap<u32, Option<String>> = HashMap::new();
        while let Ok(roots) = activity_requests.recv() {
            let processes = basalt::agents::snapshot();
            lines.retain(|pid, _| processes.iter().any(|process| process.pid == *pid));
            let result = roots
                .into_iter()
                .filter_map(|(id, pid)| {
                    basalt::agents::detect_agent(pid, &processes, |pid| {
                        lines.entry(pid).or_insert_with(|| basalt::agents::command_line(pid)).clone()
                    })
                    .map(|activity| (id, activity))
                })
                .collect::<HashMap<_, _>>();
            if activity_results.send(result).is_err() {
                break;
            }
        }
    });
    let activity_timer = slint::Timer::default();
    {
        let weak = window.as_weak();
        let state = controller.clone();
        activity_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(750),
            move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };
                let mut owner = state.borrow_mut();
                while let Ok(activities) = activity_rx.try_recv() {
                    // An agent that left or restarted starts over: its screen, its
                    // turns and its lifecycle signal belong to the one before.
                    for (id, previous) in owner.activities.clone() {
                        if activities.get(&id).is_none_or(|a| a.pid != previous.pid) {
                            owner.screens.forget(id);
                            owner.screen_states.remove(&id);
                            owner.agent_turns.remove(&id);
                        }
                    }
                    if let Some(bridge_root) = owner.bridge.as_ref().map(|b| b.root.clone()) {
                        let bridge = basalt::agent_bridge::Bridge { root: bridge_root };
                        for (id, previous) in owner.activities.clone() {
                            if activities.get(&id).is_none_or(|a| a.pid != previous.pid) {
                                owner.persist_sharing(id, &previous.id);
                                bridge.clear(id);
                                owner.signals.remove(&id);
                                owner
                                    .signal_invalidated
                                    .insert(id, std::time::Instant::now());
                            }
                        }
                    }
                    owner.activities = activities;
                }
                owner.refresh_agents(&window);
                let roots = owner
                    .surfaces
                    .iter()
                    .filter(|(_, s)| !s.exited)
                    .map(|(id, s)| (*id, s.terminal.pid()))
                    .collect();
                let _ = activity_tx.send(roots);
            },
        );
    }
    // Each agent's screen, read as it changes: what it shows decides its state.
    let screen_timer = slint::Timer::default();
    {
        let weak = window.as_weak();
        let state = controller.clone();
        screen_timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let Ok(mut owner) = state.try_borrow_mut() else {
                return;
            };
            let owner = &mut *owner;
            let now = std::time::Instant::now();
            let mut states = HashMap::new();
            for (id, surface) in &owner.surfaces {
                if surface.exited {
                    continue;
                }
                let agent = owner
                    .activities
                    .get(id)
                    .map(|activity| activity.id.clone())
                    .or_else(|| owner.tab_tools.get(id).filter(|tool| basalt::agents::agent(tool).is_some()).cloned());
                let Some(agent) = agent else {
                    continue;
                };
                let model = surface.terminal.model();
                let title = surface.terminal.title();
                let reading = owner.screens.update(*id, &agent, model.revision(), &title, || model.live_text(), now);
                states.insert(*id, reading);
            }
            if states != owner.screen_states {
                owner.screen_states = states;
            }
            owner.refresh_agents(&window);
        });
    }
    let (signal_requests_tx, signal_requests) = mpsc::sync_channel::<Vec<Uuid>>(1);
    let (signal_results_tx, signal_results) = mpsc::channel();
    let signal_root = controller.borrow().bridge.as_ref().map(|b| b.root.clone());
    std::thread::spawn(move || {
        let bridge = signal_root.map(|root| basalt::agent_bridge::Bridge { root });
        let mut previous = HashMap::new();
        while let Ok(ids) = signal_requests.recv() {
            let sampled = std::time::Instant::now();
            let signals = ids
                .into_iter()
                .filter_map(|id| bridge.as_ref().and_then(|b| b.read(id)).map(|s| (id, s)))
                .collect::<HashMap<_, _>>();
            if signals != previous {
                previous = signals.clone();
                if signal_results_tx.send((sampled, signals)).is_err() {
                    break;
                }
            }
        }
    });
    let signal_timer = slint::Timer::default();
    {
        let state = controller.clone();
        signal_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(75),
            move || {
                let ids = state
                    .borrow()
                    .surfaces
                    .iter()
                    .filter(|(_, s)| !s.exited)
                    .map(|(id, _)| *id)
                    .collect();
                let _ = signal_requests_tx.try_send(ids);
            },
        );
    }
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let state = controller.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_micros(5_000),
        move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let mut owner = state.borrow_mut();
            let mut changed = false;
            if owner.agents_refreshed.elapsed() >= Duration::from_secs(1) {
                owner.refresh_agents(&window);
            }
            while let Ok((sampled, mut signals)) = signal_results.try_recv() {
                signals.retain(|id, _| {
                    owner.desktop.contains_tab(*id)
                        && owner
                            .signal_invalidated
                            .get(id)
                            .is_none_or(|cleared| sampled >= *cleared)
                });
                for (id, signal) in &signals {
                    if basalt_core::tools::conversation::valid_id(&signal.session_id) {
                        if let Some(conversation) = owner
                            .launches
                            .get_mut(id)
                            .and_then(|launch| launch.conversation.as_mut())
                        {
                            if conversation.tool == signal.provider
                                && conversation.id.as_deref() != Some(&signal.session_id)
                            {
                                conversation.id = Some(signal.session_id.clone());
                                owner.save_dirty = true;
                            }
                        }
                    }
                }
                if owner.signals != signals {
                    let now = std::time::Instant::now();
                    for (id, signal) in &signals {
                        if owner.signals.get(id) != Some(signal) {
                            owner.signal_changed.insert(*id, now);
                        }
                    }
                    owner.signals = signals;
                    owner.refresh_agents(&window);
                }
            }

            while let Ok((id, result)) = rx.try_recv() {
                changed = true;
                owner.pending.retain(|pending| *pending != id);
                match result {
                    Ok((terminal, conversation, cwd)) if owner.desktop.contains_tab(id) => {
                        if let Some(launch) = owner.launches.get_mut(&id) {
                            launch.conversation = conversation;
                            if let Some(cwd) = cwd {
                                launch.cwd = cwd;
                            }
                        }
                        owner.save_dirty = true;
                        if let Some(bytes) = owner.pending_input.remove(&id) {
                            if let Err(error) = terminal.input(bytes) {
                                window.set_message(error.to_string().into());
                            }
                        }
                        terminal.model().on_change(terminal_waker());
                        owner.surfaces.insert(
                            id,
                            Surface {
                                terminal,
                                revision: u64::MAX,
                                title_revision: u64::MAX,
                                frame: TerminalFrame::default(),
                                renderer: Default::default(),
                                look: None,
                                size: TerminalSize {
                                    columns: 80,
                                    rows: 24,
                                    cell_width: 9,
                                    cell_height: 23,
                                },
                                pending_size: None,
                                sized: false,
                                focused: false,
                                exited: false,
                            },
                        );
                    }
                    Ok((terminal, _, _)) => {
                        std::thread::spawn(move || {
                            let _ = terminal.stop(Duration::from_millis(100));
                        });
                    }
                    Err(error) if owner.desktop.contains_tab(id) => {
                        owner.pending_input.remove(&id);
                        owner.failures.insert(id, error);
                    }
                    Err(_) => {}
                }
            }
            let title_updates: Vec<_> = owner
                .surfaces
                .iter_mut()
                .filter_map(|(id, surface)| {
                    surface
                        .terminal
                        .title_update_since(surface.title_revision)
                        .map(|(version, title)| {
                            surface.title_revision = version;
                            (*id, title)
                        })
                })
                .collect();
            for (id, title) in title_updates {
                changed |= owner.desktop.apply_terminal_title(id, &title);
            }
            // Output and pending resizes only touch the panes; titles and exits change
            // the sidebar and status too.
            let mut output = false;
            let visible: Vec<_> = owner
                .desktop
                .visible_layout(owner.viewport, 1.0, owner.dock_width)
                .into_iter()
                .flat_map(|layout| layout.panes)
                .filter_map(|pane| {
                    owner
                        .desktop
                        .owner_of_pane(pane.id)
                        .and_then(|workspace| workspace.projected_pane(pane.id))
                        .and_then(|pane| pane.selected)
                })
                .collect();
            for surface in visible.iter().filter_map(|id| owner.surfaces.get(id)) {
                changed |= !surface.exited && surface.terminal.exit_summary().is_some();
                output |= surface.terminal.model().revision() != surface.revision
                    || surface
                        .pending_size
                        .is_some_and(|(_, since)| since.elapsed() >= RESIZE_SETTLE);
            }
            output |= owner.take_gpu();
            #[cfg(windows)]
            {
                let (hover, pressed) = window_frame::caption_state();
                if window.get_caption_hover() != hover {
                    window.set_caption_hover(hover);
                }
                if window.get_caption_pressed() != pressed {
                    window.set_caption_pressed(pressed);
                }
            }
            while let Ok(event) = install_rx.try_recv() {
                owner.install_event(&window, event);
                output = true;
            }
            output |= owner.finish_install();
            while let Ok((program, help)) = help_rx.try_recv() {
                owner.helps.insert(program, Some(help));
                output |= owner.completion.is_some();
            }
            if changed {
                owner.refresh(&window);
            } else if output {
                owner.paced_refresh(&window);
            }
        },
    );
    if !smoke && controller.borrow().desktop.selected.is_nil() {
        controller.borrow_mut().dispatch(&window, Command::NewWorkspace, CommandTarget::Focused);
    }
    controller.borrow_mut().ensure_terminals();
    controller.borrow_mut().refresh(&window);
    window.show()?;
    window.invoke_focus_terminal();
    if smoke && std::env::args().any(|arg| arg == "--verify-interaction") {
        use slint::platform::WindowEvent;
        for text in "title BASALT_STARTUP_KEY_OK"
            .chars()
            .chain(std::iter::once(char::from(slint::platform::Key::Return)))
        {
            window.window().dispatch_event(WindowEvent::KeyPressed {
                text: text.to_string().into(),
            });
            window.window().dispatch_event(WindowEvent::KeyReleased {
                text: text.to_string().into(),
            });
        }
    }
    window.window().with_winit_window(|native| {
        native.set_theme(Some(slint::winit_030::winit::window::Theme::Dark));
        basalt::display::apply_scale(&window, native.scale_factor());
        #[cfg(windows)]
        if let Ok(width) = window_frame::install(native) {
            window.set_caption_width(width);
            window_frame::set_control_region(
                native,
                f64::from(window.get_header_control_left()),
                f64::from(window.get_header_control_width()),
            );
            window_frame::set_appearance(native, native_background(&window), true);
            window_frame::set_header_height(native, f64::from(window.get_header_height()));
        }
    });
    // Panes refresh at the display's own rate once its monitor is known.
    {
        let (state, weak) = (Rc::downgrade(&controller), window.as_weak());
        slint::Timer::single_shot(Duration::from_millis(200), move || {
            let (Some(state), Some(window)) = (state.upgrade(), weak.upgrade()) else {
                return;
            };
            let millihertz = window
                .window()
                .with_winit_window(|native| native.current_monitor().and_then(|monitor| monitor.refresh_rate_millihertz()))
                .flatten();
            if let Some(millihertz) = millihertz.filter(|rate| *rate >= 30_000) {
                state.borrow_mut().frame_interval = Duration::from_nanos(1_000_000_000_000 / u64::from(millihertz));
            }
        });
    }
    // winit creates its actual HWND when the event loop resumes. Calls before
    // that point may see no native window and must be retried after creation.
    {
        let weak = window.as_weak();
        slint::Timer::single_shot(Duration::from_millis(100), move || {
            if let Some(window) = weak.upgrade() {
                window.window().with_winit_window(|native| {
                    native.set_theme(Some(slint::winit_030::winit::window::Theme::Dark));
                    basalt::display::apply_scale(&window, native.scale_factor());
                    #[cfg(windows)]
                    if let Ok(width) = window_frame::install(native) {
                        window.set_caption_width(width);
                        window_frame::set_control_region(
                            native,
                            f64::from(window.get_header_control_left()),
                            f64::from(window.get_header_control_width()),
                        );
                        window_frame::set_appearance(native, native_background(&window), true);
                        window_frame::set_header_height(
                            native,
                            f64::from(window.get_header_height()),
                        );
                    }
                });
            }
        });
    }
    let verification_timer = std::env::args()
        .position(|arg| arg == "--verify-ui")
        .and_then(|index| std::env::args().nth(index + 1))
        .map(|path| native_smoke::start(&window, controller.clone(), PathBuf::from(path)));
    let tools_timer = std::env::args()
        .position(|arg| arg == "--verify-workspace-tools")
        .and_then(|index| std::env::args().nth(index + 1))
        .map(|path| {
            native_smoke::workspace_tools(&window, controller.clone(), PathBuf::from(path))
        });
    let workspace_timer = std::env::args()
        .position(|arg| arg == "--verify-workspace-storage")
        .and_then(|index| std::env::args().nth(index + 1))
        .map(|path| {
            native_smoke::workspace_roundtrip(&window, controller.clone(), PathBuf::from(path))
        });
    let menu_timer = std::env::args()
        .position(|arg| arg == "--verify-menu-latency")
        .and_then(|index| std::env::args().nth(index + 1))
        .map(|path| native_smoke::menu_latency(&window, controller.clone(), PathBuf::from(path)));
    let close_timer = std::env::args()
        .position(|arg| arg == "--verify-last-workspace-close")
        .and_then(|index| std::env::args().nth(index + 1))
        .map(|path| {
            native_smoke::last_workspace_close(&window, controller.clone(), PathBuf::from(path))
        });
    let typing_timer = std::env::args()
        .position(|arg| arg == "--verify-typing-latency")
        .and_then(|index| std::env::args().nth(index + 1))
        .map(|path| native_smoke::typing_latency(&window, controller.clone(), PathBuf::from(path)));
    let agent_timer = std::env::args()
        .position(|arg| arg == "--verify-agent-hooks")
        .and_then(|index| std::env::args().nth(index + 1))
        .map(|path| native_smoke::agent_hooks(&window, controller.clone(), PathBuf::from(path)));
    let live_timer = std::env::args()
        .position(|arg| arg == "--verify-live-codex-hook")
        .and_then(|index| std::env::args().nth(index + 1))
        .map(|path| {
            native_smoke::live_codex_hook(&window, controller.clone(), PathBuf::from(path))
        });
    let responsiveness_timer = std::env::args()
        .position(|arg| arg == "--verify-responsiveness")
        .and_then(|index| std::env::args().nth(index + 1))
        .map(|path| native_smoke::responsiveness(&window, controller.clone(), PathBuf::from(path)));
    slint::run_event_loop()?;
    let verified = tools_timer.as_ref().is_none_or(|run| run.passed())
        && workspace_timer.as_ref().is_none_or(|run| run.passed())
        && verification_timer.as_ref().is_none_or(|run| run.passed())
        && menu_timer.as_ref().is_none_or(|run| run.passed())
        && close_timer.as_ref().is_none_or(|run| run.passed())
        && typing_timer.as_ref().is_none_or(|run| run.passed())
        && agent_timer.as_ref().is_none_or(|run| run.passed())
        && live_timer.as_ref().is_none_or(|run| run.passed())
        && responsiveness_timer.as_ref().is_none_or(|run| run.passed())
;
    drop(tools_timer);
    drop(workspace_timer);
    drop(responsiveness_timer);
    drop(live_timer);
    drop(agent_timer);
    drop(typing_timer);
    drop(close_timer);
    drop(menu_timer);
    drop(verification_timer);
    timer.stop();
    signal_timer.stop();
    save_timer.stop();
    {
        let owner = controller.borrow();
        if let Some(tx) = &owner.save_tx {
            if let Ok(bytes) = serde_json::to_vec_pretty(
                &serde_json::json!({"version":1,"desktop":&owner.desktop,"terminals":&owner.launches}),
            ) {
                let _ = tx.send(bytes);
            }
        }
    }
    controller.borrow_mut().save_tx.take();
    if let Some(worker) = save_worker {
        let _ = worker.join();
    }
    let terminals: Vec<_> = controller
        .borrow_mut()
        .surfaces
        .drain()
        .map(|(_, surface)| surface.terminal)
        .collect();
    let cleanup: Vec<_> = terminals
        .into_iter()
        .map(|terminal| std::thread::spawn(move || terminal.stop(Duration::from_millis(200))))
        .collect();
    for worker in cleanup {
        let _ = worker.join();
    }
    if let Some(bridge) = controller.borrow_mut().bridge.take() {
        if bridge.root.is_absolute() {
            let _ = std::fs::remove_dir_all(&bridge.root);
        }
    }
    if !verified {
        return Err("Native UI smoke failed; inspect the requested output directory".into());
    }
    Ok(())
}
