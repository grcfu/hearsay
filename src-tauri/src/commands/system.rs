//! Telling the user what does and does not work right now.
//!
//! Every prerequisite in this app can be missing independently — the helper may not be
//! built, macOS may not have granted permission, the Python sidecar may not be installed
//! — and each failure has a different fix. Reporting them separately is what lets the UI
//! say which one to do something about, instead of "something went wrong".

use crate::state::CommandResult;
use hearsay_audio::helper;
use hearsay_audio::process::{audible_apps, AudibleApp};
use hearsay_core::transcribe::SidecarPaths;
use serde::Serialize;

/// What the app can and cannot do at this moment.
#[derive(Debug, Serialize)]
pub struct SystemStatus {
    /// The Swift audio helper was found. Without it there is no system audio at all.
    pub helper_available: bool,
    pub helper_path: Option<String>,
    /// macOS permits capturing system audio. Without this a recording would be silent.
    pub audio_permission: bool,
    /// The Python sidecar is installed. Without it recordings still happen; they just
    /// stay untranscribed until it is set up.
    pub transcription_available: bool,
    /// Any problem that stopped one of the checks from running.
    pub problem: Option<String>,
}

#[tauri::command]
pub fn system_status() -> SystemStatus {
    let (helper_available, helper_path, mut problem) = match helper::helper_path() {
        Ok(path) => (true, Some(path.display().to_string()), None),
        Err(error) => (false, None, Some(error.to_string())),
    };

    let audio_permission = if helper_available {
        match helper::permission_granted() {
            Ok(granted) => granted,
            Err(error) => {
                problem.get_or_insert(error.to_string());
                false
            }
        }
    } else {
        false
    };

    SystemStatus {
        helper_available,
        helper_path,
        audio_permission,
        transcription_available: SidecarPaths::is_available(),
        problem,
    }
}

/// Asks macOS to show the system-audio permission prompt.
///
/// macOS only shows it once per app. If the user has already answered, this returns the
/// stored answer and the UI has to send them to System Settings instead.
#[tauri::command]
pub fn request_audio_permission() -> CommandResult<bool> {
    Ok(helper::request_permission()?)
}

/// Apps currently making sound, which is what the user picks from before recording.
///
/// Grouped per app rather than per process: Chrome, Slack and Zoom all play through
/// helper processes, and offering those individually would be asking the user to know
/// something they have no reason to know.
#[tauri::command]
pub fn list_audible_apps() -> CommandResult<Vec<AudibleApp>> {
    let processes = helper::list_processes()?;
    Ok(audible_apps(&processes))
}
