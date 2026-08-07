//! The whole pipeline, end to end, on real audio.
//!
//! Speaks a sentence through the speakers, captures it with the process tap, stores it,
//! transcribes it, and searches for a word that was said. Everything between "a meeting
//! is happening" and "I can find what was said in it" is exercised here.
//!
//! ```sh
//! cargo test -p hearsay-core --test live_pipeline -- --ignored --nocapture
//! ```

use chrono::Utc;
use hearsay_audio::{Mode, Recording, TapTarget};
use hearsay_core::db::{Database, NewSegment};
use hearsay_core::transcribe::{transcribe_recording, DEFAULT_MODEL};
use std::process::Command;
use std::time::Duration;

#[test]
#[ignore = "needs the helper built, permission granted, speakers audible, and the sidecar installed"]
fn record_store_transcribe_and_search() {
    let db = Database::open_in_memory().expect("database opens");
    let started_at = Utc::now();
    let event_id = db
        .create_event("Pipeline test", "listen_only", started_at, None, None)
        .expect("event is created");

    let path = std::env::temp_dir().join(format!("hearsay-pipeline-{}.wav", std::process::id()));

    // Speak into the machine's own output so the tap has something real to capture.
    let mut speaking = Command::new("say")
        .arg("The quarterly migration deadline is the seventeenth of November.")
        .spawn()
        .expect("say should run on macOS");

    let recording =
        Recording::start(Mode::ListenOnly, TapTarget::SystemWide, &path).expect("recording starts");

    let _ = speaking.wait();
    std::thread::sleep(Duration::from_millis(400));
    let outcome = recording.stop().expect("recording stops cleanly");

    println!(
        "captured {} frames ({} ms)",
        outcome.frames, outcome.duration_ms
    );
    assert!(
        outcome.produced_audio,
        "the tap captured only silence; check system audio permission"
    );

    db.finish_event(event_id, Utc::now()).expect("event closes");
    db.set_audio_path(event_id, &path.to_string_lossy())
        .expect("audio path saves");

    let models_dir = hearsay_core::paths::models_dir().expect("models dir resolves");
    let segments = transcribe_recording(&path, Mode::ListenOnly, DEFAULT_MODEL, &models_dir, |_| {})
        .expect("transcription succeeds");

    for segment in &segments {
        println!("[{}] {}", segment.channel, segment.text);
    }
    assert!(!segments.is_empty(), "nothing was transcribed");

    let rows: Vec<NewSegment> = segments
        .into_iter()
        .map(|segment| NewSegment {
            channel: segment.channel,
            start_ms: segment.start_ms,
            end_ms: segment.end_ms,
            text: segment.text,
        })
        .collect();
    db.replace_segments(event_id, &rows).expect("segments save");

    // The payoff: a word that was spoken out loud is findable in the database.
    let hits = db.search("migration", 20).expect("search runs");
    println!("search found {} hit(s)", hits.len());
    assert!(
        !hits.is_empty(),
        "a word that was spoken aloud did not make it into search: {:?}",
        db.segments(event_id).map(|rows| rows
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>())
    );
    assert_eq!(hits[0].event_id, event_id);
    assert!(hits[0].start_ms >= 0);

    let _ = std::fs::remove_file(&path);
}
