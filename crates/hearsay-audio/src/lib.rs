//! Audio capture, mixing, and on-disk representation for Hearsay.
//!
//! This crate owns everything between "a process is making sound" and "there is a
//! finished WAV file on disk". The Swift helper in `helper/` is deliberately dumb —
//! all buffering, policy, muting, and file writing lives here.
//!
//! Errors in this crate are concrete ([`AudioError`]); callers at the application
//! boundary are expected to wrap them in `anyhow`.

pub mod echo;
pub mod helper;
pub mod mic;
pub mod mix;
pub mod mixer;
pub mod process;
pub mod recorder;
pub mod source;
pub mod wav;

pub use echo::{detect_bleed, EchoDetection};
pub use helper::{HelperEvent, HelperSource, HelperStatus, TapTarget};
pub use process::{AudibleApp, AudioProcess};
pub use mic::MicSource;
pub use mixer::Mixer;
pub use recorder::{Recording, RecordingOutcome, RecordingStatus};
pub use source::{AudioFormat, AudioSource, Chunk};
pub use wav::WavWriter;

use thiserror::Error;

/// Everything that can go wrong in the audio path.
#[derive(Debug, Error)]
pub enum AudioError {
    #[error("audio helper is not available at {path}")]
    HelperMissing { path: String },

    #[error("audio helper exited with status {status}: {stderr}")]
    HelperFailed { status: i32, stderr: String },

    #[error("audio helper produced no usable format description")]
    HelperNoFormat,

    #[error("permission to capture system audio was denied")]
    PermissionDenied,

    #[error("no audio-producing process matched the request")]
    NoSuchProcess,

    #[error("no microphone is available. Conversation mode needs an input device; \
             listen-only mode does not.")]
    NoInputDevice,

    #[error("{message}")]
    InputFailed { message: String },

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result alias for the audio layer.
pub type Result<T> = std::result::Result<T, AudioError>;

/// Which recording mode a session is running in.
///
/// [`Mode::ListenOnly`] is the default on every launch and is *never* persisted.
/// In that mode the microphone is not opened at all — see `CLAUDE.md` §4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// System audio only. The microphone is never instantiated.
    ListenOnly,
    /// Microphone (left) plus system audio (right), written as one stereo WAV.
    Conversation,
}

impl Default for Mode {
    fn default() -> Self {
        Mode::ListenOnly
    }
}

impl Mode {
    /// Number of channels written to the output WAV for this mode.
    pub fn channel_count(self) -> u16 {
        match self {
            Mode::ListenOnly => 1,
            Mode::Conversation => 2,
        }
    }

    /// Whether this mode opens the microphone. Used as a guard at every call site
    /// that could construct an input device.
    pub fn opens_microphone(self) -> bool {
        matches!(self, Mode::Conversation)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::ListenOnly => "listen_only",
            Mode::Conversation => "conversation",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_only_is_the_default() {
        assert_eq!(Mode::default(), Mode::ListenOnly);
    }

    #[test]
    fn listen_only_never_opens_the_microphone() {
        assert!(!Mode::ListenOnly.opens_microphone());
        assert!(Mode::Conversation.opens_microphone());
    }

    #[test]
    fn channel_counts_match_modes() {
        assert_eq!(Mode::ListenOnly.channel_count(), 1);
        assert_eq!(Mode::Conversation.channel_count(), 2);
    }
}
