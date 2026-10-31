//! Pure format adapters and an independent bounded spool. Native observations
//! are evidence claims, never live authority, task outcomes or invoice facts.
pub use aq_lifecycle_protocol as lifecycle;
mod boot;
mod capabilities;
pub use boot::*;
pub mod discovery;
mod formats;
pub use capabilities::*;
pub mod framing;
mod json;
mod normalize;
mod reparse;
pub use reparse::*;
pub mod source;
pub mod spool;
pub use normalize::*;
