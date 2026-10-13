//! Basalt desktop: Slint UI, workspace state and terminal runtime glue.
#[path = "runtime/lifecycle.rs"]
pub mod agent_bridge;
#[path = "runtime/agent_processes.rs"]
pub mod agents;
#[path = "runtime/agent_screen.rs"]
pub mod agent_screen;
#[path = "ui_state/canvas_layout.rs"]
pub mod canvas;
#[path = "ui_state/commands.rs"]
pub mod commands;
#[path = "rendering/display.rs"]
pub mod display;
#[cfg(windows)]
#[path = "runtime/cli_engine.rs"]
pub mod launcher;
pub mod preferences;
#[path = "ui_state/palette.rs"]
pub mod palette;
#[path = "ui_state/model_projection.rs"]
pub mod projection;
#[path = "rendering/gpu/mod.rs"]
pub mod gpu;
#[path = "rendering/terminal_frame.rs"]
pub mod screen;
#[path = "ui_state/split_layout.rs"]
pub mod split;
#[path = "ui_state/tabs.rs"]
pub mod tabs;
#[path = "rendering/typography.rs"]
pub mod typography;
#[path = "rendering/viewport.rs"]
pub mod viewport;
#[path = "ui_state/workspaces.rs"]
pub mod workspace;
#[path = "ui_state/workspace_store.rs"]
pub mod workspace_store;
slint::include_modules!();

pub mod clipboard;

#[path = "runtime/agent_summary.rs"]
pub mod agent_summary;

#[path = "runtime/command_line.rs"]
pub mod command_line;

#[path = "runtime/flags.rs"]
pub mod flags;

#[path = "runtime/packages.rs"]
pub mod packages;

#[path = "runtime/folders.rs"]
pub mod folders;
#[path = "runtime/sources.rs"]
pub mod sources;

#[path = "runtime/history.rs"]
pub mod history;

#[path = "runtime/git_branch.rs"]
pub mod git;

#[path = "runtime/shell.rs"]
pub mod shell;
