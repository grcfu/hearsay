//! Settings, which at this point means the API key and where things live on disk.

use crate::commands::summary::SPEAKER_NAME_KEY;
use crate::state::{AppState, CommandResult};
use hearsay_core::secrets;
use serde::Serialize;
use tauri::State;

/// What settings looks like. Note what is absent: the key itself never crosses this
/// boundary, only whether one exists.
#[derive(Debug, Serialize)]
pub struct Settings {
    pub has_api_key: bool,
    pub has_gemini_key: bool,
    /// "anthropic" or "gemini".
    pub provider: String,
    /// What summaries call the person recording. Empty means the default, "You".
    pub speaker_name: String,
    pub data_dir: String,
    pub recordings_dir: String,
    pub models_dir: String,
    pub transcription_available: bool,
}

#[tauri::command]
pub fn settings(state: State<'_, AppState>) -> CommandResult<Settings> {
    Ok(Settings {
        speaker_name: state.db.preference(SPEAKER_NAME_KEY)?.unwrap_or_default(),
        has_api_key: secrets::has_api_key(),
        has_gemini_key: secrets::has_gemini_key(),
        provider: secrets::summary_provider(),
        data_dir: hearsay_core::paths::data_dir()?.display().to_string(),
        recordings_dir: hearsay_core::paths::recordings_dir()?.display().to_string(),
        models_dir: hearsay_core::paths::models_dir()?.display().to_string(),
        transcription_available: hearsay_core::transcribe::SidecarPaths::is_available(),
    })
}

/// Sets the name summaries should use for the person recording.
///
/// Stored in the database rather than the Keychain: it is a preference, not a secret,
/// and it is written into every prompt.
#[tauri::command]
pub fn set_speaker_name(state: State<'_, AppState>, name: String) -> CommandResult<()> {
    state.db.set_preference(SPEAKER_NAME_KEY, &name)?;
    Ok(())
}

/// Saves the API key to the macOS Keychain.
///
/// Returns nothing on purpose: there is no round trip that could echo the key back into
/// the webview, into a devtools log, or into a crash report.
#[tauri::command]
pub fn save_api_key(key: String) -> CommandResult<()> {
    secrets::set_api_key(&key)?;
    tracing::info!("an API key was saved to the Keychain");
    Ok(())
}

/// Saves a Gemini key, and switches the provider to it.
///
/// Switching on save rather than making it a separate step: someone adding a Gemini key
/// wants Gemini, and leaving them with a key that does nothing until they find a second
/// control would be a trap.
#[tauri::command]
pub fn save_gemini_key(key: String) -> CommandResult<()> {
    secrets::set_gemini_key(&key)?;
    secrets::set_summary_provider("gemini")?;
    tracing::info!("a Gemini key was saved; summaries will use Gemini");
    Ok(())
}

#[tauri::command]
pub fn clear_gemini_key() -> CommandResult<()> {
    secrets::clear_gemini_key()?;
    secrets::set_summary_provider("anthropic")?;
    Ok(())
}

/// Chooses which service writes summaries.
#[tauri::command]
pub fn set_summary_provider(provider: String) -> CommandResult<()> {
    match provider.as_str() {
        "anthropic" | "gemini" => {
            secrets::set_summary_provider(&provider)?;
            Ok(())
        }
        other => Err(crate::state::CommandError {
            message: format!("unknown provider {other:?}"),
        }),
    }
}

#[tauri::command]
pub fn clear_api_key() -> CommandResult<()> {
    secrets::clear_api_key()?;
    tracing::info!("the API key was removed from the Keychain");
    Ok(())
}
