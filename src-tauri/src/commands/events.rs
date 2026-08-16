//! Reading, renaming, searching, and deleting recordings.

use crate::state::{AppState, CommandError, CommandResult};
use hearsay_core::db::{Event, MuteSpan, SearchHit, Segment};
use hearsay_core::storage;
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

/// What one recording's audio occupies right now.
#[derive(Debug, Serialize)]
pub struct AudioUsage {
    pub event_id: i64,
    pub bytes: u64,
}

/// How much disk each recording's audio is using.
///
/// Separate from `list_events` because it stats a file per recording, and the list is
/// reloaded on every change — a rename should not go and touch the disk. Recordings whose
/// audio is gone, deleted or never written, are left out rather than reported as zero:
/// nothing there is different from nothing to reclaim.
#[tauri::command]
pub fn audio_usage(state: State<'_, AppState>) -> CommandResult<Vec<AudioUsage>> {
    Ok(state
        .db
        .events()?
        .into_iter()
        .filter_map(|event| {
            storage::audio_bytes(event.audio_path.as_deref()).map(|bytes| AudioUsage {
                event_id: event.id,
                bytes,
            })
        })
        .collect())
}

/// How much disk a deletion gave back.
#[derive(Debug, Serialize)]
pub struct ReclaimedAudio {
    pub bytes: u64,
}

/// Deletes a recording's audio, keeping everything written from it.
///
/// The transcript, the summary, the search index and the questions all survive, because
/// none of them are derived from the file — segments are the source of truth and they are
/// stored as rows. What is lost is playback, click-to-seek, saving a copy, and the ability
/// to ever transcribe this recording again.
///
/// That last one is why this refuses more than it strictly has to. The audio is the only
/// thing here that cannot be rebuilt from anything else, so a deletion that leaves nothing
/// behind is not a saving, it is a loss of the whole recording:
///
/// - a recording still being made has a file that is still being written;
/// - a recording with a pass in flight is being read right now, and would fail partway;
/// - a recording that has never been transcribed has nothing to keep, so deleting its
///   audio would throw away everything it was rather than only the largest part.
#[tauri::command]
pub fn delete_audio(state: State<'_, AppState>, event_id: i64) -> CommandResult<ReclaimedAudio> {
    if let Some(active) = state.lock_recording()?.as_ref() {
        if active.event_id == event_id {
            return Err(CommandError {
                message: "this recording is still running — stop it first".to_string(),
            });
        }
    }

    if super::transcription::is_transcribing(event_id) {
        return Err(CommandError {
            message: "this recording is being transcribed right now. Wait for the \
                      transcript, then the audio can go."
                .to_string(),
        });
    }

    let event = state.db.event(event_id)?.ok_or_else(|| CommandError {
        message: format!("no recording with id {event_id}"),
    })?;

    if event.audio_was_deleted() {
        return Err(CommandError {
            message: "the audio for this recording has already been deleted".to_string(),
        });
    }

    let path = event.audio_path.clone().ok_or_else(|| CommandError {
        message: "this recording has no audio file".to_string(),
    })?;

    if event.transcribed_at.is_none() {
        return Err(CommandError {
            message: "this recording has never been transcribed. Deleting its audio now \
                      would leave nothing at all — transcribe it first."
                .to_string(),
        });
    }

    // Measured before the file goes, so the reclaimed figure is the real one rather than
    // an estimate from the recording's duration.
    let bytes = storage::audio_bytes(Some(&path)).unwrap_or(0);

    let file = std::path::Path::new(&path);
    if file.exists() {
        std::fs::remove_file(file).map_err(|error| CommandError {
            message: format!("could not delete the audio file — {}: {error}", file.display()),
        })?;
    }

    // Only once the file is actually gone. Marking first and failing to remove it would
    // orphan the audio: still on disk, taking the same space, with nothing left in the
    // database pointing at it to try again.
    state.db.mark_audio_deleted(event_id)?;

    Ok(ReclaimedAudio { bytes })
}
