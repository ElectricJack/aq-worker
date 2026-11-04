//! Scoped AQ consumer. Cached context and local intents never confer graph authority.
pub mod client;
pub mod completion;
pub mod context;
mod files;
pub mod journal;
pub mod mail;
pub mod node;
pub mod provision;
pub use aq_agent_api as contract;
