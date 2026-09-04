//! What a microphone that goes away mid-recording does to the file.
//!
//! Start a `conversation` recording with AirPods in and then disconnect them, and the
//! input device is gone for good: `MicSource` binds one device for the life of the
//! session, so no buffer ever arrives again. The recording is meant to lose the left
//! channel and nothing else.
//!
//! What it used to lose was the meeting. `Mixer::committable_frames` waits for both
//! sides of every frame — which is what keeps the two channels aligned — so a mic queue
//! that never grows again holds the *system* channel back with it. The file stopped
//! growing, the system backlog reached its cap, and every frame after that was evicted.
//! Dropped audio leaves no marker in the transcript, so the rest of the meeting was not
//! delayed or truncated but destroyed.
//!
//! These run against the real [`Mixer`] and [`WavWriter`] with deterministic input, so
//! they need no devices and cannot be flaky. `device_switch.rs` covers the live path.

use hearsay_audio::mixer::{Channel, Mixer};
use hearsay_audio::{AudioFormat, WavWriter};

const SAMPLE_RATE: u32 = 48_000;
const CHUNK_FRAMES: usize = 480; // 10 ms, the size a device typically delivers
const TOTAL_CHUNKS: usize = 60; // 600 ms
/// Frames held back from the writer. Shorter than the real scrub window, which would
/// make the test a minute long for no extra coverage.
const DELAY_FRAMES: usize = CHUNK_FRAMES * 5;

/// Chunks the microphone may miss before the file stops waiting on it.
///
/// Deliberately not the recorder's `MIC_STALL_GRACE`, which is private and measured in
/// seconds against a wall clock. What is being tested here is the mechanism — that
/// padding releases the system channel and keeps the two aligned — and the constant
/// itself is pinned in the recorder's own tests.
const GRACE_CHUNKS: usize = 2;

/// A deterministic, always-non-zero signal, distinct per channel so a mix-up between
/// them would be obvious rather than passing silently.
fn signal(channel_seed: f32, frame: usize) -> f32 {
    let phase = frame as f32 * 0.01 + channel_seed;
    0.35 * phase.sin() + 0.4 * channel_seed
}

struct Recorded {
    left: Vec<i16>,
    right: Vec<i16>,
    dropped_frames: u64,
}

/// Runs the pipeline with the microphone delivering nothing across `dead_chunks`, in the
/// order the writer thread does it: pad a stalled microphone, then commit whatever has
/// aged past the delay.
fn record(name: &str, dead_chunks: std::ops::Range<usize>) -> Recorded {
    let mut path = std::env::temp_dir();
    path.push(format!("hearsay-miclost-{}-{name}.wav", std::process::id()));

    let format = AudioFormat::new(SAMPLE_RATE, 2);
    let mut writer = WavWriter::create(&path, format).expect("writer is created");
    let mut mixer = Mixer::with_delay(SAMPLE_RATE, 2, DELAY_FRAMES);

    let mut stalled_chunks = 0usize;

    for chunk in 0..TOTAL_CHUNKS {
        let base = chunk * CHUNK_FRAMES;

        // The system tap is unaffected by the microphone going away, and keeps
        // delivering for the whole recording.
        let system: Vec<f32> = (0..CHUNK_FRAMES).map(|i| signal(2.0, base + i)).collect();
        mixer.push(Channel::System, &system);

        if dead_chunks.contains(&chunk) {
            // Nothing at all: not zeros, not a short buffer. A device that has gone
            // away does not call back.
            stalled_chunks += 1;
        } else {
            let mic: Vec<f32> = (0..CHUNK_FRAMES).map(|i| signal(1.0, base + i)).collect();
            mixer.push(Channel::Mic, &mic);
            stalled_chunks = 0;
        }

        if stalled_chunks > GRACE_CHUNKS {
            mixer.pad_mic_to_system();
        }

        let ready = mixer.committable_frames();
        if ready > 0 {
            writer.write_samples(&mixer.take(ready)).expect("frames write");
        }
    }

    writer.finalize().expect("finalise succeeds");

    let mut reader = hound::WavReader::open(&path).expect("the file is a readable wav");
    let samples: Vec<i16> = reader
        .samples::<i16>()
        .collect::<Result<_, _>>()
        .expect("samples decode");

    let left = samples.iter().copied().step_by(2).collect::<Vec<i16>>();
    let right = samples.iter().copied().skip(1).step_by(2).collect::<Vec<i16>>();

    let _ = std::fs::remove_file(&path);

    Recorded {
        left,
        right,
        dropped_frames: mixer.dropped_frames(),
    }
}

