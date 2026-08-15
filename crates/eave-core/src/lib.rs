//! eave-core: engine-side session lifecycle for driving coding-agent CLIs.
//!
//! Stage A of the v0 slice (docs/spec/04-slice.md): spawn, stream-JSON parse,
//! process-group identity, verified kill. No UI, no journal, no wire protocol.

pub mod claude;
pub mod events;
pub mod proc;
pub mod process;
pub mod session;
pub mod stream;
pub mod thresholds;
