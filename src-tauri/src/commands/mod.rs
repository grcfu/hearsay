//! Commands the frontend can invoke.
//!
//! Submodules are public and referenced by full path in `generate_handler!`. Tauri's
//! macro generates hidden items alongside each command, and a `pub use` re-export leaves
//! those behind.

pub mod system;
