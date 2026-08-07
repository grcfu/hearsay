//! The mute guarantee, asserted on the bytes that actually reach disk.
//!
//! Muting has to do exactly two things and nothing else:
//!
//! 1. The left channel is **exactly zero** for the whole muted span — not attenuated,
//!    not nearly zero, not a fade. If a single non-zero sample survives, someone's voice
//!    survived with it.
//! 2. The right channel is **byte-identical** to the same recording made unmuted. Muting
//!    the microphone must not perturb the other side of the conversation by so much as
//!    one sample, and must not shift it in time.
//!
//! These run against the real [`Mixer`] and [`WavWriter`] with deterministic input, so
//! they need no devices and can't be flaky. The live end-to-end path is covered
//! separately in `live_capture.rs`.

use hearsay_audio::mixer::{Channel, Mixer};
use hearsay_audio::{AudioFormat, WavWriter};
use std::path::PathBuf;

const SAMPLE_RATE: u32 = 48_000;
const CHUNK_FRAMES: usize = 480; // 10 ms, the size a device typically delivers
const TOTAL_CHUNKS: usize = 40; // 400 ms
const MUTE_FROM_CHUNK: usize = 10;
const MUTE_TO_CHUNK: usize = 25;

/// A deterministic, always-non-zero signal. Distinct per channel so a mix-up between
/// them would be obvious rather than passing silently.
fn signal(channel_seed: f32, frame: usize) -> f32 {
    let phase = frame as f32 * 0.01 + channel_seed;
    // Offset keeps every sample away from zero, so "is this sample zero?" is a real
    // question rather than one a silent passage could answer by accident.
    0.35 * phase.sin() + 0.4 * channel_seed
}

struct Recorded {
    left: Vec<i16>,
    right: Vec<i16>,
    frames: usize,
}

/// Runs the mute-aware pipeline over a fixed signal, muting across `mute_chunks` if set,
/// and reads back what landed on disk.
fn record(name: &str, mute_chunks: Option<(usize, usize)>) -> Recorded {
    let mut path = std::env::temp_dir();
    path.push(format!("hearsay-mute-{}-{name}.wav", std::process::id()));

    let format = AudioFormat::new(SAMPLE_RATE, 2);
    let mut writer = WavWriter::create(&path, format).expect("writer is created");
    let mut mixer = Mixer::new(SAMPLE_RATE, 2);

    for chunk in 0..TOTAL_CHUNKS {
        if let Some((from, to)) = mute_chunks {
            // Toggled on chunk boundaries, exactly as the hotkey does mid-recording.
            mixer.set_muted(chunk >= from && chunk < to);
        }

        let base = chunk * CHUNK_FRAMES;
        let mic: Vec<f32> = (0..CHUNK_FRAMES).map(|i| signal(1.0, base + i)).collect();
        let system: Vec<f32> = (0..CHUNK_FRAMES).map(|i| signal(2.0, base + i)).collect();

        mixer.push(Channel::Mic, &mic);
        mixer.push(Channel::System, &system);

        let interleaved = mixer.take(CHUNK_FRAMES);
        writer.write_samples(&interleaved).expect("samples write");
    }
    writer.finalize().expect("file finalises");

    let reader = hound::WavReader::open(&path).expect("file is a readable wav");
    assert_eq!(reader.spec().channels, 2);
    let samples: Vec<i16> = reader
        .into_samples::<i16>()
        .collect::<Result<Vec<_>, _>>()
        .expect("samples decode");

    let left = samples.iter().copied().step_by(2).collect::<Vec<i16>>();
    let right = samples.iter().copied().skip(1).step_by(2).collect::<Vec<i16>>();
    let frames = left.len();

    let _ = std::fs::remove_file(&path);
    Recorded { left, right, frames }
}

fn mute_range() -> (usize, usize) {
    (MUTE_FROM_CHUNK * CHUNK_FRAMES, MUTE_TO_CHUNK * CHUNK_FRAMES)
}

/// The guarantee the whole feature exists for.
#[test]
fn the_left_channel_is_exactly_zero_across_a_muted_span() {
    let muted = record("muted", Some((MUTE_FROM_CHUNK, MUTE_TO_CHUNK)));
    let (from, to) = mute_range();

    let offenders: Vec<(usize, i16)> = muted.left[from..to]
        .iter()
        .enumerate()
        .filter(|(_, sample)| **sample != 0)
        .map(|(index, sample)| (from + index, *sample))
        .collect();

    assert!(
        offenders.is_empty(),
        "{} of {} samples in the muted span are non-zero (first few: {:?}). \
         Muting must write true zeros, not attenuation.",
        offenders.len(),
        to - from,
        &offenders[..offenders.len().min(5)]
    );
}

/// Muting must not touch the other side of the conversation.
#[test]
fn the_right_channel_is_byte_identical_whether_or_not_the_mic_was_muted() {
    let open = record("open", None);
    let muted = record("muted-right", Some((MUTE_FROM_CHUNK, MUTE_TO_CHUNK)));

    assert_eq!(
        open.frames, muted.frames,
        "muting changed the length of the recording"
    );
    assert_eq!(
        open.right, muted.right,
        "the system channel differs between a muted and an unmuted recording — muting \
         the microphone must not perturb or shift the other party's audio"
    );
}

/// Muting writes zeros rather than dropping samples, so nothing after the span slides
/// earlier. If it did, every later timestamp — and the speaker attribution that rests on
/// them — would be wrong.
#[test]
fn audio_after_a_muted_span_stays_where_it_was() {
    let open = record("open-tail", None);
    let muted = record("muted-tail", Some((MUTE_FROM_CHUNK, MUTE_TO_CHUNK)));
    let (_, to) = mute_range();

    assert_eq!(open.left[to..], muted.left[to..], "the tail of the mic channel shifted");
    assert_eq!(
        open.left[..mute_range().0],
        muted.left[..mute_range().0],
        "audio before the muted span was altered"
    );
}

/// The control: without muting, the microphone channel really does carry signal. Without
/// this, the zero-check above would pass just as well on a pipeline that records nothing.
#[test]
fn an_unmuted_recording_has_a_non_silent_mic_channel() {
    let open = record("control", None);
    let (from, to) = mute_range();

    let non_zero = open.left[from..to].iter().filter(|s| **s != 0).count();
    assert!(
        non_zero > (to - from) / 2,
        "the unmuted control recorded {non_zero} non-zero samples out of {} — the test \
         signal is not reaching the file, so the mute assertions prove nothing",
        to - from
    );
}

/// A recording stopped while still muted must not lose the span. `Recording::stop`
/// closes the open span; this asserts the underlying bookkeeping directly.
#[test]
fn muting_and_unmuting_is_symmetric() {
    let mut mixer = Mixer::new(SAMPLE_RATE, 2);
    assert!(!mixer.is_muted());
    mixer.set_muted(true);
    assert!(mixer.is_muted());
    mixer.set_muted(false);
    assert!(!mixer.is_muted());

    mixer.push(Channel::Mic, &[0.5, 0.5]);
    mixer.push(Channel::System, &[0.25, 0.25]);
    assert_eq!(mixer.take(2), vec![0.5, 0.25, 0.5, 0.25]);
}

/// The files under test are cleaned up as they are read; this guards against a change
/// that leaves them behind in the user's temp directory.
#[test]
fn recordings_made_by_this_test_do_not_linger() {
    let _ = record("cleanup", None);
    let mut path: PathBuf = std::env::temp_dir();
    path.push(format!("hearsay-mute-{}-cleanup.wav", std::process::id()));
    assert!(!path.exists(), "left {} behind", path.display());
}
