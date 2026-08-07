//! Every path Hearsay writes to at runtime.
//!
//! Everything lives under `~/Library/Application Support/hearsay/`. Nothing is
//! written anywhere else — no caches in `/tmp`, no dotfiles in `$HOME`.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// `~/Library/Application Support/hearsay/`, created if missing.
pub fn data_dir() -> Result<PathBuf> {
    let base = dirs::data_dir().context("could not resolve ~/Library/Application Support")?;
    let dir = base.join("hearsay");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create data directory {}", dir.display()))?;
    Ok(dir)
}

/// The SQLite database file.
pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("hearsay.sqlite"))
}

/// Directory holding recorded WAV files, created if missing.
pub fn recordings_dir() -> Result<PathBuf> {
    let dir = data_dir()?.join("recordings");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create recordings directory {}", dir.display()))?;
    Ok(dir)
}

/// Directory holding downloaded whisper model weights, created if missing.
pub fn models_dir() -> Result<PathBuf> {
    let dir = data_dir()?.join("models");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create models directory {}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_is_under_application_support() {
        let dir = data_dir().expect("data dir resolves");
        assert!(dir.ends_with("hearsay"), "got {}", dir.display());
        assert!(dir.to_string_lossy().contains("Application Support"));
    }
}
