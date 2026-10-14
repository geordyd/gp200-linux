//! Real-codec simulator and explicit capture replay; no physical MIDI access.
//! The older domain-level demo device remains a separately named unit fixture.
mod backend;
mod device;
mod replay;
mod scenario;
mod wire;
pub use backend::*;
pub use device::*;
pub use replay::*;
pub use scenario::*;
pub use wire::*;
