//! Detecting the other party's voice bleeding into the microphone.
//!
//! On speakers, what the far end says comes out of the laptop, crosses the air, and
//! arrives back in the microphone a few tens of milliseconds later. The left channel
//! then contains a faint copy of the right, and speaker attribution — the entire point
//! of recording the two separately — starts describing the wrong person.
//!
//! The fix is a suggestion, not a filter: cross-correlate the two channels, and if the
//! microphone looks like a delayed copy of the system audio, say so and recommend
//! headphones. Deliberately **no** acoustic echo cancellation, adaptive filter, or
//! double-talk detector — those are a large, delicate subsystem, and a one-line banner
//! solves the problem better by removing its cause.

/// Air travel plus buffering. Below this the correlation is the two people genuinely
/// talking over each other; above it, it is no longer plausibly an echo of the same
/// sound in the same room.
const MIN_LAG_MS: u32 = 20;
const MAX_LAG_MS: u32 = 150;

/// Correlation strong enough to be worth mentioning.
///
/// Speech against unrelated speech sits well below this. Set high enough that a banner
/// means something — a warning that fires on a good setup teaches the user to ignore it.
const CORRELATION_THRESHOLD: f32 = 0.30;

/// Analysis runs on a decimated copy: an echo shows up in the energy envelope, and 8 kHz
/// turns a search that would be hundreds of millions of multiplies into a few million.
const ANALYSIS_RATE: u32 = 8_000;

/// A channel quieter than this carries no speech to correlate, and correlating noise
/// against noise produces confident nonsense.
const SILENCE_FLOOR: f32 = 1e-4;

/// What the detector found.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct EchoDetection {
    /// Delay at which the microphone best matches the system audio.
    pub lag_ms: u32,
    /// Normalised correlation at that lag, 0 to 1.
    pub correlation: f32,
}

/// Looks for the system channel echoing back through the microphone.
///
/// `mic` and `system` are mono, same length, same rate. Returns `None` when there is
/// nothing to worry about — including when either channel is too quiet to judge, which
/// is the common case and must never produce a warning.
pub fn detect_bleed(mic: &[f32], system: &[f32], sample_rate: u32) -> Option<EchoDetection> {
    if sample_rate == 0 {
        return None;
    }

    let mic = decimate(mic, sample_rate, ANALYSIS_RATE);
    let system = decimate(system, sample_rate, ANALYSIS_RATE);

    let min_lag = (MIN_LAG_MS * ANALYSIS_RATE / 1000) as usize;
    let max_lag = (MAX_LAG_MS * ANALYSIS_RATE / 1000) as usize;

    // Enough signal left after the longest lag to be worth correlating: about a second.
    let usable = mic.len().min(system.len());
    if usable <= max_lag + ANALYSIS_RATE as usize / 2 {
        return None;
    }

    if rms(&mic) < SILENCE_FLOOR || rms(&system) < SILENCE_FLOOR {
        return None;
    }

    let window = usable - max_lag;
    let system_energy = energy(&system[..window]).sqrt();
    if system_energy <= 0.0 {
        return None;
    }

    let mut best: Option<EchoDetection> = None;

    for lag in min_lag..=max_lag {
        let shifted = &mic[lag..lag + window];
        let mic_energy = energy(shifted).sqrt();
        if mic_energy <= 0.0 {
            continue;
        }

        let mut dot = 0.0f32;
        for index in 0..window {
            dot += shifted[index] * system[index];
        }
        // Normalised: this measures shape, not loudness, so a faint echo scores as
        // highly as a loud one. That matters — bleed is quiet by nature.
        let correlation = (dot / (mic_energy * system_energy)).abs();

        if best.map_or(true, |current| correlation > current.correlation) {
            best = Some(EchoDetection {
                lag_ms: (lag as u32 * 1000) / ANALYSIS_RATE,
                correlation,
            });
        }
    }

    best.filter(|found| found.correlation >= CORRELATION_THRESHOLD)
}

/// Splits interleaved stereo into its two mono channels.
pub fn split_stereo(interleaved: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let frames = interleaved.len() / 2;
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    for frame in 0..frames {
        left.push(interleaved[frame * 2]);
        right.push(interleaved[frame * 2 + 1]);
    }
    (left, right)
}

/// Averages down to `to_rate`. Averaging rather than picking every Nth sample: plain
/// decimation aliases high frequencies into the band being correlated and invents
/// structure that was never there.
fn decimate(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate <= to_rate || samples.is_empty() {
        return samples.to_vec();
    }
    let factor = (from_rate / to_rate) as usize;
    if factor <= 1 {
        return samples.to_vec();
    }

    samples
        .chunks(factor)
        .map(|chunk| chunk.iter().sum::<f32>() / chunk.len() as f32)
        .collect()
}

