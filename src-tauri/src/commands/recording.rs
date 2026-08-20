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

/// Changes the mode of the recording that is already running.
///
/// Going to `conversation` opens a microphone that was never open, which costs a
/// sub-second gap in the system channel — the tap's aggregate device has to be destroyed
/// before an input device will open promptly. Going the other way closes the microphone
/// outright rather than muting it, so §4's guarantee holds for the rest of the session.
///
/// `events.mode` is corrected here rather than at stop, because it decides how many
/// channels transcription reads and the recording could end in a way that never reaches
/// [`stop_recording`] — a crash, a lid closing. Recovery would then find a stereo file
/// still marked `listen_only` and transcribe the silent left channel as the whole meeting.
#[tauri::command]
pub fn switch_mode(
    app: AppHandle,
    state: State<'_, AppState>,
    mode: String,
) -> CommandResult<LiveStatus> {
    let wanted = parse_mode(&mode)?;

    let mut active = state.lock_recording()?;
    let session = active.as_mut().ok_or_else(|| CommandError {
        message: "nothing is recording, so there is no mode to change".to_string(),
    })?;

    let event_id = session.event_id;
    let settled = session.recording.set_mode(wanted)?;
    let has_mic_channel = session.recording.has_mic_channel();
    let status = session.recording.status();
    drop(active);

    // The file is stereo from the first switch onwards and cannot be narrowed again, so
    // this only ever moves one way.
    if has_mic_channel {
        state.db.set_mode(event_id, Mode::Conversation.as_str())?;
    }

    crate::tray::refresh(&app);
    let _ = app.emit(
        "recording-mode",
        serde_json::json!({
            "event_id": event_id,
            "mode": settled.as_str(),
        }),
    );

    if status.system_audio_lost {
        // The microphone is recording and the other half of the meeting is not. Said out
        // loud, now, while there is still something the user can do about it.
        tracing::error!(
            "recording {event_id} lost system audio when the mode was switched; only the \
             microphone is being recorded"
        );
        let _ = app.emit(
            "recording-system-audio-lost",
            serde_json::json!({ "event_id": event_id }),
        );
    }

    Ok(LiveStatus {
        recording: true,
        event_id: Some(event_id),
        mode: Some(settled.as_str().to_string()),
        status,
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
    let outcome = session.recording.stop()?;
    // What the *file* is, not what the session ended as. A recording that gained a
    // microphone and then closed it again ends in listen-only with a stereo file, and
    // transcribing that as one mono channel would read the silent left channel as the
    // whole meeting.
    let mode = outcome.layout_mode();

    state.db.finish_event(event_id, Utc::now())?;
    state.db.set_mode(event_id, mode.as_str())?;

    // Every muted stretch is persisted so the transcript can say so out loud. A silent
    // gap with no marker would read as "nobody spoke".
    if !outcome.mute_spans.is_empty() {
        state.db.replace_mute_spans(event_id, &outcome.mute_spans)?;
        tracing::info!(
            "recording {event_id} had {} muted span(s)",
            outcome.mute_spans.len()
        );
    }

    // The stretches with no microphone at all, and the sub-second gaps in system audio
    // that opening one costs. Both are only ever present when the mode changed mid
    // recording, and both are missing speech that the file cannot account for on its own.
    let capture_spans: Vec<(&str, i64, i64)> = outcome
        .no_microphone_spans
        .iter()
        .map(|(start, end)| (hearsay_core::db::NO_MICROPHONE, *start, *end))
        .chain(
            outcome
                .system_gaps
                .iter()
                .map(|(start, end)| (hearsay_core::db::SYSTEM_AUDIO_GAP, *start, *end)),
        )
        .collect();
    if !capture_spans.is_empty() {
        state.db.replace_capture_spans(event_id, &capture_spans)?;
        tracing::info!(
            "recording {event_id} changed mode; {} stretch(es) had a channel missing",
            capture_spans.len()
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
