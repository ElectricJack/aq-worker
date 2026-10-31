//! Candidate storage-neutral normalization over bounded indexed reads and
//! tentative deltas. Wire claims never confer authority or invoice truth.
pub use aq_lifecycle_protocol as lifecycle;
mod capabilities;
mod formats;
pub use capabilities::*;
pub mod framing;
mod json;
mod normalize;
pub mod rows;
pub use normalize::*;
