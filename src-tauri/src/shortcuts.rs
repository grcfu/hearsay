//! Global hotkeys.
//!
//! These must work while Hearsay is in the background — that is the entire point. The
//! user is in a meeting looking at the meeting app, not at this one, and the moment they
//! need to mute is the moment they are least able to go hunting for a window.

use tauri::AppHandle;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

/// ⌘⇧M — mute or unmute the microphone.
///
/// Built on call rather than stored in a `const`: `Shortcut::new` is not a const fn.
pub fn mute() -> Shortcut {
    Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyM)
}

/// ⌘⇧X — erase microphone audio captured in the last minute but not yet written.
pub fn scrub() -> Shortcut {
    Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyX)
}

/// Registers every global shortcut.
///
/// A failure here is reported but not fatal: another app may already own the
/// combination, and a recorder that runs without its hotkeys is far better than one that
/// refuses to start.
pub fn register(app: &AppHandle) {
    let shortcuts = app.global_shortcut();

    for (shortcut, name) in [(mute(), "⌘⇧M (mute)"), (scrub(), "⌘⇧X (scrub)")] {
        match shortcuts.register(shortcut) {
            Ok(()) => tracing::info!("registered global shortcut {name}"),
            Err(error) => tracing::warn!(
                "could not register {name}: {error}. Another app may already use it; \
                 the in-window control still works."
            ),
        }
    }
}

/// Dispatches a shortcut press to the thing it controls.
///
/// Only fires on key-down: without this check every press fires twice, toggling mute
/// straight back off.
pub fn handle(app: &AppHandle, shortcut: &Shortcut, state: ShortcutState) {
    if state != ShortcutState::Pressed {
        return;
    }
    if shortcut == &mute() {
        crate::commands::mute::toggle_from_shortcut(app);
    } else if shortcut == &scrub() {
        crate::commands::scrub::scrub_from_shortcut(app);
    }
}
