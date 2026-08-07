//! Reading, renaming, searching, and deleting recordings.

use crate::state::{AppState, CommandError, CommandResult};
use hearsay_core::db::{Event, MuteSpan, SearchHit, Segment};
use serde::Serialize;
use tauri::State;

/// Everything the detail pane needs, in one round trip.
#[derive(Debug, Serialize)]
pub struct EventDetail {
    pub event: Event,
    pub segments: Vec<Segment>,
    pub mute_spans: Vec<MuteSpan>,
}

#[tauri::command]
pub fn list_events(state: State<'_, AppState>) -> CommandResult<Vec<Event>> {
    Ok(state.db.events()?)
}

#[tauri::command]
pub fn event_detail(state: State<'_, AppState>, event_id: i64) -> CommandResult<EventDetail> {
    let event = state.db.event(event_id)?.ok_or_else(|| CommandError {
        message: format!("no recording with id {event_id}"),
    })?;
    Ok(EventDetail {
        segments: state.db.segments(event_id)?,
        mute_spans: state.db.mute_spans(event_id)?,
        event,
    })
}

#[tauri::command]
pub fn rename_event(
    state: State<'_, AppState>,
    event_id: i64,
    title: String,
) -> CommandResult<()> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err(CommandError {
            message: "a recording needs a name".to_string(),
        });
    }
    Ok(state.db.rename_event(event_id, trimmed)?)
}

#[tauri::command]
pub fn search_transcripts(
    state: State<'_, AppState>,
    query: String,
    limit: Option<usize>,
) -> CommandResult<Vec<SearchHit>> {
    Ok(state.db.search(&query, limit.unwrap_or(60))?)
}

/// Deletes a recording, its transcript, and its audio file.
///
/// The audio file goes too — leaving it behind would mean "delete" quietly kept the most
/// sensitive part. If the file cannot be removed the database row is still deleted and
/// the problem is reported, rather than leaving a row pointing at nothing.
#[tauri::command]
pub fn delete_event(state: State<'_, AppState>, event_id: i64) -> CommandResult<()> {
    if state.is_recording() {
        // The path of a running session is live; deleting it out from under the writer
        // would leave a half-written file and a confused recorder.
        return Err(CommandError {
            message: "stop the current recording before deleting anything".to_string(),
        });
    }

    let event = state.db.event(event_id)?.ok_or_else(|| CommandError {
        message: format!("no recording with id {event_id}"),
    })?;

    let mut audio_problem = None;
    if let Some(path) = &event.audio_path {
        let path = std::path::Path::new(path);
        if path.exists() {
            if let Err(error) = std::fs::remove_file(path) {
                audio_problem = Some(format!("{}: {error}", path.display()));
            }
        }
    }

    state.db.delete_event(event_id)?;

    match audio_problem {
        Some(problem) => Err(CommandError {
            message: format!("the recording was deleted but its audio file remains — {problem}"),
        }),
        None => Ok(()),
    }
}