/// Every frame the recording should hold, given the tail still inside the delay.
fn expected_frames() -> usize {
    TOTAL_CHUNKS * CHUNK_FRAMES - DELAY_FRAMES
}

/// The regression, stated on the file: a dead microphone costs one channel, not the
/// recording.
#[test]
fn a_microphone_that_dies_does_not_stop_the_file() {
    let died_at = 20;
    let recorded = record("dies", died_at..TOTAL_CHUNKS);

    assert_eq!(
        recorded.right.len(),
        expected_frames(),
        "the file stopped growing when the microphone went away"
    );
    assert_eq!(
        recorded.dropped_frames, 0,
        "system audio was captured and then thrown away — the loss with no marker in \
         the transcript"
    );
}

/// The system channel has to be untouched by the whole business: every frame present,
/// in position, and not shifted by the gap on the other side.
#[test]
fn the_system_channel_survives_the_microphone_going_away() {
    let recorded = record("system-intact", 20..TOTAL_CHUNKS);

    assert_eq!(
        recorded.right.len(),
        expected_frames(),
        "the system channel is short — frames it captured never reached the file"
    );

    for (frame, sample) in recorded.right.iter().enumerate() {
        let expected = (signal(2.0, frame) * i16::MAX as f32) as i16;
        assert!(
            (*sample as i32 - expected as i32).abs() <= 1,
            "the system channel is wrong at frame {frame}: {sample} against {expected}"
        );
    }
}

/// Byte-identical, against a recording where nothing went wrong. The strongest form of
/// the promise above: losing the microphone must not perturb the other side of the
/// conversation by so much as one sample, and must not move it in time.
#[test]
fn losing_the_microphone_does_not_disturb_the_other_party() {
    let healthy = record("control", 0..0);
    let lost = record("compare", 20..TOTAL_CHUNKS);

    assert_eq!(healthy.right.len(), lost.right.len());
    assert_eq!(
        healthy.right, lost.right,
        "the system channel differs from the same recording made with a working \
         microphone"
    );
}

/// What was captured before the device went is kept; what comes after is true zeros.
/// The zeros are what the `no_microphone` span in the transcript accounts for.
#[test]
fn audio_captured_before_the_microphone_died_is_kept() {
    let died_at = 20;
    let recorded = record("before-and-after", died_at..TOTAL_CHUNKS);
    let death = died_at * CHUNK_FRAMES;

    assert!(
        recorded.left[..death].iter().any(|sample| *sample != 0),
        "the microphone audio captured before the device went away was lost"
    );
    assert!(
        recorded.left[death..].iter().all(|sample| *sample == 0),
        "something reached the mic channel after the device stopped delivering"
    );
}

/// The reason padding is a queue operation and not a flag saying "stop waiting". Audio
/// captured after the device recovers has to land beside the system audio captured at
/// the same moment; if it landed in the gap instead, the left channel would be
/// permanently ahead of the right and the speaker attribution would go with it.
#[test]
fn a_microphone_that_comes_back_lands_where_it_left_off() {
    let (died_at, returned_at) = (20, 40);
    let recorded = record("recovers", died_at..returned_at);

    assert_eq!(recorded.right.len(), expected_frames());
    assert_eq!(recorded.dropped_frames, 0);

    let gap = died_at * CHUNK_FRAMES..returned_at * CHUNK_FRAMES;
    assert!(
        recorded.left[gap.clone()].iter().all(|sample| *sample == 0),
        "the gap has to be true silence"
    );

    // Beyond the gap, the two channels have to line up again frame for frame.
    for frame in gap.end..recorded.left.len() {
        let expected = (signal(1.0, frame) * i16::MAX as f32) as i16;
        assert!(
            (recorded.left[frame] as i32 - expected as i32).abs() <= 1,
            "the recovered microphone is out of step at frame {frame}: {} against \
             {expected} — the channels have drifted apart",
            recorded.left[frame]
        );
    }
}
