//! Application core for Hearsay: storage, transcription driving, summaries, secrets.
//!
//! The Tauri layer above this crate is intentionally thin — it maps commands to
//! functions here and owns no business logic.

pub mod paths;

pub use hearsay_audio::Mode;
