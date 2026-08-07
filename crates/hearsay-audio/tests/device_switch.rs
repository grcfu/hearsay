//! What happens when the output device changes mid-recording.
//!
//! `CLAUDE.md` §3 promises "AirPods connect and disconnect freely". The aggregate device
//! is created with the current default output device as its clock source, captured once
//! at start — so this asserts the recording survives that device being swapped out from
//! under it, which is exactly what connecting or disconnecting AirPods does.

use hearsay_audio::{AudioSource, Chunk, HelperSource, TapTarget};
use std::process::Command;
use std::time::{Duration, Instant};

fn switch_output(setdev: &str, id: &str) {
    let out = Command::new(setdev).arg(id).output().expect("setdev runs");
    println!("  {}", String::from_utf8_lossy(&out.stdout).trim());
}

/// Reads for `seconds`, returning frames seen.
fn drain(source: &mut HelperSource, seconds: f32) -> u64 {
    let format = source.format();
    let deadline = Instant::now() + Duration::from_secs_f32(seconds);
    let mut frames = 0u64;
    while Instant::now() < deadline {
        match source.next_chunk_timeout(Duration::from_millis(200)) {
            Chunk::Samples(s) => frames += format.frames(s.len()) as u64,
            Chunk::Idle => continue,
            Chunk::Finished => break,
        }
    }
    frames
}

#[test]
#[ignore = "needs two output devices; set HEARSAY_SETDEV, HEARSAY_DEV_A, HEARSAY_DEV_B"]
fn a_recording_survives_the_output_device_changing() {
    let setdev = std::env::var("HEARSAY_SETDEV").expect("HEARSAY_SETDEV");
    let dev_a = std::env::var("HEARSAY_DEV_A").expect("HEARSAY_DEV_A");
    let dev_b = std::env::var("HEARSAY_DEV_B").expect("HEARSAY_DEV_B");

    let mut source = HelperSource::start(TapTarget::SystemWide).expect("tap starts");
    println!("format: {:?}", source.format());

    let before = drain(&mut source, 3.0);
    println!("frames before switch: {before}");

    println!("switching output device (simulating AirPods connecting):");
    switch_output(&setdev, &dev_b);
    let during = drain(&mut source, 3.0);
    println!("frames after switching away: {during}");

    println!("switching back (simulating AirPods disconnecting):");
    switch_output(&setdev, &dev_a);
    let after = drain(&mut source, 3.0);
    println!("frames after switching back: {after}");

    source.stop().expect("stops cleanly");
    // Always restore, even if an assertion below fails.
    switch_output(&setdev, &dev_a);

    assert!(before > 0, "the tap delivered nothing before the switch");
    assert!(
        during > 0,
        "the tap stopped delivering frames when the output device changed — a recording \
         would go silent the moment AirPods connect"
    );
    assert!(
        after > 0,
        "the tap did not recover after the output device changed back"
    );
}
