//! Generating and regenerating summaries.
//!
//! Runs on a worker thread and reports through the `summary` event, because a long
//! meeting can take a minute or more and the window must stay responsive. Summaries are
//! derived: regenerating one never touches the transcript or the audio.

use crate::state::{AppState, CommandError, CommandResult};
use hearsay_core::summary::{self, Provider};
use hearsay_core::Database;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

/// Starts summary generation for an event.
///
/// Returns as soon as the work is queued. Progress and the result arrive as `summary`
/// events so the UI can show them without blocking.
#[tauri::command]
pub fn generate_summary(
    app: AppHandle,
    state: State<'_, AppState>,
    event_id: i64,
) -> CommandResult<()> {
    if !summary::is_available() {
        return Err(CommandError {
            message: "no Anthropic API key is set. Add one in settings — recording, \
                      transcription and search all work without it."
                .to_string(),
        });
    }

    let segments = state.db.segments(event_id)?;
    if segments.is_empty() {
        return Err(CommandError {
            message: "this recording has no transcript yet. Summaries are written from \
                      the transcript, so there is nothing to summarise."
                .to_string(),
        });
    }

    let db = state.db.clone();
    std::thread::Builder::new()
        .name(format!("hearsay-summary-{event_id}"))
        .spawn(move || run(&app, &db, event_id))
        .map(|_| ())
        .map_err(|error| CommandError {
            message: format!("could not start summary generation: {error}"),
        })
}

fn run(app: &AppHandle, db: &Arc<Database>, event_id: i64) {
    let _ = app.emit(
        "summary",
        serde_json::json!({ "event_id": event_id, "stage": "started" }),
    );

    let outcome = (|| -> anyhow::Result<()> {
        let segments = db.segments(event_id)?;
        let spans: Vec<(i64, i64)> = db
            .mute_spans(event_id)?
            .into_iter()
            .map(|span| (span.start_ms, span.end_ms))
            .collect();

        // Each provider names its own model; the caller does not choose one.
        let model = Provider::current().default_model();
        let summary = summary::summarize(&segments, &spans, model)?;
        db.set_summary(
            event_id,
            &summary.to_markdown(),
            Some(summary.title.as_str()),
            model,
        )?;
        Ok(())
    })();

    match outcome {
        Ok(()) => {
            let _ = app.emit(
                "summary",
                serde_json::json!({ "event_id": event_id, "stage": "done" }),
            );
        }
        Err(error) => {
            tracing::error!("summary for {event_id} failed: {error:#}");
            let _ = app.emit(
                "summary",
                serde_json::json!({
                    "event_id": event_id,
                    "stage": "failed",
                    // The chain, not just the outermost message — "could not reach the
                    // Anthropic API: dns error" is actionable, "could not reach" is not.
                    "message": format!("{error:#}"),
                }),
            );
        }
    }
}
