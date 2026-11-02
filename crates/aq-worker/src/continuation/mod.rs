//! Suspension history and finite retention proposals. Serialized records never
//! construct execution, stop, a central hold, or verified retention authority.
pub(crate) mod advance;
pub mod history;
#[cfg(feature = "test-support")]
pub mod orchestration;

pub mod source;

#[cfg(test)]
pub(crate) mod synthetic_retention;
#[cfg(test)]
mod tests;
pub mod transfer;
