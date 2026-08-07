//! Tests that need real audio playing on a real machine.
//!
//! These are `#[ignore]`d because they depend on the state of the world: the helper must
//! be built, macOS must have granted system-audio permission, and something must
//! actually be making sound. Run them by hand when touching the capture path:
//!
//! ```sh
//! ./helper/build.sh
//! # start playing something audible, then:
//! cargo test -p hearsay-audio --test live_capture -- --ignored --nocapture
//! ```

use hearsay_audio::helper::{list_processes, permission_granted};
use hearsay_audio::process::audible_apps;
use hearsay_audio::{AudioSource, Chunk, HelperSource, TapTarget};
use std::time::{Duration, Instant};

/// Reads for `seconds`, returning the frames seen. Uses the timeout variant so an output
/// device that goes quiet ends the test promptly instead of blocking on `recv`.
fn drain(source: &mut dyn AudioSource, seconds: u64) -> u64 {
    let format = source.format();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut frames = 0u64;

    while Instant::now() < deadline {
        match source.next_chunk_timeout(Duration::from_millis(250)) {
            Chunk::Samples(samples) => frames += format.frames(samples.len()) as u64,
            Chunk::Idle => continue,
            Chunk::Finished => break,
        }
    }
    frames
}

#[test]
#[ignore = "needs the helper built, permission granted, and audio playing"]
fn captures_real_audio_from_the_system_tap() {
    assert!(
        permission_granted().expect("could not ask the helper about permission"),
        "system audio permission is not granted; nothing below can pass"
    );

    let mut source = HelperSource::start(TapTarget::SystemWide).expect("helper should start");

    let format = source.format();
    assert!(
        format.sample_rate >= 8_000,
        "implausible sample rate: {format:?}"
    );
    assert!(format.channels >= 1, "implausible channel count: {format:?}");
    println!("format: {format:?}");

    let frames = drain(&mut source, 5);
    let nonzero = source.nonzero_samples();
    source.stop().expect("helper should stop cleanly");

    println!(
        "captured {frames} frames ({} ms), {nonzero} non-zero samples",
        format.duration_ms(frames)
    );

    assert!(
        frames > 0,
        "the tap delivered no frames at all — is anything playing?"
    );
    assert!(
        nonzero > 0,
        "every captured sample was zero — the tap ran but recorded silence"
    );
}

#[test]
#[ignore = "needs the helper built and at least one app playing audio"]
fn captures_only_the_process_it_was_pointed_at() {
    let processes = list_processes().expect("the helper should list processes");
    let apps = audible_apps(&processes);
    assert!(
        !apps.is_empty(),
        "nothing is playing audio; start something and re-run"
    );

    let app = &apps[0];
    println!("recording {} (pids {:?})", app.name, app.pids);

    let mut source = HelperSource::start(TapTarget::Processes(app.pids.clone()))
        .expect("a process-scoped tap should start");

    let frames = drain(&mut source, 5);
    let nonzero = source.nonzero_samples();
    source.stop().expect("helper should stop cleanly");

    println!("captured {frames} frames, {nonzero} non-zero samples");
    assert!(
        nonzero > 0,
        "process-scoped tap on {} produced only silence",
        app.name
    );
}

/// The failure this whole project is built to avoid: a tap pointed at something that is
/// not making sound must not look like a successful recording.
#[test]
#[ignore = "needs the helper built"]
fn a_process_making_no_sound_yields_no_audio() {
    let processes = list_processes().expect("the helper should list processes");
    let silent = processes
        .iter()
        .find(|process| !process.is_running_output)
        .expect("some process is not playing audio");

    println!("recording {} (pid {})", silent.display_name(), silent.pid);
    let mut source = HelperSource::start(TapTarget::Processes(vec![silent.pid]))
        .expect("a process-scoped tap should start");

    drain(&mut source, 3);
    let nonzero = source.nonzero_samples();
    source.stop().expect("helper should stop cleanly");

    assert_eq!(
        nonzero, 0,
        "a silent process leaked {nonzero} non-zero samples — scoping is not working"
    );
}

/// End-to-end: a listen-only session produces a playable WAV with real audio in it.
#[test]
#[ignore = "needs the helper built, permission granted, and audio playing"]
fn a_listen_only_session_writes_a_playable_wav() {
    use hearsay_audio::{Mode, Recording};

    let path = std::env::temp_dir().join(format!("hearsay-session-{}.wav", std::process::id()));
    let recording = Recording::start(Mode::ListenOnly, TapTarget::SystemWide, &path)
        .expect("recording should start");

    std::thread::sleep(Duration::from_secs(5));
    let status = recording.status();
    println!("mid-session: {status:?}");

    let outcome = recording.stop().expect("recording should stop cleanly");
    println!(
        "wrote {} frames ({} ms) to {}",
        outcome.frames,
        outcome.duration_ms,
        outcome.path.display()
    );

    assert!(outcome.frames > 0, "no audio was written");
    assert!(
        outcome.produced_audio,
        "the session wrote {} frames and every one was silent",
        outcome.frames
    );

    // Listen-only is one channel: the tap's stereo is one voice, not two.
    let reader = hound::WavReader::open(&path).expect("the file should be a readable wav");
    assert_eq!(reader.spec().channels, 1);
    assert_eq!(reader.spec().sample_rate, outcome.format.sample_rate);
    assert!(reader.len() > 0, "the wav header reports no samples");

    let _ = std::fs::remove_file(&path);
}

