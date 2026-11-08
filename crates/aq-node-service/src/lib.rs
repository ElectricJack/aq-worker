//! The worker as a service (spec 0007): configuration, state layout, the server
//! link, the aq-git child, the slot supervisor and the OS service layer. It
//! knows nothing about the native pipeline; `aq-worker` supplies a `SlotBackend`.

pub mod config;
pub mod enrollment_file;
pub mod git;
pub mod layout;
pub mod link;
pub mod supervisor;

pub use config::{AqGitConfig, MeasurementConfig, RepositoryBinding, ServiceConfig};
pub use enrollment_file::EnrollmentFile;
pub use git::AqGit;
pub use layout::{Layout, write_machine_json};
pub use link::{LinkError, ServerLink, default_machine_key};
pub use supervisor::{
    AqGitFactory, AttemptOutcome, BACKOFF_MAX, BACKOFF_START, CONTROL_INTERVAL, ControlSource,
    GitChild, GitFactory, QUIET_WINDOW, RecoverOutcome, SlotBackend, SlotContext, Supervisor,
    mark_handoff, running_attempts,
};
