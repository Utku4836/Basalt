//! Profiles: the profile store, the environment a profile gives a CLI, private data
//! sharing between profiles, credentials and the per-profile activity book.

pub mod activity;
pub mod credentials;
pub mod environment;
pub mod sharing;
pub mod store;

pub use store::*;
