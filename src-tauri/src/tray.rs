//! The menu bar item.
//!
//! Its job is to answer two questions without the user switching to the app: is it
//! recording, and is the microphone muted. Both hotkeys work while Hearsay is in the
//! background, so the menu bar is the only feedback the user gets for them.

use crate::state::AppState;
use std::sync::{Mutex, OnceLock};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Manager, Wry};

const TRAY_ID: &str = "hearsay";

/// Handles to the menu items whose text changes.
///
/// `TrayIcon` exposes no getter for its menu, so the items are kept here rather than
/// rebuilt on every refresh — rebuilding the menu closes it out from under a user who
/// has it open.
struct Items {
    status: MenuItem<Wry>,
    mute: MenuItem<Wry>,
}

static ITEMS: OnceLock<Mutex<Items>> = OnceLock::new();

/// Builds the menu bar item. Called once at startup.
pub fn build(app: &AppHandle) -> tauri::Result<TrayIcon> {
    let status = MenuItem::with_id(app, "status", "Not recording", false, None::<&str>)?;
    let mute = MenuItem::with_id(app, "mute", "Mute microphone", false, Some("Cmd+Shift+M"))?;
    let show = MenuItem::with_id(app, "show", "Open Hearsay", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Hearsay", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &status,
            &PredefinedMenuItem::separator(app)?,
            &mute,
            &PredefinedMenuItem::separator(app)?,
            &show,
            &quit,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .tooltip("Hearsay — not recording")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "mute" => crate::commands::mute::toggle_from_shortcut(app),
            "show" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "quit" => {
                // Stop any running recording first, so the WAV is finalised rather than
                // left for `repair` to rescue.
                if let Some(state) = app.try_state::<AppState>() {
                    if let Ok(mut active) = state.lock_recording() {
                        if let Some(session) = active.take() {
                            let _ = session.recording.stop();
                        }
                    }
                }
                app.exit(0);
            }
            _ => {}
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone()).icon_as_template(true);
    }

    let tray = builder.build(app)?;
    let _ = ITEMS.set(Mutex::new(Items {
        status: status.clone(),
        mute: mute.clone(),
    }));
    Ok(tray)
}

/// Brings the menu bar item in line with the current recording state.
///
/// Called after anything that changes it: starting, stopping, muting, scrubbing. Cheap
/// enough to call unconditionally.
pub fn refresh(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let Ok(active) = state.lock_recording() else {
        return;
    };

    let (status_text, tooltip, mute_enabled, mute_label) = match active.as_ref() {
        Some(session) => {
            let conversation = session.recording.mode().opens_microphone();
            let muted = session.recording.is_muted();
            let status = if !conversation {
                "Recording — listen only".to_string()
            } else if muted {
                "Recording — microphone muted".to_string()
            } else {
                "Recording — microphone live".to_string()
            };
            let label = if muted { "Unmute microphone" } else { "Mute microphone" };
            (status.clone(), format!("Hearsay — {status}"), conversation, label)
        }
        None => (
            "Not recording".to_string(),
            "Hearsay — not recording".to_string(),
            false,
            "Mute microphone",
        ),
    };
    let recording = active.is_some();
    let conversation = mute_enabled;
    drop(active);

    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let _ = tray.set_tooltip(Some(&tooltip));

    // A short title beside the icon: the menu bar's whole job here is to be readable
    // without clicking. Only shown while recording, so an idle menu bar stays quiet.
    let _ = tray.set_title(if recording { Some("●") } else { None });

    if let Some(items) = ITEMS.get().and_then(|items| items.lock().ok()) {
        let _ = items.status.set_text(&status_text);
        let _ = items.mute.set_text(mute_label);
        let _ = items.mute.set_enabled(conversation);
    }
}
