//! Suspension history and finite retention proposals. Serialized records never
//! construct execution, stop, a central hold, or verified retention authority.
pub mod history;

pub mod source;

#[cfg(test)]
mod tests;
pub mod transfer;
