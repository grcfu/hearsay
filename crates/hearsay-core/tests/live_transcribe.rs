//! Transcription tests that run the real sidecar against real audio.
//!
//! `#[ignore]`d because they need `./python/setup_venv.sh` to have run and, the first
//! time, a model download. Run them by hand when touching the transcription path:
//!
//! ```sh
//! cargo test -p hearsay-core --test live_transcribe -- --ignored --nocapture
//! ```

use hearsay_audio::Mode;
use hearsay_core::transcribe::{transcribe_recording, SidecarPaths, DEFAULT_MODEL};
use std::path::PathBuf;

/// Builds a two-channel WAV with different speech on each side, using macOS `say` so the
/// test carries no audio fixtures of its own.
fn stereo_fixture() -> PathBuf {
    use std::process::Command;

    let dir = std::env::temp_dir().join(format!("hearsay-transcribe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir should be creatable");

    let left = dir.join("left.wav");
    let right = dir.join("right.wav");
    let stereo = dir.join("stereo.wav");

    for (path, text) in [
        (&left, "Yes I agree, let me take that action item."),
        (&right, "We need someone to own the migration timeline."),
    ] {
        let status = Command::new("say")
            .args(["-o"])
            .arg(path)
            .args(["--data-format=LEI16@16000", text])
            .status()
            .expect("say should run on macOS");
        assert!(status.success(), "say failed for {}", path.display());
    }

    let paths = SidecarPaths::discover().expect("sidecar should be installed");
    let script = format!(
        r#"
import numpy as np, soundfile as sf
left, _ = sf.read({left:?}, dtype="float32")
right, _ = sf.read({right:?}, dtype="float32")
n = max(len(left), len(right))
l = np.zeros(n, dtype="float32"); l[: len(left)] = left
r = np.zeros(n, dtype="float32"); r[: len(right)] = right
sf.write({stereo:?}, np.stack([l, r], axis=1), 16000)
"#,
        left = left.to_string_lossy(),
        right = right.to_string_lossy(),
        stereo = stereo.to_string_lossy(),
    );
    let status = Command::new(&paths.python)
        .args(["-c", &script])
        .status()
        .expect("python should run");
    assert!(status.success(), "could not build the stereo fixture");

    stereo
}

#[test]
#[ignore = "needs ./python/setup_venv.sh and a downloaded model"]
fn each_channel_is_transcribed_and_attributed_separately() {
    let audio = stereo_fixture();
    let models_dir = hearsay_core::paths::models_dir().expect("models dir resolves");

    let segments = transcribe_recording(
        &audio,
        Mode::Conversation,
        DEFAULT_MODEL,
        &models_dir,
        |event| println!("{event:?}"),
    )
    .expect("transcription should succeed");

    for segment in &segments {
        println!("[{}] {}..{} {}", segment.channel, segment.start_ms, segment.end_ms, segment.text);
    }

    let mic: String = segments
        .iter()
        .filter(|s| s.channel == "mic")
        .map(|s| s.text.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    let system: String = segments
        .iter()
        .filter(|s| s.channel == "system")
        .map(|s| s.text.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");

    assert!(!mic.is_empty(), "the mic channel produced no text");
    assert!(!system.is_empty(), "the system channel produced no text");

    // Each channel must contain its own words and not the other's — that is the whole
    // speaker-attribution guarantee.
    assert!(
        mic.contains("action item"),
        "mic channel lost its own speech: {mic:?}"
    );
    assert!(
        system.contains("migration"),
        "system channel lost its own speech: {system:?}"
    );
    assert!(
        !mic.contains("migration"),
        "the other party's speech leaked into the mic channel: {mic:?}"
    );
    assert!(
        !system.contains("action item"),
        "the user's speech leaked into the system channel: {system:?}"
    );

    // The merged timeline must be in order, not two concatenated monologues.
    let mut previous = i64::MIN;
    for segment in &segments {
        assert!(
            segment.start_ms >= previous,
            "segments are out of order at {}",
            segment.start_ms
        );
        previous = segment.start_ms;
    }
}

#[test]
#[ignore = "needs ./python/setup_venv.sh and a downloaded model"]
fn a_listen_only_recording_is_all_system_audio() {
    let audio = stereo_fixture();
    let models_dir = hearsay_core::paths::models_dir().expect("models dir resolves");

    let segments = transcribe_recording(
        &audio,
        Mode::ListenOnly,
        DEFAULT_MODEL,
        &models_dir,
        |_| {},
    )
    .expect("transcription should succeed");

    assert!(!segments.is_empty(), "expected some transcript");
    assert!(
        segments.iter().all(|s| s.channel == "system"),
        "listen_only produced a segment attributed to the microphone"
    );
}
