//! Choosing what to record.
//!
//! The Swift helper matches bundle identifiers exactly and holds no policy. The policy
//! lives here: Chrome plays its audio from `com.google.Chrome.helper`, and Slack, Teams
//! and Zoom do the same thing with their own helper processes. Asking to record "Google
//! Chrome" has to mean "Chrome and the helpers it speaks through", so this module
//! resolves a user-facing choice into the concrete PIDs the helper is handed.

use serde::{Deserialize, Serialize};

/// One process as Core Audio sees it, as reported by `hearsay-audio-helper --list`.
///
/// Object IDs are not stable across launches. Everything downstream resolves by PID,
/// freshly, at the moment recording starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioProcess {
    pub object_id: u32,
    pub pid: i32,
    #[serde(default)]
    pub bundle_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    pub is_running_output: bool,
    #[serde(default)]
    pub is_running_input: bool,
    /// A normal windowed app rather than a daemon or agent.
    #[serde(default)]
    pub is_application: bool,
}

impl AudioProcess {
    /// What to show a human. Falls back through name, bundle id, then bare PID.
    pub fn display_name(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }
        if let Some(bundle) = &self.bundle_id {
            return bundle.clone();
        }
        format!("pid {}", self.pid)
    }

    /// The identifier the UI groups by, so Chrome and its helpers appear as one app.
    pub fn app_key(&self) -> String {
        match &self.bundle_id {
            Some(bundle) => root_bundle_id(bundle).to_string(),
            None => format!("pid:{}", self.pid),
        }
    }
}

/// Strips the helper suffix from a bundle identifier.
///
/// `com.google.Chrome.helper` and `com.electron.slack.helper.renderer` both belong to
/// their parent app. Only known helper markers are stripped — `com.apple.Music` must not
/// be shortened to `com.apple`.
pub fn root_bundle_id(bundle_id: &str) -> &str {
    const HELPER_MARKERS: [&str; 3] = [".helper", ".Helper", ".framework"];

    for marker in HELPER_MARKERS {
        if let Some(index) = bundle_id.find(marker) {
            return &bundle_id[..index];
        }
    }
    bundle_id
}

/// Every process that belongs to the app identified by `bundle_id`.
///
/// Matches the app itself and any helper process underneath it. Case-insensitive,
/// because bundle identifiers are compared case-insensitively by macOS.
pub fn processes_for_app<'a>(
    processes: &'a [AudioProcess],
    bundle_id: &str,
) -> Vec<&'a AudioProcess> {
    let wanted = root_bundle_id(bundle_id).to_lowercase();
    processes
        .iter()
        .filter(|process| match &process.bundle_id {
            Some(candidate) => root_bundle_id(candidate).to_lowercase() == wanted,
            None => false,
        })
        .collect()
}