/// Conversation mode: one stereo file, microphone on the left, system audio on the right.
///
/// Opening the microphone triggers the macOS permission prompt the first time. If this
/// test fails with a permission error, grant Microphone access to whatever launched it
/// in System Settings → Privacy & Security → Microphone.
#[test]
#[ignore = "opens the microphone — needs macOS microphone permission"]
fn conversation_mode_writes_the_mic_left_and_system_right() {
    use hearsay_audio::{Mode, Recording};

    let path = std::env::temp_dir().join(format!("hearsay-stereo-{}.wav", std::process::id()));
    let recording = Recording::start(Mode::Conversation, TapTarget::SystemWide, &path)
        .expect("conversation recording should start");

    std::thread::sleep(Duration::from_secs(4));
    let status = recording.status();
    println!("mid-session: {status:?}");

    let outcome = recording.stop().expect("recording should stop cleanly");
    println!(
        "wrote {} frames ({} ms)",
        outcome.frames, outcome.duration_ms
    );

    let reader = hound::WavReader::open(&path).expect("the file should be a readable wav");
    assert_eq!(reader.spec().channels, 2, "conversation mode must be stereo");

    let samples: Vec<i16> = reader
        .into_samples::<i16>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("samples decode");
    assert!(!samples.is_empty(), "no audio was written");

    // Interleaved: even indices are left (mic), odd are right (system).
    let left_peak = samples.iter().step_by(2).map(|s| s.abs()).max().unwrap_or(0);
    let right_peak = samples
        .iter()
        .skip(1)
        .step_by(2)
        .map(|s| s.abs())
        .max()
        .unwrap_or(0);
    println!("left (mic) peak: {left_peak}, right (system) peak: {right_peak}");

    // Exactly one frame count for both channels — that is the alignment guarantee.
    assert_eq!(samples.len() % 2, 0, "stereo output must be whole frames");
    assert!(
        status.has_mic_audio || left_peak > 0,
        "the microphone channel is entirely silent — check microphone permission"
    );

    let _ = std::fs::remove_file(&path);
}

/// The retroactive scrub, end to end: everything spoken before the scrub is erased
/// before it ever reaches the file, and everything after it is recorded normally.
///
/// The recording is far shorter than the 60-second window, so at the moment of the
/// scrub the entire session is still in the buffer — which is exactly the case the
/// feature exists for.
#[test]
#[ignore = "opens the microphone — needs macOS microphone permission"]
fn scrubbing_erases_microphone_audio_before_it_reaches_the_file() {
    use hearsay_audio::{Mode, Recording};

    let path = std::env::temp_dir().join(format!("hearsay-scrub-{}.wav", std::process::id()));
    let recording = Recording::start(Mode::Conversation, TapTarget::SystemWide, &path)
        .expect("conversation recording should start");

    std::thread::sleep(Duration::from_secs(3));
    let erased = recording.scrub_microphone().expect("scrub should succeed");
    println!("scrubbed {erased} microphone samples");
    assert!(erased > 0, "the scrub found nothing buffered to erase");

    std::thread::sleep(Duration::from_secs(3));
    let outcome = recording.stop().expect("recording should stop cleanly");
    println!("wrote {} frames ({} ms)", outcome.frames, outcome.duration_ms);

    let reader = hound::WavReader::open(&path).expect("the file should be a readable wav");
    let spec = reader.spec();
    let samples: Vec<i16> = reader
        .into_samples::<i16>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("samples decode");

    let left: Vec<i16> = samples.iter().copied().step_by(2).collect();
    let rate = spec.sample_rate as usize;

    // The first ~2.5 s of microphone audio was inside the buffer when the scrub ran, so
    // it must be gone. Stopping short of the 3 s mark leaves room for scheduling jitter
    // between the scrub and the samples that arrived alongside it.
    let scrubbed_region = &left[..(rate * 5 / 2).min(left.len())];
    let survivors = scrubbed_region.iter().filter(|s| **s != 0).count();
    assert_eq!(
        survivors, 0,
        "{survivors} microphone samples survived the scrub — audio the user asked to \
         erase reached the file"
    );

    // And the session as a whole still covers real elapsed time: scrubbing erases
    // content, it does not shorten the recording.
    let seconds = left.len() as f64 / rate as f64;
    assert!(
        seconds > 5.0,
        "the recording is only {seconds:.1}s — the scrub appears to have dropped frames \
         instead of zeroing them"
    );

    let _ = std::fs::remove_file(&path);
}
