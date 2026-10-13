//! UI-independent profile, installation and terminal infrastructure.

pub mod domain;
pub mod error;
pub mod profiles;
pub mod service;
pub mod storage;
pub mod system;
pub mod terminal;
pub mod tools;

pub use error::{Error, ErrorKind, Result};
pub use service::BackendService;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
