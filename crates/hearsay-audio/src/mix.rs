//! Turning what a source produces into what a channel of the WAV needs.
//!
//! One distinction matters and is easy to blur: the *tap* delivers stereo, because
//! that is how the machine plays audio. That stereo pair is a single voice in Hearsay's
//! terms — "everyone else" — so it is folded to one channel before being written.
//!
//! That is not the same as mixing to mono. The microphone and the system audio are never
//! combined; they stay on the left and right of the output file, which is the whole
//! basis for knowing who said what.

/// Folds an interleaved multi-channel buffer down to one channel by averaging.
///
/// Averaging rather than taking the first channel: audio panned hard to one side would
/// otherwise vanish, and a caller heard only in the right ear would be missing from the
/// transcript entirely.
pub fn downmix_to_mono(samples: &[f32], channels: u16) -> Vec<f32> {
    let channels = channels.max(1) as usize;
    if channels == 1 {
        return samples.to_vec();
    }

    let frames = samples.len() / channels;
    let mut mono = Vec::with_capacity(frames);
    let scale = 1.0 / channels as f32;

    for frame in 0..frames {
        let base = frame * channels;
        let mut sum = 0.0f32;
        for channel in 0..channels {
            // The slice length was checked by the frame count, so this cannot be out of
            // range; `get` keeps it that way if the arithmetic is ever changed.
            sum += samples.get(base + channel).copied().unwrap_or(0.0);
        }
        mono.push(sum * scale);
    }
    mono
}

/// Interleaves two mono channels into one stereo buffer.
///
/// The shorter side is padded with true zeros rather than truncating the longer one:
/// dropping the tail would lose audio, and the two channels must stay sample-aligned or
/// speaker attribution drifts apart over the length of a meeting.
pub fn interleave_stereo(left: &[f32], right: &[f32]) -> Vec<f32> {
    let frames = left.len().max(right.len());
    let mut out = Vec::with_capacity(frames * 2);
    for frame in 0..frames {
        out.push(left.get(frame).copied().unwrap_or(0.0));
        out.push(right.get(frame).copied().unwrap_or(0.0));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_folds_to_the_average_of_both_sides() {
        let samples = [1.0, 0.0, 0.5, 0.5, -1.0, 1.0];
        assert_eq!(downmix_to_mono(&samples, 2), vec![0.5, 0.5, 0.0]);
    }

    #[test]
    fn audio_panned_hard_to_one_side_survives_the_downmix() {
        // Only the right channel carries anything; it must not disappear.
        let samples = [0.0, 0.8, 0.0, 0.6];
        let mono = downmix_to_mono(&samples, 2);
        assert!(mono.iter().all(|sample| *sample > 0.0), "got {mono:?}");
    }

    #[test]
    fn mono_input_passes_through_untouched() {
        let samples = [0.1, 0.2, 0.3];
        assert_eq!(downmix_to_mono(&samples, 1), samples.to_vec());
    }

    #[test]
    fn interleaving_keeps_the_two_sides_apart() {
        let left = [1.0, 2.0];
        let right = [-1.0, -2.0];
        assert_eq!(interleave_stereo(&left, &right), vec![1.0, -1.0, 2.0, -2.0]);
    }

    #[test]
    fn a_shorter_channel_is_padded_rather_than_truncating_the_other() {
        let left = [1.0];
        let right = [-1.0, -2.0, -3.0];
        let stereo = interleave_stereo(&left, &right);
        assert_eq!(stereo, vec![1.0, -1.0, 0.0, -2.0, 0.0, -3.0]);
    }
}
