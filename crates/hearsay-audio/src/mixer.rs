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

/// Slack above the commit delay before the oldest audio is dropped.
///
/// Two devices at a nominal 48 kHz still drift by a few samples per minute. Without a
/// cap, the faster one's backlog grows for the length of the meeting and its channel
/// ends up increasingly delayed. One second is far beyond any legitimate scheduling
/// jitter and far below anything a listener would notice being trimmed.
const BACKLOG_SLACK_SECONDS: f32 = 1.0;

/// How long microphone audio waits before it is committed to disk.
///
/// This is the window the retroactive scrub can reach back into: press ⌘⇧X and
/// everything still inside it is erased before it ever reaches the file. The mute button
/// only helps someone who thought to press it in advance; this is what covers the side
/// conversation you only realised was sensitive after it started.
///
/// **Both** channels are held for this long, not just the microphone. Committing the
/// system channel immediately and the microphone a minute later would put them a minute
/// out of step in the finished file, and every timestamp — and the speaker attribution
/// that rests on it — would be wrong.
pub const SCRUB_WINDOW_SECONDS: u32 = 60;

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
    /// Frames held back from the writer so the scrub has something to erase.
    delay_frames: usize,
    /// Whether a microphone is currently feeding [`Channel::Mic`].
    ///
    /// Separate from `channels`, which describes the *file*. Once a recording has been
    /// upgraded to conversation its file is stereo for good, but the microphone can be
    /// closed again afterwards — and a channel nobody is filling must not be waited on,
    /// or `committable_frames` would return zero for the rest of the recording and the
    /// file would stop growing.
    mic_present: bool,
    /// While true, microphone samples are replaced by zeros on the way in.
    ///
    /// Deliberately applied here rather than by stopping the device: the input stream
    /// keeps running, so there is no reopen, no permission re-prompt, and no gap in the
    /// timeline — just zeros. See `CLAUDE.md` §5.
    muted: bool,
    dropped_frames: u64,
}

