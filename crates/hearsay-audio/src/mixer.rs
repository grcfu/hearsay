//! Keeping two independent capture devices aligned in one file.
//!
//! The microphone and the system tap are separate devices with separate clocks, and
//! neither is a usable master clock on its own: the tap stops producing entirely when
//! the output device goes quiet, and the microphone stops if it is unplugged. Using
//! either as the timebase would make a pause in one channel silently compress the other.
//!
//! So **wall-clock time is the master**. Every tick the writer asks for exactly as many
//! frames as have elapsed since recording started, and each channel supplies what it has
//! — padded with true zeros where it has nothing. The file's duration therefore always
//! matches real elapsed time, and a sample at position T in the left channel was
//! captured at the same moment as position T in the right.

use std::collections::VecDeque;

/// How far a channel may run ahead of the wall clock before the oldest audio is dropped.
///
/// Two devices at a nominal 48 kHz still drift by a few samples per minute. Without a
/// cap, the faster one's backlog grows for the length of the meeting and its channel
/// ends up increasingly delayed. One second is far beyond any legitimate scheduling
/// jitter and far below anything a listener would notice being trimmed.
const MAX_BACKLOG_SECONDS: f32 = 1.0;

/// Which channel a buffer belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Left. The user's microphone. Only present in conversation mode.
    Mic,
    /// Right in conversation mode, the only channel in listen-only mode.
    System,
}

/// Buffers both channels and emits interleaved frames on demand.
pub struct Mixer {
    mic: VecDeque<f32>,
    system: VecDeque<f32>,
    sample_rate: u32,
    channels: u16,
    max_backlog: usize,
    /// While true, microphone samples are replaced by zeros on the way in.
    ///
    /// Deliberately applied here rather than by stopping the device: the input stream
    /// keeps running, so there is no reopen, no permission re-prompt, and no gap in the
    /// timeline — just zeros. See `CLAUDE.md` §5.
    muted: bool,
    dropped_frames: u64,
}

