//! Application core for Hearsay: storage, transcription driving, summaries, secrets.
//!
//! The Tauri layer above this crate is intentionally thin — it maps commands to
//! functions here and owns no business logic.

pub mod calendar;
pub mod chat;
pub mod db;
pub mod dedupe;
pub mod export;
pub mod paths;
pub mod secrets;
pub mod storage;
pub mod summary;
pub mod transcribe;

pub use db::{CaptureSpan, ChatMessage, Database, Event, MuteSpan, NewSegment, Segment};
pub use summary::Summary;
pub use hearsay_audio::Mode;
pub use transcribe::{transcribe_recording, Channel, TranscriptSegment, TranscriptionResult};