impl Mixer {
    /// A mixer that commits audio as soon as it arrives. Used in listen-only mode, where
    /// there is no microphone and therefore nothing to scrub — so the file on disk stays
    /// current instead of trailing a minute behind.
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        Self::with_delay(sample_rate, channels, 0)
    }

    /// A mixer that holds `delay_frames` frames back from the writer.
    pub fn with_delay(sample_rate: u32, channels: u16, delay_frames: usize) -> Self {
        let slack = (sample_rate as f32 * BACKLOG_SLACK_SECONDS) as usize;
        Self {
            mic: VecDeque::new(),
            system: VecDeque::new(),
            sample_rate,
            channels,
            max_backlog: (delay_frames + slack).max(1),
            delay_frames,
            mic_present: channels > 1,
            muted: false,
            dropped_frames: 0,
        }
    }

    /// Frames the scrub can still reach.
    pub fn delay_frames(&self) -> usize {
        self.delay_frames
    }

    /// Widens the output from one channel to two, for a recording being upgraded from
    /// listen-only to conversation mid-session.
    ///
    /// Only ever called with both queues already drained, so the first stereo frame
    /// pairs microphone audio and system audio captured at the same instant. Widening
    /// with a backlog on either side would offset the two channels by the size of that
    /// backlog for the rest of the recording, and every timestamp after the switch —
    /// along with the speaker attribution resting on it — would be wrong.
    pub fn set_channels(&mut self, channels: u16) {
        self.channels = channels;
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Says whether a microphone is feeding the mic channel.
    ///
    /// Set false when the microphone is closed mid-recording: the queue keeps whatever it
    /// still holds — that audio was captured and is still inside the scrub window — and
    /// once it runs dry [`Mixer::take`] pads the channel with true zeros.
    pub fn set_mic_present(&mut self, present: bool) {
        self.mic_present = present;
    }

    pub fn mic_present(&self) -> bool {
        self.mic_present
    }

    /// Changes how long audio is held back before it can be committed.
    ///
    /// Raised from zero when a listen-only recording gains a microphone, because from
    /// that moment there is something for the retroactive scrub to erase. The backlog cap
    /// moves with it: leaving it at the old value would have the mixer trim the very
    /// audio the new delay is asking it to hold, and report it as dropped.
    pub fn set_delay_frames(&mut self, delay_frames: usize) {
        self.delay_frames = delay_frames;
        let slack = (self.sample_rate as f32 * BACKLOG_SLACK_SECONDS) as usize;
        self.max_backlog = (delay_frames + slack).max(1);
    }

    /// Empties both queues and returns everything they held as interleaved frames.
    ///
    /// Used at a mode switch, where the file's channel count is about to change and
    /// nothing may be left buffered across the boundary.
    pub fn drain(&mut self) -> Vec<f32> {
        let frames = self.pending_frames();
        self.take(frames)
    }

    /// Frames old enough to commit — everything buffered beyond the delay.
    ///
    /// In conversation mode this is the shorter of the two channels: a frame is only
    /// committable once both sides of it exist, or they would drift apart.
    pub fn committable_frames(&self) -> usize {
        let buffered = if self.channels <= 1 || !self.mic_present {
            self.system.len()
        } else {
            self.mic.len().min(self.system.len())
        };
        buffered.saturating_sub(self.delay_frames)
    }

    /// Everything still buffered, ignoring the delay. Used to flush at the end of a
    /// recording, where there is no later audio for the scrub to protect.
    pub fn pending_frames(&self) -> usize {
        if self.channels <= 1 {
            self.system.len()
        } else {
            self.mic.len().max(self.system.len())
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
    /// This is the retroactive scrub. Audio captured within the last
    /// [`SCRUB_WINDOW_SECONDS`] but not yet committed to disk is erased in place,
    /// keeping the frame count — and therefore the alignment with the system channel —
    /// untouched. Anything already written to the file is beyond reach and is not
    /// touched; the window is the honest limit of what this can undo.
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
        if self.channels <= 1 || !self.mic_present {
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

    // ---- the scrub window ----

    #[test]
    fn audio_inside_the_delay_is_not_committable_yet() {
        let mut mixer = Mixer::with_delay(1_000, 2, 500);
        mixer.push(Channel::Mic, &vec![0.5; 400]);
        mixer.push(Channel::System, &vec![0.5; 400]);
        assert_eq!(
            mixer.committable_frames(),
            0,
            "nothing should be committable while it is still inside the scrub window"
        );

        mixer.push(Channel::Mic, &vec![0.5; 200]);
        mixer.push(Channel::System, &vec![0.5; 200]);
        assert_eq!(mixer.committable_frames(), 100);
    }

    /// The whole point: audio spoken within the window can still be erased.
    #[test]
    fn scrubbing_erases_mic_audio_still_inside_the_window() {
        let mut mixer = Mixer::with_delay(1_000, 2, 500);
        mixer.push(Channel::Mic, &vec![0.9; 600]);
        mixer.push(Channel::System, &vec![0.3; 600]);

        assert_eq!(mixer.scrub_mic(), 600);

        // The 100 frames now old enough to commit carry silence on the left and the
        // untouched system audio on the right.
        let frames = mixer.take(100);
        assert!(frames.iter().step_by(2).all(|s| *s == 0.0), "mic audio survived the scrub");
        assert!(
            frames.iter().skip(1).step_by(2).all(|s| (*s - 0.3).abs() < 1e-6),
            "the scrub disturbed the system channel"
        );
    }

    /// The honest limit: audio already committed is beyond reach.
    #[test]
    fn scrubbing_cannot_reach_audio_already_taken_for_writing() {
        let mut mixer = Mixer::with_delay(1_000, 2, 500);
        mixer.push(Channel::Mic, &vec![0.9; 700]);
        mixer.push(Channel::System, &vec![0.3; 700]);

        let committed = mixer.take(200);
        assert!(committed.iter().step_by(2).any(|s| *s != 0.0));

        // Scrubbing afterwards only touches what is left.
        assert_eq!(mixer.scrub_mic(), 500);
    }

    #[test]
    fn a_mixer_with_no_delay_commits_immediately() {
        let mut mixer = Mixer::new(1_000, 1);
        mixer.push(Channel::System, &vec![0.5; 10]);
        assert_eq!(mixer.committable_frames(), 10);
    }

    #[test]
    fn pending_frames_covers_everything_still_held_for_the_final_flush() {
        let mut mixer = Mixer::with_delay(1_000, 2, 500);
        mixer.push(Channel::Mic, &vec![0.5; 300]);
        mixer.push(Channel::System, &vec![0.5; 300]);
        assert_eq!(mixer.committable_frames(), 0);
        assert_eq!(mixer.pending_frames(), 300, "the tail must still be flushable at stop");
    }

    // ---- switching mode mid-recording ----

    /// The switch drains both queues first, so the first stereo frame pairs audio
    /// captured at the same instant on both devices.
    #[test]
    fn widening_to_stereo_after_a_drain_keeps_the_channels_aligned() {
        let mut mixer = Mixer::new(1_000, 1);
        mixer.push(Channel::System, &[0.1, 0.2, 0.3]);

        let drained = mixer.drain();
        assert_eq!(drained, vec![0.1, 0.2, 0.3], "mono frames come out one per frame");
        assert_eq!(mixer.pending_frames(), 0, "nothing may straddle the switch");

        mixer.set_channels(2);
        mixer.set_mic_present(true);

        mixer.push(Channel::Mic, &[0.9]);
        mixer.push(Channel::System, &[0.4]);
        assert_eq!(mixer.take(1), vec![0.9, 0.4]);
    }

    #[test]
    fn raising_the_delay_raises_the_backlog_cap_with_it() {
        let mut mixer = Mixer::new(1_000, 1); // no delay: 1 s of slack, 1000 frames
        mixer.set_delay_frames(2_000);

        // Two seconds of audio inside a two-second window must survive: trimming it
        // would drop the very frames the scrub was just asked to protect.
        mixer.push(Channel::System, &vec![0.5; 2_000]);
        assert_eq!(mixer.dropped_frames(), 0);
        assert_eq!(mixer.committable_frames(), 0, "all of it is inside the window");
    }

    /// The failure this guards against: closing the microphone leaves its queue empty
    /// forever, and a `min` across both channels would then pin the file's length where
    /// it stood at the switch for the rest of the meeting.
    #[test]
    fn closing_the_microphone_does_not_stall_the_file() {
        let mut mixer = Mixer::new(1_000, 2);
        mixer.set_mic_present(false);

        mixer.push(Channel::System, &vec![0.5; 100]);
        assert_eq!(
            mixer.committable_frames(),
            100,
            "system audio must stay committable with no microphone feeding the other side"
        );

        let frames = mixer.take(2);
        assert_eq!(frames, vec![0.0, 0.5, 0.0, 0.5], "the left channel pads with zeros");
    }

    /// Microphone audio captured before the switch is still inside the scrub window and
    /// still aligned, so it belongs in the file rather than being discarded.
    #[test]
    fn microphone_audio_buffered_before_it_closed_is_still_written() {
        let mut mixer = Mixer::new(1_000, 2);
        mixer.push(Channel::Mic, &[0.7, 0.7]);
        mixer.push(Channel::System, &[0.1, 0.2, 0.3]);

        mixer.set_mic_present(false);

        let frames = mixer.take(3);
        assert_eq!(
            frames,
            vec![0.7, 0.1, 0.7, 0.2, 0.0, 0.3],
            "the two frames it did capture keep their place, then the channel goes quiet"
        );
    }

    /// The scrub has to keep working on audio captured while the microphone was open,
    /// which is why the delay is not dropped back to zero when it closes.
    #[test]
    fn the_scrub_still_reaches_microphone_audio_after_the_microphone_closed() {
        let mut mixer = Mixer::with_delay(1_000, 2, 500);
        mixer.push(Channel::Mic, &vec![0.9; 400]);
        mixer.push(Channel::System, &vec![0.3; 400]);

        mixer.set_mic_present(false);

        assert_eq!(mixer.scrub_mic(), 400, "held-back mic audio is still erasable");
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