impl Mixer {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let max_backlog = (sample_rate as f32 * MAX_BACKLOG_SECONDS) as usize;
        Self {
            mic: VecDeque::new(),
            system: VecDeque::new(),
            sample_rate,
            channels,
            max_backlog: max_backlog.max(1),
            muted: false,
            dropped_frames: 0,
        }
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    /// Frames discarded because a channel ran too far ahead of the clock.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames
    }

    /// Accepts mono samples for one channel.
    pub fn push(&mut self, channel: Channel, samples: &[f32]) {
        let queue = match channel {
            Channel::Mic => &mut self.mic,
            Channel::System => &mut self.system,
        };

        if channel == Channel::Mic && self.muted {
            // Zeros, not a skip: skipping would shorten the left channel and shift
            // everything after it out of alignment with the right.
            queue.extend(std::iter::repeat(0.0).take(samples.len()));
        } else {
            queue.extend(samples.iter().copied());
        }

        if queue.len() > self.max_backlog {
            let excess = queue.len() - self.max_backlog;
            queue.drain(..excess);
            self.dropped_frames += excess as u64;
        }
    }

    /// Zeroes everything the microphone channel is still holding.
    ///
    /// Used by the retroactive scrub: audio that has been captured but not yet committed
    /// to disk is erased in place, keeping the frame count — and therefore the alignment
    /// with the system channel — untouched.
    pub fn scrub_mic(&mut self) -> usize {
        let count = self.mic.len();
        for sample in self.mic.iter_mut() {
            *sample = 0.0;
        }
        count
    }

    /// Removes and returns exactly `frames` frames of interleaved output.
    ///
    /// A channel with nothing buffered contributes silence. That is the correct
    /// reading — nobody was speaking, or nothing was playing — and it keeps the two
    /// channels sample-aligned no matter how the devices are behaving.
    pub fn take(&mut self, frames: usize) -> Vec<f32> {
        let channels = self.channels.max(1) as usize;
        let mut out = Vec::with_capacity(frames * channels);

        for _ in 0..frames {
            if channels == 1 {
                out.push(self.system.pop_front().unwrap_or(0.0));
            } else {
                // Left is the user, right is everyone else. Never mixed together.
                out.push(self.mic.pop_front().unwrap_or(0.0));
                out.push(self.system.pop_front().unwrap_or(0.0));
            }
        }
        out
    }

    /// Frames available on the shorter of the active channels.
    pub fn buffered_frames(&self) -> usize {
        if self.channels <= 1 {
            self.system.len()
        } else {
            self.mic.len().min(self.system.len())
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// Resamples mono audio by linear interpolation.
///
/// The microphone and the output device usually both run at 48 kHz, in which case this
/// is never called. When they differ, linear interpolation is well below the noise floor
/// of a speech recording, and the transcription sidecar resamples again anyway.
pub fn resample_mono(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || from_rate == 0 || samples.is_empty() {
        return samples.to_vec();
    }

    let ratio = to_rate as f64 / from_rate as f64;
    let out_len = ((samples.len() as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);

    for index in 0..out_len {
        let position = index as f64 / ratio;
        let left = position.floor() as usize;
        let fraction = (position - left as f64) as f32;

        let a = samples.get(left).copied().unwrap_or(0.0);
        let b = samples.get(left + 1).copied().unwrap_or(a);
        out.push(a + (b - a) * fraction);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_output_puts_the_mic_on_the_left() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.push(Channel::Mic, &[1.0, 1.0]);
        mixer.push(Channel::System, &[-1.0, -1.0]);

        assert_eq!(mixer.take(2), vec![1.0, -1.0, 1.0, -1.0]);
    }

    #[test]
    fn a_starved_channel_becomes_silence_rather_than_shifting_the_other() {
        let mut mixer = Mixer::new(48_000, 2);
        // The system tap delivered nothing this tick — the output device is idle.
        mixer.push(Channel::Mic, &[0.5, 0.6, 0.7]);

        let frames = mixer.take(3);
        assert_eq!(frames, vec![0.5, 0.0, 0.6, 0.0, 0.7, 0.0]);

        // When the tap resumes, its samples land at the next frame — still aligned.
        mixer.push(Channel::System, &[-0.5]);
        mixer.push(Channel::Mic, &[0.8]);
        assert_eq!(mixer.take(1), vec![0.8, -0.5]);
    }

    #[test]
    fn asking_for_more_frames_than_exist_yields_silence_not_a_short_buffer() {
        let mut mixer = Mixer::new(48_000, 2);
        let frames = mixer.take(2);
        assert_eq!(frames.len(), 4, "the output must always be the length asked for");
        assert!(frames.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn listen_only_output_is_one_channel_of_system_audio() {
        let mut mixer = Mixer::new(48_000, 1);
        mixer.push(Channel::System, &[0.1, 0.2]);
        assert_eq!(mixer.take(2), vec![0.1, 0.2]);
    }

    /// Muting must write zeros, not drop samples — dropping would shorten the left
    /// channel and desynchronise everything after the muted span.
    #[test]
    fn muting_writes_zeros_and_preserves_frame_count() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_muted(true);
        mixer.push(Channel::Mic, &[0.9, 0.9, 0.9]);
        mixer.push(Channel::System, &[0.1, 0.2, 0.3]);

        let frames = mixer.take(3);
        assert_eq!(frames, vec![0.0, 0.1, 0.0, 0.2, 0.0, 0.3]);
    }

    #[test]
    fn unmuting_resumes_recording_the_microphone() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_muted(true);
        mixer.push(Channel::Mic, &[0.9]);
        mixer.set_muted(false);
        mixer.push(Channel::Mic, &[0.4]);
        mixer.push(Channel::System, &[0.0, 0.0]);

        assert_eq!(mixer.take(2), vec![0.0, 0.0, 0.4, 0.0]);
    }

    #[test]
    fn the_system_channel_is_never_muted() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_muted(true);
        mixer.push(Channel::Mic, &[1.0]);
        mixer.push(Channel::System, &[0.75]);

        let frames = mixer.take(1);
        assert_eq!(frames, vec![0.0, 0.75]);
    }

    #[test]
    fn scrubbing_zeroes_pending_mic_audio_without_changing_its_length() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.push(Channel::Mic, &[0.9, 0.8, 0.7]);
        mixer.push(Channel::System, &[0.1, 0.2, 0.3]);

        let scrubbed = mixer.scrub_mic();
        assert_eq!(scrubbed, 3);

        let frames = mixer.take(3);
        assert_eq!(frames, vec![0.0, 0.1, 0.0, 0.2, 0.0, 0.3]);
    }

    #[test]
    fn a_runaway_channel_is_trimmed_rather_than_growing_without_bound() {
        let mut mixer = Mixer::new(1_000, 2); // 1 s cap = 1000 samples
        mixer.push(Channel::Mic, &vec![0.5; 1_500]);
        assert!(mixer.dropped_frames() > 0);
        assert!(mixer.buffered_frames() <= 1_000);
    }

    #[test]
    fn resampling_is_a_no_op_at_a_matching_rate() {
        let samples = [0.1, 0.2, 0.3];
        assert_eq!(resample_mono(&samples, 48_000, 48_000), samples.to_vec());
    }

    #[test]
    fn resampling_scales_length_by_the_rate_ratio() {
        let samples = vec![0.5f32; 16_000];
        let out = resample_mono(&samples, 16_000, 48_000);
        assert_eq!(out.len(), 48_000);
        assert!(out.iter().all(|sample| (*sample - 0.5).abs() < 1e-6));
    }
}
