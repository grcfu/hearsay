//! Calendar commands and the auto-arm prompt.

use crate::state::{AppState, CommandError, CommandResult};
use chrono::{Duration as ChronoDuration, Utc};
use hearsay_core::calendar::{self, CalendarEvent, ClientCredentials};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

/// How far ahead to offer to start recording.
const ARM_WINDOW_MINUTES: i64 = 3;
/// How often to look. Calendars change slowly; a minute is ample and costs nothing.
const POLL_SECONDS: u64 = 60;

#[derive(Debug, Serialize)]
pub struct CalendarStatus {
    pub connected: bool,
    /// The meeting starting now or shortly, if any.
    pub next: Option<CalendarEvent>,
}

#[tauri::command]
pub fn calendar_status() -> CommandResult<CalendarStatus> {
    let connected = calendar::is_connected();
    let next = if connected { upcoming().ok().flatten() } else { None };
    Ok(CalendarStatus { connected, next })
}

/// Signs in to Google. Blocks until the browser flow finishes or times out.
#[tauri::command]
pub fn connect_calendar(
    app: AppHandle,
    client_id: String,
    client_secret: String,
) -> CommandResult<()> {
    let credentials = ClientCredentials {
        client_id: client_id.trim().to_string(),
        client_secret: client_secret.trim().to_string(),
    };
    if credentials.client_id.is_empty() || credentials.client_secret.is_empty() {
        return Err(CommandError {
            message: "both the client ID and client secret are needed".to_string(),
        });
    }

    calendar::connect(&credentials, |url| {
        if let Err(error) = tauri_plugin_opener::open_url(url, None::<&str>) {
            tracing::error!("could not open the browser for sign-in: {error}");
        }
    })?;
    let _ = app.emit("calendar", serde_json::json!({ "connected": true }));
    Ok(())
}

#[tauri::command]
pub fn disconnect_calendar(app: AppHandle) -> CommandResult<()> {
    calendar::disconnect()?;
    let _ = app.emit("calendar", serde_json::json!({ "connected": false }));
    Ok(())
}

/// Links a finished recording to the meeting it overlaps, and adopts its title.
///
/// The user's own title always wins if they have set one — a calendar title is a better
/// default than a timestamp, never better than a decision.
#[tauri::command]
pub fn link_to_calendar(state: State<'_, AppState>, event_id: i64) -> CommandResult<Option<String>> {
    let event = state.db.event(event_id)?.ok_or_else(|| CommandError {
        message: format!("no recording with id {event_id}"),
    })?;
    let ended_at = event.ended_at.unwrap_or_else(Utc::now);

    let events = calendar::events_between(
        event.started_at - ChronoDuration::hours(2),
        ended_at + ChronoDuration::hours(2),
    )?;

    match calendar::match_recording(&events, event.started_at, ended_at) {
        Some(matched) => {
            state.db.link_calendar_event(event_id, &matched.id)?;
            if event.title.starts_with("Recording, ") {
                state.db.rename_event(event_id, &matched.title)?;
            }
            Ok(Some(matched.title.clone()))
        }
        None => Ok(None),
    }
}

fn upcoming() -> anyhow::Result<Option<CalendarEvent>> {
    let now = Utc::now();
    let events = calendar::events_between(now - ChronoDuration::minutes(30), now + ChronoDuration::hours(2))?;
    Ok(calendar::next_starting(&events, now, ChronoDuration::minutes(ARM_WINDOW_MINUTES)).cloned())
}

/// Watches the calendar and offers to start recording when a meeting begins.
///
/// It only ever *offers*. Nothing starts recording on its own — a recorder that arms
/// itself is one the user cannot trust to be off.
pub fn spawn_auto_arm(app: AppHandle) {
    std::thread::Builder::new()
        .name("hearsay-calendar".into())
        .spawn(move || {
            let mut offered: Option<String> = None;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(POLL_SECONDS));

                if !calendar::is_connected() {
                    offered = None;
                    continue;
                }
                // Don't interrupt a session in progress with an offer to start one.
                let busy = app
                    .try_state::<AppState>()
                    .map(|state| state.is_recording())
                    .unwrap_or(false);
                if busy {
                    continue;
                }

                match upcoming() {
                    Ok(Some(event)) => {
                        // One offer per meeting; repeating it every minute would be
                        // nagging, and the user has already said no once.
                        if offered.as_deref() != Some(event.id.as_str()) {
                            offered = Some(event.id.clone());
                            tracing::info!("offering to record \"{}\"", event.title);
                            let _ = app.emit("calendar-arm", &event);
                        }
                    }
                    Ok(None) => offered = None,
                    Err(error) => tracing::warn!("calendar check failed: {error:#}"),
                }
            }
        })
        .map(|_| ())
        .unwrap_or_else(|error| tracing::error!("could not start the calendar watcher: {error}"));
}
