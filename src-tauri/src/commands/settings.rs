//! Settings, which at this point means the API key and where things live on disk.

use crate::state::CommandResult;
use hearsay_core::secrets;
use serde::Serialize;

/// What settings looks like. Note what is absent: the key itself never crosses this
/// boundary, only whether one exists.
#[derive(Debug, Serialize)]
pub struct Settings {
    pub has_api_key: bool,
    pub data_dir: String,
    pub recordings_dir: String,
    pub models_dir: String,
    pub transcription_available: bool,
}

#[tauri::command]
pub fn settings() -> CommandResult<Settings> {
    Ok(Settings {
        has_api_key: secrets::has_api_key(),
        data_dir: hearsay_core::paths::data_dir()?.display().to_string(),
        recordings_dir: hearsay_core::paths::recordings_dir()?.display().to_string(),
        models_dir: hearsay_core::paths::models_dir()?.display().to_string(),
        transcription_available: hearsay_core::transcribe::SidecarPaths::is_available(),
    })
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

#[tauri::command]
pub fn clear_api_key() -> CommandResult<()> {
    secrets::clear_api_key()?;
    tracing::info!("the API key was removed from the Keychain");
    Ok(())
}
