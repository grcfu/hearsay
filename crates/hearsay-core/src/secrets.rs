//! The Anthropic API key.
//!
//! Stored in the macOS Keychain and nowhere else. Never in a `.env`, never in SQLite,
//! never in a log line, never in an error message, and never sent to the frontend — the
//! UI is only ever told whether a key exists, never what it is.
//!
//! Everything in the app works without a key. Recording, transcription, playback and
//! search never consult this module; only summary generation does.

use anyhow::{Context, Result};

/// Keychain service name. Shows in Keychain Access as the item's "where".
const SERVICE: &str = "com.hearsay.app";
/// Account name within the service.
const ACCOUNT: &str = "anthropic-api-key";

/// Which model provider generates summaries: "anthropic" or "gemini".
const PROVIDER: &str = "summary-provider";
/// Google Gemini API key.
const GEMINI_KEY: &str = "gemini-api-key";

/// Google OAuth client details, supplied by the user.
const CALENDAR_CLIENT: &str = "google-calendar-client";
/// Google OAuth tokens. Refresh tokens are long-lived credentials and belong in the
/// Keychain for exactly the same reasons the API key does.
const CALENDAR_TOKENS: &str = "google-calendar-tokens";

fn entry() -> Result<keyring::Entry> {
    named_entry(ACCOUNT)
}

fn named_entry(account: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, account).context("could not reach the macOS Keychain")
}

fn read(account: &str) -> Result<Option<String>> {
    match named_entry(account)?.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(anyhow::anyhow!(
            "could not read {account} from the Keychain: {error}"
        )),
    }
}

fn write(account: &str, value: &str) -> Result<()> {
    named_entry(account)?
        .set_password(value)
        .with_context(|| format!("could not save {account} to the Keychain"))
}

fn remove(account: &str) -> Result<()> {
    match named_entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(anyhow::anyhow!(
            "could not remove {account} from the Keychain: {error}"
        )),
    }
}

/// The chosen summary provider, defaulting to Anthropic.
pub fn summary_provider() -> String {
    read(PROVIDER)
        .ok()
        .flatten()
        .unwrap_or_else(|| "anthropic".to_string())
}

pub fn set_summary_provider(value: &str) -> Result<()> {
    write(PROVIDER, value)
}

pub fn gemini_key() -> Result<Option<String>> {
    read(GEMINI_KEY)
}

pub fn set_gemini_key(key: &str) -> Result<()> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        anyhow::bail!("an empty key cannot be saved");
    }
    write(GEMINI_KEY, trimmed)
}

pub fn clear_gemini_key() -> Result<()> {
    remove(GEMINI_KEY)
}

/// The user's Google OAuth client details, as stored JSON.
pub fn calendar_client() -> Result<Option<String>> {
    read(CALENDAR_CLIENT)
}

pub fn set_calendar_client(value: &str) -> Result<()> {
    write(CALENDAR_CLIENT, value)
}

pub fn clear_calendar_client() -> Result<()> {
    remove(CALENDAR_CLIENT)
}

/// Google OAuth tokens, as stored JSON.
pub fn calendar_tokens() -> Result<Option<String>> {
    read(CALENDAR_TOKENS)
}

pub fn set_calendar_tokens(value: &str) -> Result<()> {
    write(CALENDAR_TOKENS, value)
}

pub fn clear_calendar_tokens() -> Result<()> {
    remove(CALENDAR_TOKENS)
}

/// Stores the key, replacing any existing one.
///
/// The key is trimmed, because pasted keys routinely carry a trailing newline and an
/// invisible character would otherwise become a baffling authentication failure.
pub fn set_api_key(key: &str) -> Result<()> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        anyhow::bail!("an empty key cannot be saved");
    }
    entry()?
        .set_password(trimmed)
        // Deliberately vague: an error string must never be able to carry the key.
        .context("could not save the key to the Keychain")?;
    Ok(())
}

/// Reads the key. `None` means no key is stored, which is a normal state, not an error.
pub fn api_key() -> Result<Option<String>> {
    match entry()?.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(anyhow::anyhow!(
            "could not read the key from the Keychain: {}",
            // `error` cannot contain the secret; keyring reports the lookup, not the value.
            error
        )),
    }
}

/// Whether an Anthropic key is stored. This is all the frontend is ever told.
pub fn has_api_key() -> bool {
    matches!(api_key(), Ok(Some(_)))
}

pub fn has_gemini_key() -> bool {
    matches!(gemini_key(), Ok(Some(_)))
}

/// Whether summaries can run at all — whichever provider is selected has a key.
pub fn has_summary_key() -> bool {
    match summary_provider().as_str() {
        "gemini" => has_gemini_key(),
        _ => has_api_key(),
    }
}

/// Removes the key. Removing a key that is not there succeeds.
pub fn clear_api_key() -> Result<()> {
    match entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(anyhow::anyhow!(
            "could not remove the key from the Keychain: {error}"
        )),
    }
}

/// A safe fingerprint of a key, for confirming *which* key is stored without revealing
/// it. Shows only the prefix and the last four characters, the way a bank shows a card.
pub fn key_hint(key: &str) -> String {
    let trimmed = key.trim();
    let characters: Vec<char> = trimmed.chars().collect();
    if characters.len() <= 12 {
        return "•".repeat(characters.len().max(4));
    }
    let tail: String = characters[characters.len() - 4..].iter().collect();
    format!("sk-ant-…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hint_never_reveals_the_middle_of_a_key() {
        let key = "sk-ant-api03-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let hint = key_hint(key);
        assert!(hint.ends_with("6789"), "got {hint}");
        assert!(
            !hint.contains("ABCDEFGH"),
            "the hint leaked part of the key: {hint}"
        );
    }

    #[test]
    fn a_short_value_is_fully_masked() {
        assert_eq!(key_hint("abcd"), "••••");
        assert!(!key_hint("shortkey").contains("short"));
    }

    #[test]
    fn an_empty_key_is_refused() {
        assert!(set_api_key("   ").is_err());
    }
}
