//! eave-core: engine-side session lifecycle for driving coding-agent CLIs.
//!
//! Stages A+B of the v0 slice (docs/spec/04-slice.md): spawn, stream-JSON
//! parse, process-group identity, verified kill, append-only journal with
//! crash-tail repair, cold resume. No UI, no wire protocol.

pub mod acp;
pub mod attention;
pub mod capabilities;
pub mod claude;
pub mod codex;
pub mod events;
pub mod grok;
pub mod hermes;
pub mod journal;
pub mod pi;
pub mod proc;
pub mod process;
pub mod provider;
pub mod recorder;
pub mod session;
pub mod store;
pub mod stream;
pub mod thresholds;
pub mod tool;
pub mod turn;