/// Apps the user can choose to record, one entry per app rather than one per helper
/// process.
///
/// Every real application is listed, not just the ones currently making noise. You pick
/// what to record *before* the meeting starts, and at that moment the meeting app is
/// silent — filtering to "currently playing" left the picker empty exactly when it was
/// needed. `is_playing` is reported so the UI can mark the ones already making sound.
///
/// Daemons and background agents are excluded: `cloudpaird` is not something anyone
/// means to record.
pub fn audible_apps(processes: &[AudioProcess]) -> Vec<AudibleApp> {
    let mut apps: Vec<AudibleApp> = Vec::new();

    for process in processes.iter().filter(|p| p.is_application || p.is_running_output) {
        let key = process.app_key();
        match apps.iter_mut().find(|app| app.key == key) {
            Some(app) => {
                app.pids.push(process.pid);
                app.is_playing |= process.is_running_output;
                // A helper process usually has no useful name; prefer any real one.
                if app.name.is_empty() {
                    app.name = process.display_name();
                }
            }
            None => apps.push(AudibleApp {
                key,
                name: process.display_name(),
                bundle_id: process.bundle_id.clone(),
                pids: vec![process.pid],
                is_playing: process.is_running_output,
            }),
        }
    }

    // Apps already making sound first — usually the one you want — then alphabetical.
    apps.sort_by(|a, b| {
        b.is_playing
            .cmp(&a.is_playing)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    apps
}

/// An app the user can choose to record, with every PID that makes sound on its behalf.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudibleApp {
    pub key: String,
    pub name: String,
    pub bundle_id: Option<String>,
    pub pids: Vec<i32>,
    /// Making sound right now.
    pub is_playing: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: i32, bundle: Option<&str>, name: Option<&str>, output: bool) -> AudioProcess {
        AudioProcess {
            object_id: pid as u32,
            pid,
            bundle_id: bundle.map(String::from),
            name: name.map(String::from),
            is_running_output: output,
            is_running_input: false,
            is_application: true,
        }
    }

    #[test]
    fn helper_bundles_resolve_to_their_parent_app() {
        assert_eq!(root_bundle_id("com.google.Chrome.helper"), "com.google.Chrome");
        assert_eq!(root_bundle_id("com.google.Chrome"), "com.google.Chrome");
        assert_eq!(
            root_bundle_id("com.electron.slack.helper.renderer"),
            "com.electron.slack"
        );
    }

    #[test]
    fn a_normal_bundle_id_is_never_truncated() {
        assert_eq!(root_bundle_id("com.apple.Music"), "com.apple.Music");
        assert_eq!(root_bundle_id("us.zoom.xos"), "us.zoom.xos");
    }

    #[test]
    fn selecting_chrome_captures_its_helper_processes() {
        let processes = vec![
            process(100, Some("com.google.Chrome"), Some("Google Chrome"), false),
            process(101, Some("com.google.Chrome.helper"), Some("helper"), true),
            process(102, Some("com.apple.Music"), Some("Music"), true),
        ];

        let matched = processes_for_app(&processes, "com.google.Chrome");
        let pids: Vec<i32> = matched.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![100, 101]);
    }

    #[test]
    fn music_playing_alongside_is_a_separate_app() {
        let processes = vec![
            process(101, Some("com.google.Chrome.helper"), Some("helper"), true),
            process(102, Some("com.apple.Music"), Some("Music"), true),
        ];

        let apps = audible_apps(&processes);
        assert_eq!(apps.len(), 2);
        let chrome = apps
            .iter()
            .find(|a| a.key == "com.google.Chrome")
            .expect("chrome is listed");
        assert_eq!(chrome.pids, vec![101]);
    }

    /// You choose what to record before the meeting starts, when the app is still
    /// silent. Offering only what is already playing left the picker empty.
    #[test]
    fn a_silent_app_is_still_offered_so_it_can_be_chosen_in_advance() {
        let processes = vec![process(100, Some("com.apple.Safari"), Some("Safari"), false)];
        let apps = audible_apps(&processes);
        assert_eq!(apps.len(), 1);
        assert!(!apps[0].is_playing);
    }

    #[test]
    fn apps_already_making_sound_are_listed_first() {
        let mut quiet = process(100, Some("com.apple.Safari"), Some("Safari"), false);
        quiet.is_application = true;
        let mut loud = process(101, Some("com.zoom.xos"), Some("Zoom"), true);
        loud.is_application = true;

        let apps = audible_apps(&[quiet, loud]);
        assert_eq!(apps[0].name, "Zoom", "the app making sound should be first");
        assert!(apps[0].is_playing);
    }

    /// Daemons register with Core Audio too, and nobody means to record `cloudpaird`.
    #[test]
    fn background_daemons_are_not_offered() {
        let mut daemon = process(50, Some("com.apple.cloudpaird"), Some("cloudpaird"), false);
        daemon.is_application = false;
        assert!(audible_apps(&[daemon]).is_empty());
    }
}
