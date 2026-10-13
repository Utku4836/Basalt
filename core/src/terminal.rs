//! Terminals: the screen model and its decoder, the native PTY that feeds it, and
//! profile-owned terminal sessions.

#[cfg(windows)]
pub mod pty;
pub mod screen;
#[cfg(windows)]
pub mod session;

pub use screen::*;
