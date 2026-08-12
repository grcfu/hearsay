//! Starting, watching, and stopping a recording.
//!
//! One session at a time. Two concurrent recordings would contend for the same tap and
//! produce two half-recordings of the same meeting, so starting a second one is refused
//! rather than queued.

use crate::state::{AppState, CommandError, CommandResult};
use chrono::Utc;
use hearsay_audio::{Mode, Recording, RecordingStatus, TapTarget};
use hearsay_core::db::Event;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

/// What the frontend asks for when starting a recording.
#[derive(Debug, Deserialize)]
pub struct StartRequest {
    pub mode: String,
    /// Processes to tap. Empty means system-wide.
    #[serde(default)]
    pub pids: Vec<i32>,
    /// What to call the recording, for the app being captured.
    #[serde(default)]
    pub source_name: Option<String>,
}

/// Live state, polled by the UI while recording.
#[derive(Debug, Serialize)]
pub struct LiveStatus {
    pub recording: bool,
    pub event_id: Option<i64>,
    pub mode: Option<String>,
    #[serde(flatten)]
    pub status: RecordingStatus,
}

fn parse_mode(raw: &str) -> CommandResult<Mode> {
    match raw {
        "listen_only" => Ok(Mode::ListenOnly),
        "conversation" => Ok(Mode::Conversation),
        other => Err(CommandError {
            message: format!("unknown recording mode {other:?}"),
        }),
    }
}

#[tauri::command]
pub fn start_recording(
    app: AppHandle,
    state: State<'_, AppState>,
    request: StartRequest,
) -> CommandResult<Event> {
    let mode = parse_mode(&request.mode)?;

    let mut active = state.lock_recording()?;
    if active.is_some() {
        return Err(CommandError {
            message: "a recording is already running".to_string(),
        });
    }

    let started_at = Utc::now();
    let title = default_title(&request, started_at);

    // The event row exists before the first sample does, so a recording that ends badly
    // still has somewhere for its audio to be found.
    let event_id = state
        .db
        .create_event(&title, mode.as_str(), started_at, None, None)?;

    let filename = format!("{}-{event_id}.wav", started_at.format("%Y%m%d-%H%M%S"));
    let path = hearsay_core::paths::recordings_dir()?.join(filename);

    let target = if request.pids.is_empty() {
        TapTarget::SystemWide
    } else {
        TapTarget::Processes(request.pids.clone())
    };

    let recording = match Recording::start(mode, target, &path) {
        Ok(recording) => recording,
        Err(error) => {
            // Nothing was captured, so leave no empty event behind to puzzle over.
            let _ = state.db.delete_event(event_id);
            return Err(error.into());
        }
    };

    state
        .db
        .set_audio_path(event_id, &path.to_string_lossy())?;
    *active = Some(super::super::state::ActiveRecording {
        event_id,
        recording,
    });
    drop(active);
    crate::tray::refresh(&app);

    state
        .db
        .event(event_id)?
        .ok_or_else(|| CommandError {
            message: "the recording was created but could not be read back".to_string(),
        })
}

#[tauri::command]
pub fn recording_status(state: State<'_, AppState>) -> CommandResult<LiveStatus> {
    let active = state.lock_recording()?;
    Ok(match active.as_ref() {
        Some(session) => LiveStatus {
            recording: true,
            event_id: Some(session.event_id),
            mode: Some(session.recording.mode().as_str().to_string()),
            status: session.recording.status(),
        },
        None => LiveStatus {
            recording: false,
            event_id: None,
            mode: None,
            status: RecordingStatus::default(),
        },
    })
}

/// Stops the recording, finalises the file, and starts transcription in the background.
///
/// Transcription is not awaited: it takes minutes on a long meeting, and the recording
/// is safely on disk the moment this returns. Progress arrives as `transcription`
/// events.
#[tauri::command]
pub fn stop_recording(app: AppHandle, state: State<'_, AppState>) -> CommandResult<Event> {
    let session = {
        let mut active = state.lock_recording()?;
        active.take().ok_or_else(|| CommandError {
            message: "nothing is recording".to_string(),
        })?
    };

    let event_id = session.event_id;
    let mode = session.recording.mode();
    let outcome = session.recording.stop()?;

    state.db.finish_event(event_id, Utc::now())?;

    // Every muted stretch is persisted so the transcript can say so out loud. A silent
    // gap with no marker would read as "nobody spoke".
    if !outcome.mute_spans.is_empty() {
        state.db.replace_mute_spans(event_id, &outcome.mute_spans)?;
        tracing::info!(
            "recording {event_id} had {} muted span(s)",
            outcome.mute_spans.len()
        );
    }
    crate::tray::refresh(&app);

    if !outcome.produced_audio {
        // Never let this pass quietly. A file full of zeros is a failed recording, and
        // the user has to hear about it while they still remember the meeting.
        tracing::error!(
            "recording {event_id} captured {} frames, every one of them silent",
            outcome.frames
        );
        let _ = app.emit(
            "recording-silent",
            serde_json::json!({
                "event_id": event_id,
                "frames": outcome.frames,
            }),
        );
    }

    // Audio the mixer had to drop is the one loss with nothing to show for it: no marker
    // in the transcript, no gap in the timeline, just missing speech. Report it for the
    // same reason a muted span is written down — a silent omission is the bug.
    let dropped_ms = outcome.dropped_ms();
    if dropped_ms > 0 {
        tracing::warn!(
            "recording {event_id} dropped {dropped_ms} ms of captured audio; \
             the writer could not keep up"
        );
        let _ = app.emit(
            "recording-dropped-audio",
            serde_json::json!({
                "event_id": event_id,
                "dropped_ms": dropped_ms,
            }),
        );
    }

    let event = state.db.event(event_id)?.ok_or_else(|| CommandError {
        message: "the recording finished but could not be read back".to_string(),
    })?;

    if outcome.frames > 0 {
        super::transcription::spawn_transcription(
            app,
            state.db.clone(),
            event_id,
            outcome.path,
            mode,
        );
    }

    Ok(event)
}

/// A default name for a recording: the app being captured, or the time of day.
///
/// Deliberately something a person would recognise. The model may propose a better title
/// later, but that never overwrites this one.
fn default_title(request: &StartRequest, started_at: chrono::DateTime<Utc>) -> String {
    match request.source_name.as_deref() {
        Some(name) if !name.trim().is_empty() => name.trim().to_string(),
        _ => format!(
            "Recording, {}",
            started_at
                .with_timezone(&chrono::Local)
                .format("%-d %b at %-I:%M %p")
        ),
    }
}
