//! Agent Q worker connector library.

pub mod artifact_client;
pub mod backend;
pub mod backoff;
pub mod checks;
pub mod client;
pub mod clock;
pub mod config;
pub mod controller;
pub mod execution_client;
pub mod fixture;
#[cfg(feature = "synthetic-runtime")]
pub mod journal;
#[cfg(feature = "synthetic-runtime")]
pub mod runtime;

#[cfg(feature = "synthetic-runtime")]
pub mod driver;

pub mod fault;
pub mod guardian;
/// H5 physical suspension and continuation bootstrap over CON-03 ports.
pub mod hibernation;

pub mod sandbox_driver;
pub mod source;
#[cfg(feature = "synthetic-runtime")]
mod source_driver;
pub mod source_transfer;
pub mod transfer_client;

pub mod native;

pub mod template;

/// What this worker reports as its version: the package version, `+`, and
/// the source revision it was built from (see `build.rs`), for example
/// `0.5.1+a1b2c3d4e5f6`. At most 64 characters, the server's limit.
pub const VERSION: &str = env!("AQ_WORKER_VERSION");
