//! Application state, and the error type every command returns.
//!
//! This layer is deliberately thin. It owns handles — the database, the recorder — and
//! translates between Tauri's world and `hearsay-core`. Business logic that ends up here
//! belongs in a crate instead.

use hearsay_audio::Recording;
use hearsay_core::Database;
use std::sync::{Arc, Mutex, MutexGuard};

/// The session currently being recorded, if any.
pub struct ActiveRecording {
    pub event_id: i64,
    pub recording: Recording,
}

/// Everything the commands need. Held by Tauri and shared across invocations.
pub struct AppState {
    pub db: Arc<Database>,
    /// At most one recording runs at a time. Two would fight over the same tap.
    recording: Mutex<Option<ActiveRecording>>,
}

impl AppState {
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            recording: Mutex::new(None),
        }
    }

    /// Takes the recording lock, turning a poisoned mutex into a reportable error rather
    /// than a second panic.
    pub fn lock_recording(&self) -> CommandResult<MutexGuard<'_, Option<ActiveRecording>>> {
        self.recording.lock().map_err(|_| CommandError {
            message: "the recorder is in an unknown state after an earlier failure. \
                      Restart Hearsay; any audio already written is safe on disk."
                .to_string(),
        })
    }

    /// Whether a recording is running, without holding the lock open.
    pub fn is_recording(&self) -> bool {
        self.recording
            .lock()
            .map(|active| active.is_some())
            .unwrap_or(false)
    }
}

/// The error shape the frontend sees.
///
/// `anyhow::Error` is not serialisable, so it is flattened to a message here. Errors are
/// rendered to the user verbatim, so their text is written for a person rather than for
/// a log — and no error message ever contains an API key.
#[derive(Debug, serde::Serialize)]
pub struct CommandError {
    pub message: String,
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl From<anyhow::Error> for CommandError {
    fn from(error: anyhow::Error) -> Self {
        // The chain matters: "could not start recording: no such process" is useful,
        // "no such process" on its own is not.
        let mut message = error.to_string();
        for cause in error.chain().skip(1) {
            message.push_str(&format!(": {cause}"));
        }
        Self { message }
    }
}

impl From<hearsay_audio::AudioError> for CommandError {
    fn from(error: hearsay_audio::AudioError) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

impl From<std::io::Error> for CommandError {
    fn from(error: std::io::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

/// Shorthand for a command's return type.
pub type CommandResult<T> = std::result::Result<T, CommandError>;
