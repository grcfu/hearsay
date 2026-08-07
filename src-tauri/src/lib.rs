//! The Tauri shell.
//!
//! Commands and state only. Anything that could be tested without a window belongs in
//! `hearsay-audio` or `hearsay-core`.

mod commands;
mod permission;
mod shortcuts;
mod state;
mod tray;

use hearsay_core::Database;
use state::AppState;
use std::sync::Arc;
use tauri::Manager;

/// Starts the application.
///
/// A failure here — the database will not open, the data directory is unwritable — is
/// fatal and is reported before the window appears rather than as an empty screen.
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("HEARSAY_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let db = match open_database() {
        Ok(db) => Arc::new(db),
        Err(error) => {
            tracing::error!("could not open the database: {error:#}");
            eprintln!("Hearsay could not start: {error:#}");
            // Launched from Finder there is no terminal to print to, so without this the
            // app simply fails to open with no explanation. The most likely cause is an
            // older build meeting a database a newer one already migrated.
            show_startup_failure(&format!("{error:#}"));
            std::process::exit(1);
        }
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    shortcuts::handle(app, shortcut, event.state());
                })
                .build(),
        )
        .manage(AppState::new(db))
        .invoke_handler(tauri::generate_handler![
            commands::system::system_status,
            commands::system::request_audio_permission,
            commands::system::list_audible_apps,
            commands::recording::start_recording,
            commands::recording::stop_recording,
            commands::recording::recording_status,
            commands::transcription::retranscribe,
            commands::events::list_events,
            commands::events::event_detail,
            commands::events::rename_event,
            commands::events::search_transcripts,
            commands::events::delete_event,
            commands::settings::settings,
            commands::settings::save_api_key,
            commands::settings::clear_api_key,
            commands::summary::generate_summary,
            commands::mute::mute_state,
            commands::mute::set_mute,
            commands::mute::toggle_mute,
            commands::scrub::scrub_microphone,
            commands::calendar::calendar_status,
            commands::calendar::connect_calendar,
            commands::calendar::disconnect_calendar,
            commands::calendar::link_to_calendar,
        ])
        .setup(|app| {
            // Recording is driven from the window; without one there is nothing to drive
            // it, so this is a hard failure rather than a warning.
            if app.get_webview_window("main").is_none() {
                return Err("the main window is missing from tauri.conf.json".into());
            }

            // The menu bar item and the hotkeys are what make recording controllable
            // while the user is looking at their meeting app instead of at Hearsay.
            tray::build(app.handle())?;
            shortcuts::register(app.handle());
            commands::calendar::spawn_auto_arm(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("the Tauri runtime failed to start");
}

/// Reports a fatal startup problem in a dialog, since there may be no console.
fn show_startup_failure(detail: &str) {
    let message = format!(
        "Hearsay could not start.\n\n{detail}\n\nIf you recently pulled changes, \
         rebuild with ./install.sh — your recordings are untouched."
    );
    let script = format!(
        "display dialog {} with title \"Hearsay\" buttons {{\"OK\"}} with icon caution",
        serde_json::to_string(&message).unwrap_or_else(|_| "\"Hearsay could not start.\"".into())
    );
    let _ = std::process::Command::new("osascript")
        .args(["-e", &script])
        .status();
}

fn open_database() -> anyhow::Result<Database> {
    let path = hearsay_core::paths::db_path()?;
    tracing::info!("opening database at {}", path.display());
    Database::open(path)
}
