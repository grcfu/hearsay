//! Picking up recordings the app never got to finish.
//!
//! A recording is finished by `stop_recording`: it writes `ended_at`, stores the mute
//! spans, and starts transcription. Nothing does any of that if the process goes away
//! first — the machine slept, the app was force-quit, the power went. The audio is still
//! on disk, because the WAV header is rewritten as the recording runs (see
//! `hearsay_audio::wav`), but the event row is left mid-flight and no transcript is ever
//! made. Without this pass the user has a recording that looks broken and has to know
//! about the Re-transcribe button to rescue it.
//!
//! This runs **synchronously during setup**, before Tauri starts its event loop and so
//! before any command can run. That ordering is the whole reason it is safe to treat
//! every unfinished event as abandoned: no recording can be live yet, so there is nothing
//! here to mistake for one. Moving this onto a background thread would let it finalise a
//! session the user had just started.

use anyhow::Result;
use chrono::TimeDelta;
use hearsay_audio::{wav, Mode};
use hearsay_core::transcribe::SidecarPaths;
use hearsay_core::Database;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::AppHandle;

/// Transcription passes started per launch.
///
/// Each one runs `faster-whisper` over a whole recording, and they run one at a time, so
/// a backlog of thirty would have the machine busy for hours nobody asked for. Whatever
/// is left over is picked up on the next launch, since a finished pass is recorded.
const MAX_RESUMED_PER_LAUNCH: usize = 10;

/// Repairs interrupted recordings and starts any transcription that never ran.
///
/// Never fails the app: a recording that cannot be recovered is reported and skipped, and
/// is tried again next launch.
pub fn run(app: &AppHandle, db: &Arc<Database>) {
    if let Err(error) = finish_interrupted(db) {
        tracing::error!("could not finish interrupted recordings: {error:#}");
    }
    if let Err(error) = resume_transcription(app, db) {
        tracing::error!("could not resume transcription: {error:#}");
    }
}

/// Gives every abandoned recording a real end time, taken from the audio itself.
///
/// The clock cannot be used: the recording ended whenever the process died, which may
/// have been days ago. The file's own length is the only honest answer.
fn finish_interrupted(db: &Arc<Database>) -> Result<()> {
    for event in db.unfinished_events()? {
        let existing = event
            .audio_path
            .as_deref()
            .filter(|path| Path::new(path).is_file());

        let ended_at = match existing {
            Some(path) => match wav::repair(path) {
                Ok(repaired) => {
                    tracing::info!(
                        "recovered recording {}: {} ms of audio was on disk",
                        event.id,
                        repaired.duration_ms()
                    );
                    event.started_at
                        + TimeDelta::try_milliseconds(repaired.duration_ms() as i64)
                            .unwrap_or_default()
                }
                Err(error) => {
                    // Leave `ended_at` unset so the next launch tries again rather than
                    // freezing a wrong duration onto the recording.
                    tracing::error!("could not repair the audio for recording {}: {error:#}", event.id);
                    continue;
                }
            },
            None => {
                // The session died before a file existed, or the file was removed from
                // under us. Nothing to recover, but leaving it unfinished forever means
                // it is re-examined on every launch.
                tracing::warn!(
                    "recording {} has no audio file; marking it as having captured nothing",
                    event.id
                );
                event.started_at
            }
        };

        db.finish_event(event.id, ended_at)?;
    }
    Ok(())
}

