//! Saving a copy of a recording's audio outside Hearsay.

use crate::state::{AppState, CommandError, CommandResult};
use hearsay_core::export::{export_audio as write_export, suggested_file_name, ExportFormat, Span};
use serde::Serialize;
use std::path::PathBuf;
use tauri::State;

/// Where a copy ended up, and how big it turned out.
#[derive(Debug, Serialize)]
pub struct ExportedAudio {
    pub path: String,
    pub bytes: u64,
}

/// The name to prefill in the save sheet, from the recording's title, date, and span.
///
/// Built here rather than in the webview because it has to survive a filesystem: the title
/// is free text the user typed, and a slash in it would otherwise read as a directory.
#[tauri::command]
pub fn export_file_name(
    state: State<'_, AppState>,
    event_id: i64,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
) -> CommandResult<String> {
    let event = state.db.event(event_id)?.ok_or_else(|| CommandError {
        message: format!("no recording with id {event_id}"),
    })?;

    // `started_at` is RFC 3339, so the date is its first ten characters.
    let date = event
        .started_at
        .to_rfc3339()
        .chars()
        .take(10)
        .collect::<String>();

    Ok(suggested_file_name(
        event.display_title(),
        &date,
        span_from(start_ms, end_ms)?,
        ExportFormat::M4a,
    ))
}

/// Turns a pair of optional bounds from the webview into a span.
///
/// One end given and not the other is a bug in the caller rather than a request, so it is
/// reported instead of being guessed at.
fn span_from(start_ms: Option<i64>, end_ms: Option<i64>) -> CommandResult<Option<Span>> {
    match (start_ms, end_ms) {
        (None, None) => Ok(None),
        (Some(start), Some(end)) => {
            let span = Span::new(start.max(0) as u64, end.max(0) as u64)?;
            Ok(Some(span))
        }
        _ => Err(CommandError {
            message: "a selection needs both a start and an end".to_string(),
        }),
    }
}

/// Writes a copy of the recording's audio to `destination`.
///
/// The format comes from the extension the user typed, so one save sheet covers both the
/// small copy and the original. With `start_ms` and `end_ms` it writes just that span, which
/// is usually what someone wants: the part where the thing was said, not the hour around it.
/// Runs on Tauri's worker pool — a long meeting takes a few seconds to convert, and the
/// window stays live throughout.
#[tauri::command]
pub fn export_audio(
    state: State<'_, AppState>,
    event_id: i64,
    destination: String,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
) -> CommandResult<ExportedAudio> {
    // A live recording's file is still being written, and its header understates its
    // length until the next sync. A copy taken now would be short by an unpredictable
    // amount, with nothing to show which part is missing.
    if let Some(active) = state.lock_recording()?.as_ref() {
        if active.event_id == event_id {
            return Err(CommandError {
                message: "this recording is still running — stop it first, then save a copy"
                    .to_string(),
            });
        }
    }

    let event = state.db.event(event_id)?.ok_or_else(|| CommandError {
        message: format!("no recording with id {event_id}"),
    })?;

    // Said plainly, because the Audio tab hides this button once the audio is gone and
    // reaching here at all means something is out of step — a stale window, most likely.
    if event.audio_was_deleted() {
        return Err(CommandError {
            message: "the audio for this recording was deleted, so there is nothing to \
                      save a copy of. Its transcript is still here."
                .to_string(),
        });
    }

    let source = event
        .audio_path
        .map(PathBuf::from)
        .ok_or_else(|| CommandError {
            message: "this recording has no audio file".to_string(),
        })?;

    let destination = PathBuf::from(destination);
    let bytes = write_export(&source, &destination, span_from(start_ms, end_ms)?)?;

    Ok(ExportedAudio {
        path: destination.to_string_lossy().to_string(),
        bytes,
    })
}
