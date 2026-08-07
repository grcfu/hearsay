//! Running transcription in the background and reporting progress.
//!
//! Transcription is slow — minutes for a long meeting — and entirely detached from the
//! recording that produced it. It runs on its own thread, reports progress as events,
//! and writes segments when it finishes. Nothing about the app is blocked while it runs,
//! and a failure leaves the recording itself untouched.

use crate::state::{AppState, CommandError, CommandResult};
use hearsay_audio::Mode;
use hearsay_core::db::NewSegment;
use hearsay_core::transcribe::{transcribe_recording, TranscribeEvent, DEFAULT_MODEL};
use hearsay_core::Database;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

/// Starts transcription on a worker thread.
pub fn spawn_transcription(
    app: AppHandle,
    db: Arc<Database>,
    event_id: i64,
    audio_path: PathBuf,
    mode: Mode,
) {
    std::thread::Builder::new()
        .name(format!("hearsay-transcribe-{event_id}"))
        .spawn(move || {
            if let Err(error) = run(&app, &db, event_id, &audio_path, mode) {
                tracing::error!("transcription of {event_id} failed: {error:#}");
                let _ = app.emit(
                    "transcription",
                    serde_json::json!({
                        "event_id": event_id,
                        "stage": "failed",
                        "message": format!("{error:#}"),
                    }),
                );
            }
        })
        .map(|_| ())
        .unwrap_or_else(|error| {
            tracing::error!("could not start the transcription thread: {error}");
        });
}

fn run(
    app: &AppHandle,
    db: &Database,
    event_id: i64,
    audio_path: &std::path::Path,
    mode: Mode,
) -> anyhow::Result<()> {
    let models_dir = hearsay_core::paths::models_dir()?;

    let _ = app.emit(
        "transcription",
        serde_json::json!({ "event_id": event_id, "stage": "started" }),
    );

    let segments = transcribe_recording(audio_path, mode, DEFAULT_MODEL, &models_dir, |event| {
        let payload = match &event {
            TranscribeEvent::Download { file, percent } => serde_json::json!({
                "event_id": event_id, "stage": "downloading",
                "file": file, "percent": percent,
            }),
            TranscribeEvent::DownloadDone | TranscribeEvent::ModelReady => serde_json::json!({
                "event_id": event_id, "stage": "model_ready",
            }),
            TranscribeEvent::Progress { channel, percent } => serde_json::json!({
                "event_id": event_id, "stage": "transcribing",
                "channel": channel, "percent": percent,
            }),
            TranscribeEvent::Done { channel, segments } => serde_json::json!({
                "event_id": event_id, "stage": "channel_done",
                "channel": channel, "segments": segments,
            }),
            TranscribeEvent::Error { kind, message } => serde_json::json!({
                "event_id": event_id, "stage": "failed",
                "kind": kind, "message": message,
            }),
            TranscribeEvent::Log { line } => {
                tracing::debug!("transcribe: {line}");
                return;
            }
        };
        let _ = app.emit("transcription", payload);
    })?;

    // Drop microphone lines that are echoes of the other party. Only meaningful in
    // conversation mode; a listen-only transcript has one channel and nothing to compare.
    let (segments, echoes_dropped) = if mode.opens_microphone() {
        hearsay_core::dedupe::drop_echoed_segments(segments)
    } else {
        (segments, 0)
    };
    if echoes_dropped > 0 {
        tracing::info!("dropped {echoes_dropped} echoed mic segment(s) from {event_id}");
    }

    let rows: Vec<NewSegment> = segments
        .into_iter()
        .map(|segment| NewSegment {
            channel: segment.channel,
            start_ms: segment.start_ms,
            end_ms: segment.end_ms,
            text: segment.text,
        })
        .collect();

    let count = rows.len();
    db.replace_segments(event_id, &rows)?;

    let _ = app.emit(
        "transcription",
        serde_json::json!({
            "event_id": event_id, "stage": "done", "segments": count,
            "echoes_dropped": echoes_dropped,
        }),
    );
    Ok(())
}

/// Re-runs transcription for a recording that already exists.
///
/// Useful after setting up the Python sidecar for the first time, or when a recording
/// was made before it was installed.
#[tauri::command]
pub fn retranscribe(
    app: AppHandle,
    state: State<'_, AppState>,
    event_id: i64,
) -> CommandResult<()> {
    let event = state.db.event(event_id)?.ok_or_else(|| CommandError {
        message: format!("no recording with id {event_id}"),
    })?;

    let path = event.audio_path.clone().ok_or_else(|| CommandError {
        message: "this recording has no audio file".to_string(),
    })?;
    let path = PathBuf::from(path);
    if !path.is_file() {
        return Err(CommandError {
            message: format!("the audio file is missing: {}", path.display()),
        });
    }

    let mode = if event.mode == "conversation" {
        Mode::Conversation
    } else {
        Mode::ListenOnly
    };

    spawn_transcription(app, state.db.clone(), event_id, path, mode);
    Ok(())
}