/// Starts transcription for recordings that have audio and have never been transcribed.
///
/// Covers both the interrupted sessions just repaired above and the case where the app
/// was quit while a pass was still running.
fn resume_transcription(app: &AppHandle, db: &Arc<Database>) -> Result<()> {
    let pending: Vec<_> = db
        .untranscribed_events()?
        .into_iter()
        .filter(|event| {
            event
                .audio_path
                .as_deref()
                .is_some_and(|path| Path::new(path).is_file())
        })
        .collect();

    if pending.is_empty() {
        return Ok(());
    }

    // Without the sidecar every pass would fail immediately, and since a failed pass is
    // never marked done it would fail again on every launch from now on.
    if !SidecarPaths::is_available() {
        tracing::warn!(
            "{} recording(s) have never been transcribed, and the Python sidecar is not \
             installed. Run ./python/setup_venv.sh and they will be picked up next launch.",
            pending.len()
        );
        return Ok(());
    }

    let total = pending.len();
    let starting = total.min(MAX_RESUMED_PER_LAUNCH);
    tracing::info!("resuming transcription for {starting} recording(s), oldest first");
    if total > starting {
        // Say what was left behind. Silently starting a subset would read as "everything
        // is in hand" when it is not.
        tracing::warn!(
            "{} more recording(s) are still untranscribed and will be picked up on later \
             launches, or now with Re-transcribe",
            total - starting
        );
    }

    for event in pending.into_iter().take(starting) {
        let Some(path) = event.audio_path.as_deref().map(PathBuf::from) else {
            continue;
        };
        let mode = if event.mode == "conversation" {
            Mode::Conversation
        } else {
            Mode::ListenOnly
        };
        crate::commands::transcription::spawn_transcription(
            app.clone(),
            Arc::clone(db),
            event.id,
            path,
            mode,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use hearsay_audio::source::AudioFormat;
    use hearsay_audio::wav::WavWriter;

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("hearsay-recovery-{}-{name}.wav", std::process::id()));
        path
    }

    /// Half a second of stereo audio, left abandoned exactly as a killed process would:
    /// samples flushed, header never brought up to date.
    fn abandoned_recording(path: &Path, sample_rate: u32, seconds: u32) {
        let format = AudioFormat::new(sample_rate, 2);
        let mut writer = WavWriter::create(path, format).expect("writer is created");
        let frames = (sample_rate * seconds) as usize;
        writer
            .write_samples(&vec![0.25f32; frames * 2])
            .expect("samples write");
        std::mem::forget(writer);
    }

    #[test]
    fn an_interrupted_recording_is_finished_from_the_length_of_its_audio() {
        let path = temp_path("interrupted");
        abandoned_recording(&path, 48_000, 3);

        let db = Arc::new(hearsay_core::Database::open_in_memory().expect("database opens"));
        let started_at = Utc::now();
        let id = db
            .create_event(
                "Interrupted",
                "conversation",
                started_at,
                Some(&path.to_string_lossy()),
                None,
            )
            .expect("event is created");

        finish_interrupted(&db).expect("recovery runs");

        let event = db.event(id).expect("query works").expect("event exists");
        let duration = event.duration_ms().expect("the recording should now be finished");
        assert_eq!(
            duration, 3_000,
            "the end time must come from the audio, not from the clock"
        );
        assert!(
            db.unfinished_events().expect("query works").is_empty(),
            "the recovered recording should no longer look interrupted"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// A recovered recording still needs a transcript, so it must be left pending.
    #[test]
    fn a_recovered_recording_is_still_waiting_to_be_transcribed() {
        let path = temp_path("pending");
        abandoned_recording(&path, 48_000, 1);

        let db = Arc::new(hearsay_core::Database::open_in_memory().expect("database opens"));
        let id = db
            .create_event(
                "Interrupted",
                "listen_only",
                Utc::now(),
                Some(&path.to_string_lossy()),
                None,
            )
            .expect("event is created");

        finish_interrupted(&db).expect("recovery runs");

        let pending: Vec<i64> = db
            .untranscribed_events()
            .expect("query works")
            .into_iter()
            .map(|event| event.id)
            .collect();
        assert_eq!(pending, vec![id]);

        let _ = std::fs::remove_file(&path);
    }

    /// A session that died before writing anything must still stop being re-examined, or
    /// every launch from now on inspects a row that can never be recovered.
    #[test]
    fn an_event_whose_audio_never_existed_is_still_marked_finished() {
        let db = Arc::new(hearsay_core::Database::open_in_memory().expect("database opens"));
        let id = db
            .create_event("Never started", "listen_only", Utc::now(), None, None)
            .expect("event is created");

        finish_interrupted(&db).expect("recovery runs");

        let event = db.event(id).expect("query works").expect("event exists");
        assert_eq!(
            event.duration_ms(),
            Some(0),
            "a recording that captured nothing should read as zero length"
        );
        assert!(db.unfinished_events().expect("query works").is_empty());
    }

    /// Recovery must not touch a recording that ended cleanly.
    #[test]
    fn a_recording_that_ended_properly_is_left_alone() {
        let db = Arc::new(hearsay_core::Database::open_in_memory().expect("database opens"));
        let started_at = Utc::now();
        let id = db
            .create_event("Finished", "listen_only", started_at, None, None)
            .expect("event is created");
        let ended_at = started_at + TimeDelta::try_seconds(90).unwrap_or_default();
        db.finish_event(id, ended_at).expect("finish works");

        finish_interrupted(&db).expect("recovery runs");

        let event = db.event(id).expect("query works").expect("event exists");
        assert_eq!(event.duration_ms(), Some(90_000));
    }
}
