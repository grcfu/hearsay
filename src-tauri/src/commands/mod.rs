//! Commands the frontend can invoke.
//!
//! Submodules are public and referenced by full path in `generate_handler!`. Tauri's
//! macro generates hidden items alongside each command, and a `pub use` re-export leaves
//! those behind.

pub mod calendar;
pub mod events;
pub mod mute;
pub mod recording;
pub mod scrub;
pub mod settings;
pub mod summary;
pub mod system;
pub mod version;
pub mod transcription;
