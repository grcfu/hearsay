//! The retroactive scrub.
//!
//! Mute helps someone who thought to press it in advance. This is the other case: the
//! side conversation you only realised was sensitive once it had started. ⌘⇧X erases
//! everything the microphone has captured in the last minute that has not yet reached
//! the file.
//!
//! It is honest about its limit. Audio older than the scrub window is already on disk and
//! cannot be recalled, so the result says how much was actually erased rather than
//! implying the whole conversation is gone.

use crate::state::{AppState, CommandError, CommandResult};
use hearsay_audio::mixer::SCRUB_WINDOW_SECONDS;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

/// What a scrub actually did.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ScrubResult {
    /// Milliseconds of microphone audio erased before it reached the file.
    pub erased_ms: u64,
    /// The furthest back a scrub can reach, in seconds.
    pub window_seconds: u32,
    /// False in listen-only mode, where the microphone was never opened.
    pub applicable: bool,
}

#[tauri::command]
pub fn scrub_microphone(app: AppHandle, state: State<'_, AppState>) -> CommandResult<ScrubResult> {
    scrub_now(&app, &state)
}

/// Samples to milliseconds. The mixer holds one channel of mono microphone audio, so
/// samples and frames are the same thing here.
fn frames_to_ms(frames: usize) -> u64 {
    // Sources negotiate their own rate; 48 kHz is what every Core Audio tap on this
    // target reports, and the figure is only ever shown to a human as "about N seconds".
    (frames as u64) * 1000 / 48_000
}

/// Handles the ⌘⇧X global shortcut.
///
/// Quiet when it does not apply: pressing it outside a conversation recording is a
/// logged no-op, not a dialog thrown over whatever the user is actually doing.
pub fn scrub_from_shortcut(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    match scrub_now(app, &state) {
        Ok(result) => tracing::info!(
            "scrubbed {} ms of microphone audio by hotkey",
            result.erased_ms
        ),
        Err(error) => tracing::info!("scrub hotkey ignored: {}", error.message),
    }
}

fn scrub_now(app: &AppHandle, state: &AppState) -> CommandResult<ScrubResult> {
    let active = state.lock_recording()?;
    let session = active.as_ref().ok_or_else(|| CommandError {
        message: "nothing is recording".to_string(),
    })?;
    if !session.recording.mode().opens_microphone() {
        return Err(CommandError {
            message: "this recording is listen-only, so the microphone was never opened \
                      — there is nothing to erase."
                .to_string(),
        });
    }

    let erased = session.recording.scrub_microphone()?;
    let result = ScrubResult {
        erased_ms: frames_to_ms(erased),
        window_seconds: SCRUB_WINDOW_SECONDS,
        applicable: true,
    };
    drop(active);

    let _ = app.emit("scrub", result);
    Ok(result)
}
