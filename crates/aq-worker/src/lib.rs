//! Agent Q worker connector library.

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

pub mod source;
#[cfg(feature = "synthetic-runtime")]
mod source_driver;
pub mod source_transfer;
pub mod transfer_client;

pub mod native;

/// Finite local-provider fixture data; never runtime/model qualification.
#[cfg(any(test, feature = "test-support"))]
pub mod finite_responses_fixture;
pub mod template;