fn energy(samples: &[f32]) -> f32 {
    samples.iter().map(|s| s * s).sum()
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (energy(samples) / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// Speech-like signal: aperiodic, so it does not correlate with itself at arbitrary
    /// lags the way a sum of sines does. A periodic test signal would make the detector
    /// look far more confident than it is on real audio.
    ///
    /// Deterministic (a fixed LCG), so a failure is always reproducible.
    fn speech(len: usize, seed: u32) -> Vec<f32> {
        let mut state = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        let mut previous = 0.0f32;
        (0..len)
            .map(|i| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (state >> 8) as f32 / 8_388_608.0 - 1.0;
                // Low-pass and syllable-rate envelope: broadband, but shaped like voice.
                previous = previous * 0.7 + noise * 0.3;
                let t = i as f32 / RATE as f32;
                let envelope = 0.5 + 0.5 * (t * 7.0).sin();
                previous * envelope
            })
            .collect()
    }

    #[test]
    fn a_delayed_attenuated_copy_is_detected_as_bleed() {
        let len = RATE as usize * 2;
        let system = speech(len, 1);

        // The far end, 60 ms later and much quieter — laptop speakers into the mic.
        let delay = (RATE as usize * 60) / 1000;
        let mut mic = vec![0.0f32; len];
        for index in delay..len {
            mic[index] = system[index - delay] * 0.15;
        }

        let found = detect_bleed(&mic, &system, RATE).expect("bleed should be detected");
        println!("detected: {found:?}");
        assert!(
            (found.lag_ms as i32 - 60).abs() <= 15,
            "lag {} ms is not close to the 60 ms that was injected",
            found.lag_ms
        );
        assert!(found.correlation >= CORRELATION_THRESHOLD);
    }

    /// The case that must not warn: headphones. Two people talking, no acoustic path.
    #[test]
    fn two_unrelated_speakers_do_not_look_like_bleed() {
        let len = RATE as usize * 2;
        let system = speech(len, 1);
        let mic = speech(len, 99);

        assert_eq!(
            detect_bleed(&mic, &system, RATE),
            None,
            "unrelated speech was reported as echo — this banner would cry wolf"
        );
    }

    #[test]
    fn a_silent_microphone_never_warns() {
        let len = RATE as usize * 2;
        let system = speech(len, 1);
        let mic = vec![0.0f32; len];
        assert_eq!(detect_bleed(&mic, &system, RATE), None);
    }

    #[test]
    fn silence_on_both_channels_never_warns() {
        let len = RATE as usize * 2;
        assert_eq!(detect_bleed(&vec![0.0; len], &vec![0.0; len], RATE), None);
    }

    /// Bleed is still found when the user is quiet or speaking softly — which is the
    /// case that matters, because detection runs repeatedly and only needs one such
    /// window during a meeting.
    #[test]
    fn bleed_is_found_when_the_user_is_not_dominating_the_channel() {
        let len = RATE as usize * 2;
        let system = speech(len, 1);
        let own_voice = speech(len, 42);

        let delay = (RATE as usize * 45) / 1000;
        let mut mic: Vec<f32> = own_voice.iter().map(|s| s * 0.3).collect();
        for index in delay..len {
            mic[index] += system[index - delay] * 0.25;
        }

        let found = detect_bleed(&mic, &system, RATE)
            .expect("bleed alongside quiet speech should be detected");
        println!("under-voice detection: {found:?}");
        assert!((found.lag_ms as i32 - 45).abs() <= 20, "lag was {} ms", found.lag_ms);
    }

    /// The documented limit. When the user is talking at full volume *over* the echo,
    /// the correlation is diluted below threshold and no banner appears on that sample.
    ///
    /// This is deliberate. Separating simultaneous speech from its own echo is
    /// double-talk detection, which `CLAUDE.md` §7 rules out — and it costs nothing,
    /// because detection re-runs every minute and a meeting always contains stretches
    /// where the user is listening rather than talking.
    #[test]
    fn simultaneous_full_volume_speech_defers_to_a_later_sample() {
        let len = RATE as usize * 2;
        let system = speech(len, 1);
        let own_voice = speech(len, 42);

        let delay = (RATE as usize * 45) / 1000;
        let mut mic = own_voice.clone();
        for index in delay..len {
            mic[index] += system[index - delay] * 0.25;
        }

        assert_eq!(
            detect_bleed(&mic, &system, RATE),
            None,
            "double-talk was reported; the threshold is now low enough to produce false \
             positives on unrelated speech"
        );
    }

    #[test]
    fn too_little_audio_to_judge_returns_nothing() {
        let short = speech(1_000, 1);
        assert_eq!(detect_bleed(&short, &short, RATE), None);
    }

    #[test]
    fn stereo_splits_into_left_then_right() {
        let (left, right) = split_stereo(&[1.0, -1.0, 2.0, -2.0]);
        assert_eq!(left, vec![1.0, 2.0]);
        assert_eq!(right, vec![-1.0, -2.0]);
    }
}
