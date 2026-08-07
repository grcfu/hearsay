//! Muting the microphone.
//!
//! Only meaningful in conversation mode — in listen-only mode there is no microphone to
//! mute, and saying so is more honest than showing a control that does nothing.
//!
//! Muting writes zeros into the left channel. The input device is never stopped or
//! reopened: the timeline stays continuous, macOS never re-prompts, and the system
//! channel keeps recording throughout.

use crate::state::{AppState, CommandError, CommandResult};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

/// The mute state, as the UI and the menu bar item see it.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct MuteState {
    pub muted: bool,
    /// False in listen-only mode, where there is nothing to mute.
    pub applicable: bool,
}

#[tauri::command]
pub fn mute_state(state: State<'_, AppState>) -> CommandResult<MuteState> {
    let active = state.lock_recording()?;
    Ok(match active.as_ref() {
        Some(session) => MuteState {
            muted: session.recording.is_muted(),
            applicable: session.recording.mode().opens_microphone(),
        },
        None => MuteState {
            muted: false,
            applicable: false,
        },
    })
}

#[tauri::command]
pub fn set_mute(app: AppHandle, state: State<'_, AppState>, muted: bool) -> CommandResult<MuteState> {
    apply(&app, &state, Some(muted))
}

#[tauri::command]
pub fn toggle_mute(app: AppHandle, state: State<'_, AppState>) -> CommandResult<MuteState> {
    apply(&app, &state, None)
}

/// Shared implementation. `None` toggles.
fn apply(app: &AppHandle, state: &AppState, muted: Option<bool>) -> CommandResult<MuteState> {
    let active = state.lock_recording()?;
    let session = active.as_ref().ok_or_else(|| CommandError {
        message: "nothing is recording".to_string(),
    })?;

    if !session.recording.mode().opens_microphone() {
        return Err(CommandError {
            message: "this recording is listen-only, so the microphone was never opened \
                      — there is nothing to mute."
                .to_string(),
        });
    }

    let now = match muted {
        Some(value) => session.recording.set_muted(value)?,
        None => session.recording.toggle_mute()?,
    };

    let result = MuteState {
        muted: now,
        applicable: true,
    };
    drop(active);

    // The hotkey works while the window is unfocused, so the change has to reach the UI
    // and the menu bar without anyone having asked.
    let _ = app.emit("mute", result);
    crate::tray::refresh(app);
    Ok(result)
}

/// Handles the ⌘⇧M global shortcut.
///
/// Deliberately quiet when it does not apply: pressing it outside a conversation
/// recording is a no-op with a log line, not an error dialog over whatever the user is
/// actually doing.
pub fn toggle_from_shortcut(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    match apply(app, &state, None) {
        Ok(result) => tracing::info!(
            "microphone {} by hotkey",
            if result.muted { "muted" } else { "unmuted" }
        ),
        Err(error) => tracing::info!("mute hotkey ignored: {}", error.message),
    }
}
