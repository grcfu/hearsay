//! Asking questions about a recording.
//!
//! Runs on a worker thread and reports through the `chat` event, for the same reason
//! summaries do: the call takes seconds at best and the window must stay usable. The
//! question is stored before the call goes out, so it is still there to retry if the
//! answer never arrives.

use crate::state::{AppState, CommandError, CommandResult};
use hearsay_core::chat::{self, Turn};
use hearsay_core::db::ChatMessage;
use hearsay_core::summary::Provider;
use hearsay_core::Database;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

use super::summary::SPEAKER_NAME_KEY;

/// Everything asked about a recording so far.
#[tauri::command]
pub fn chat_history(state: State<'_, AppState>, event_id: i64) -> CommandResult<Vec<ChatMessage>> {
    Ok(state.db.chat_messages(event_id)?)
}

/// Asks a question about a recording.
///
/// Returns as soon as the work is queued; the answer arrives as a `chat` event. Refuses
/// before sending anything when there is no key or no transcript, so the failure is
/// explained rather than being a request that goes nowhere.
#[tauri::command]
pub fn ask_question(
    app: AppHandle,
    state: State<'_, AppState>,
    event_id: i64,
    question: String,
) -> CommandResult<()> {
    let question = question.trim().to_string();
    if question.is_empty() {
        return Err(CommandError {
            message: "type a question first".to_string(),
        });
    }

    if !hearsay_core::summary::is_available() {
        return Err(CommandError {
            message: "no API key is set. Add one in settings — recording, transcription \
                      and search all work without it."
                .to_string(),
        });
    }

    if state.db.segments(event_id)?.is_empty() {
        return Err(CommandError {
            message: "this recording has no transcript yet. Questions are answered from \
                      the transcript, so there is nothing to search."
                .to_string(),
        });
    }

    // Stored before the call so the question survives a failure and is not retyped.
    let question_id = state.db.add_chat_message(event_id, "user", &question)?;

    let db = state.db.clone();
    std::thread::Builder::new()
        .name(format!("hearsay-chat-{event_id}"))
        .spawn(move || run(&app, &db, event_id, question_id, &question))
        .map(|_| ())
        .map_err(|error| {
            // The thread never started, so nothing will ever answer this question.
            let _ = state.db.delete_chat_message(question_id);
            CommandError {
                message: format!("could not start the request: {error}"),
            }
        })
}

/// Forgets the conversation about a recording. The transcript and audio are untouched.
#[tauri::command]
pub fn clear_chat(state: State<'_, AppState>, event_id: i64) -> CommandResult<()> {
    state.db.clear_chat(event_id)?;
    Ok(())
}

fn run(app: &AppHandle, db: &Arc<Database>, event_id: i64, question_id: i64, question: &str) {
    let _ = app.emit(
        "chat",
        serde_json::json!({ "event_id": event_id, "stage": "asking" }),
    );

    let outcome = (|| -> anyhow::Result<String> {
        let segments = db.segments(event_id)?;
        let spans: Vec<(i64, i64)> = db
            .mute_spans(event_id)?
            .into_iter()
            .map(|span| (span.start_ms, span.end_ms))
            .collect();

        // Everything before this question. The question itself is already stored, so it
        // would otherwise be sent twice.
        let history: Vec<Turn> = db
            .chat_messages(event_id)?
            .into_iter()
            .filter(|message| message.id != question_id)
            .map(|message| Turn {
                role: message.role,
                content: message.content,
            })
            .collect();

        let model = Provider::current().default_model();
        let speaker = db.preference(SPEAKER_NAME_KEY)?;
        chat::ask(
            &segments,
            &spans,
            &history,
            question,
            model,
            speaker.as_deref(),
        )
    })();

    match outcome {
        Ok(answer) => {
            if let Err(error) = db.add_chat_message(event_id, "assistant", &answer) {
                tracing::error!("could not store the answer for {event_id}: {error:#}");
            }
            let _ = app.emit(
                "chat",
                serde_json::json!({ "event_id": event_id, "stage": "answered" }),
            );
        }
        Err(error) => {
            tracing::error!("question about {event_id} failed: {error:#}");
            // Take the question back out. Left in place it would be replayed as history on
            // every later question, sending a message the model never answered.
            if let Err(problem) = db.delete_chat_message(question_id) {
                tracing::error!("could not withdraw the failed question: {problem:#}");
            }
            let _ = app.emit(
                "chat",
                serde_json::json!({
                    "event_id": event_id,
                    "stage": "failed",
                    // The chain, not just the outermost message.
                    "message": format!("{error:#}"),
                    // Handed back so the box can be refilled rather than retyped.
                    "question": question,
                }),
            );
        }
    }
}
