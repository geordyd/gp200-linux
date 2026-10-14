//! Pure application reducer. Effects are interpreted by GUI/service adapters;
//! no MIDI, filesystem, clock or GUI work takes place in this crate.
mod controller;
mod external_changes;
mod firmware_control;
mod recovery;
mod semantic;
mod state;
mod validation;
pub use controller::device_document_id;
pub use firmware_control::*;
pub use state::*;
