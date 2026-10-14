//! Pure, bounded GP-200 normal SysEx codec.
//!
//! This crate does not open MIDI devices. [`RawTransfer`] keeps ordered record
//! bytes authoritative; framing, incremental streaming and reassembly retain
//! unknown commands. Hardware actors must match responses themselves: the wire
//! header has no observed transaction identifier.
mod assignments;
mod firmware;
mod frame;
mod globals;
mod mutations;
mod raw;
mod reassembly;
mod requests;
mod stream;
pub use assignments::*;
pub use firmware::*;
pub use frame::*;
pub use globals::*;
pub use mutations::*;
pub use raw::*;
pub use reassembly::*;
pub use requests::*;
pub use stream::*;
mod assets;
pub use assets::*;
