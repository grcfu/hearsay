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
