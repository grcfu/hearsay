//! Running a recording session.
//!
//! One reader thread per source feeds a shared [`Mixer`]; a writer thread drains it on a
//! wall clock and appends to the WAV. The session outlives any single command, so
//! everything the UI needs to display lives behind a mutex it can read at any time.
//!
//! The microphone guarantee is structural, not a runtime check: in [`Mode::ListenOnly`]
//! there is no branch in this file that constructs a microphone source. There is nothing
//! to disable and nothing to get wrong.

use crate::echo::{detect_bleed, split_stereo, EchoDetection};
use crate::mic::MicSource;
use crate::mix::downmix_to_mono;
use crate::mixer::{resample_mono, Channel, Mixer, SCRUB_WINDOW_SECONDS};
use crate::source::{AudioFormat, AudioSource, Chunk};
use crate::wav::WavWriter;
use crate::{AudioError, HelperSource, Mode, Result, TapTarget};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long a reader waits for audio before looping to check whether it should stop.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How often the writer commits elapsed frames to disk.
const WRITE_INTERVAL: Duration = Duration::from_millis(100);

/// Audio kept for echo analysis. Two seconds is plenty to correlate and small enough to
/// hold without thinking about it.
const ANALYSIS_SECONDS: usize = 2;

/// How soon after starting to check for echo, and how often after that.
///
/// Once early, so the user can put headphones on before the meeting gets going, then
/// once a minute — an echo can appear mid-call when someone unplugs their headphones.
const FIRST_ECHO_CHECK: Duration = Duration::from_secs(12);
const ECHO_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// How much dropped audio counts as losing audio rather than trimming clock drift.
///
/// The mixer trims a channel that runs ahead of the clock, and two devices at a nominal
/// 48 kHz drift by a few samples a minute, so a long meeting drops a little in normal
/// operation. A whole second is far beyond drift and means the writer is being starved —
/// usually by something else on the machine eating the CPU.
const DROPPED_AUDIO_ALARM_MS: u64 = 1_000;

/// What the UI reads while a recording is running.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RecordingStatus {
    pub elapsed_ms: u64,
    pub frames_written: u64,
    /// Peak level of the most recent committed buffer, for a meter.
    pub peak: f32,
    /// True once the system tap has produced a non-zero sample. If this stays false
    /// while a recording runs, the recording is silent and the user needs to know now
    /// rather than at playback.
    pub has_audio: bool,
    /// True once the microphone has produced a non-zero sample. Always false in
    /// listen-only mode, because there is no microphone.
    pub has_mic_audio: bool,
    /// The helper reported capturing zeros while audio was provably playing.
    pub silent_while_audio_playing: bool,
    pub muted: bool,
    /// Set when the other party's voice is bleeding into the microphone. Advisory: it
    /// suggests headphones and changes nothing about the recording.
    pub echo: Option<EchoDetection>,
    /// Audio captured but discarded because a channel outran the clock. Small values are
    /// normal — the mixer trims drift — so read [`Self::losing_audio`] to decide whether
    /// it matters.
    pub dropped_ms: u64,
    /// True once enough audio has been dropped to be a real gap rather than drift.
    ///
    /// Dropped audio leaves no marker in the transcript, so unlike a muted span it cannot
    /// be explained after the fact. The user has to hear about it while the recording is
    /// still running and something can be done about it.
    pub losing_audio: bool,
}

/// A completed mute span, in milliseconds from the start of the recording.
pub type MuteSpan = (i64, i64);

/// The result of a finished session.
#[derive(Debug, Clone)]
pub struct RecordingOutcome {
    pub path: PathBuf,
    pub frames: u64,
    pub duration_ms: u64,
    pub format: AudioFormat,
    /// False means every sample written was zero.
    pub produced_audio: bool,
    /// Every stretch during which the microphone was writing zeros.
    pub mute_spans: Vec<MuteSpan>,
    /// Frames captured but never written, because a channel outran the clock.
    pub dropped_frames: u64,
}

impl RecordingOutcome {
    /// Milliseconds of captured audio that never reached the file.
    pub fn dropped_ms(&self) -> u64 {
        self.format.duration_ms(self.dropped_frames)
    }
}

/// Shared control surface, readable and writable from any thread.
struct Shared {
    mixer: Mutex<Mixer>,
    status: Mutex<RecordingStatus>,
    mute_spans: Mutex<Vec<MuteSpan>>,
    /// Start of the mute span currently open, if any.
    mute_started_ms: Mutex<Option<i64>>,
    stop: AtomicBool,
    /// Samples the microphone erased before they reached disk.
    scrubbed_samples: AtomicU64,
    /// The most recently committed stereo audio, kept for echo analysis.
    analysis: Mutex<Vec<f32>>,
}

/// A running session. Dropping it stops the recording.
pub struct Recording {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
    writer: Option<JoinHandle<Result<RecordingOutcome>>>,
    path: PathBuf,
    mode: Mode,
    started: Instant,
}

impl Recording {
    /// Starts recording to `path`.
    ///
    /// Returns an error rather than a running-but-useless session if a device cannot be
    /// obtained: no permission, no such process, no helper, no microphone.
    pub fn start(mode: Mode, target: TapTarget, path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        // The only place a microphone is ever constructed. `Mode::ListenOnly` has no
        // path here — the match has no arm that opens an input device.
        //
        // Order matters, and not for a subtle reason: opening an input device *after*
        // the tap's aggregate device exists takes over four minutes on macOS, against
        // under 200 ms before it. Creating the aggregate device evidently leaves the
        // HAL in a state where opening a new input stream crawls. Microphone first, tap
        // second, and both open promptly.
        //
        // If the tap then fails, `mic` is dropped on the way out and `MicSource::drop`
        // closes the device — a failed start never leaves the microphone open.
        let mic = match mode {
            Mode::ListenOnly => None,
            Mode::Conversation => Some(MicSource::start()?),
        };

        let system = HelperSource::start(target)?;
        let sample_rate = system.format().sample_rate;

        let output_format = AudioFormat::new(sample_rate, mode.channel_count());
        let writer = WavWriter::create(&path, output_format)?;

        // Conversation mode holds a minute of both channels back so the retroactive
        // scrub has something to erase. Listen-only has no microphone and therefore
        // nothing to scrub, so it commits immediately and keeps the on-disk file current.
        let delay_frames = match mode {
            Mode::ListenOnly => 0,
            Mode::Conversation => (sample_rate as usize) * SCRUB_WINDOW_SECONDS as usize,
        };

        let shared = Arc::new(Shared {
            mixer: Mutex::new(Mixer::with_delay(
                sample_rate,
                mode.channel_count(),
                delay_frames,
            )),
            status: Mutex::new(RecordingStatus::default()),
            mute_spans: Mutex::new(Vec::new()),
            mute_started_ms: Mutex::new(None),
            stop: AtomicBool::new(false),
            scrubbed_samples: AtomicU64::new(0),
            analysis: Mutex::new(Vec::new()),
        });

        let mut workers = Vec::new();
        workers.push(spawn_system_reader(system, Arc::clone(&shared))?);
        if let Some(mic) = mic {
            workers.push(spawn_mic_reader(mic, sample_rate, Arc::clone(&shared))?);
        }

        let started = Instant::now();
        let writer_thread = spawn_writer(
            writer,
            output_format,
            Arc::clone(&shared),
            started,
            delay_frames,
        )?;

        Ok(Self {
            shared,
            workers,
            writer: Some(writer_thread),
            path,
            mode,
            started,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn status(&self) -> RecordingStatus {
        self.shared
            .status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default()
    }

    fn elapsed_ms(&self) -> i64 {
        self.started.elapsed().as_millis() as i64
    }

    /// Turns the microphone channel to zeros, or back on.
    ///
    /// Only meaningful in conversation mode. The input device is never stopped or
    /// reopened — muting writes zeros, so the timeline stays continuous and macOS never
    /// re-prompts. Returns the new state.
    pub fn set_muted(&self, muted: bool) -> Result<bool> {
        if !self.mode.opens_microphone() {
            // Nothing to mute: the microphone was never opened.
            return Ok(false);
        }

        let now = self.elapsed_ms();
        {
            let mut mixer = self.shared.mixer.lock().map_err(|_| poisoned())?;
            if mixer.is_muted() == muted {
                return Ok(muted);
            }
            mixer.set_muted(muted);
        }

        let mut open = self.shared.mute_started_ms.lock().map_err(|_| poisoned())?;
        if muted {
            *open = Some(now);
        } else if let Some(start) = open.take() {
            // Record the span the moment it closes, so a crash mid-recording still
            // leaves every completed span accounted for.
            self.shared
                .mute_spans
                .lock()
                .map_err(|_| poisoned())?
                .push((start, now));
        }

        if let Ok(mut status) = self.shared.status.lock() {
            status.muted = muted;
        }
        Ok(muted)
    }

    pub fn is_muted(&self) -> bool {
        self.shared
            .mixer
            .lock()
            .map(|mixer| mixer.is_muted())
            .unwrap_or(false)
    }

    pub fn toggle_mute(&self) -> Result<bool> {
        let next = !self.is_muted();
        self.set_muted(next)
    }

    /// Erases microphone audio that has been captured but not yet committed to disk.
    ///
    /// Returns the number of samples erased. Nothing already written to the file is
    /// touched — this covers the window between speaking and the audio reaching disk.
    pub fn scrub_microphone(&self) -> Result<usize> {
        if !self.mode.opens_microphone() {
            return Ok(0);
        }
        let erased = self
            .shared
            .mixer
            .lock()
            .map_err(|_| poisoned())?
            .scrub_mic();
        self.shared
            .scrubbed_samples
            .fetch_add(erased as u64, Ordering::Relaxed);
        tracing::info!("scrubbed {erased} microphone samples before they reached disk");
        Ok(erased)
    }

    pub fn scrubbed_samples(&self) -> u64 {
        self.shared.scrubbed_samples.load(Ordering::Relaxed)
    }

    /// Stops the recording and waits for the file to be finalised.
    pub fn stop(mut self) -> Result<RecordingOutcome> {
        // Close any mute span still open, so it is never lost by stopping while muted.
        if self.is_muted() {
            let _ = self.set_muted(false);
        }
        self.shared.stop.store(true, Ordering::Relaxed);

        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }

        let mut outcome = match self.writer.take() {
            Some(writer) => writer.join().unwrap_or_else(|_| {
                Err(AudioError::HelperFailed {
                    status: 0,
                    stderr: "the recording thread panicked; the audio written before that \
                             point is still on disk"
                        .to_string(),
                })
            })?,
            None => return Err(AudioError::HelperNoFormat),
        };

        outcome.mute_spans = self
            .shared
            .mute_spans
            .lock()
            .map(|spans| spans.clone())
            .unwrap_or_default();
        Ok(outcome)
    }
}

impl Drop for Recording {
    /// If a session is dropped without `stop`, the threads still wind down and the file
    /// is still finalised. A recording is never lost just because nobody asked politely.
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn poisoned() -> AudioError {
    AudioError::HelperFailed {
        status: 0,
        stderr: "the recorder's state was left inconsistent by an earlier panic".to_string(),
    }
}

fn spawn_system_reader(
    mut system: HelperSource,
    shared: Arc<Shared>,
) -> Result<JoinHandle<()>> {
    let channels = system.format().channels;

    std::thread::Builder::new()
        .name("hearsay-read-system".into())
        .spawn(move || {
            while !shared.stop.load(Ordering::Relaxed) {
                match system.next_chunk_timeout(POLL_INTERVAL) {
                    Chunk::Samples(samples) => {
                        // The tap delivers the machine's stereo output. That pair is one
                        // voice — "everyone else" — so it folds to a single channel
                        // before being written. This is not mixing mic and system
                        // together; those never meet.
                        let mono = downmix_to_mono(&samples, channels);
                        if let Ok(mut mixer) = shared.mixer.lock() {
                            mixer.push(Channel::System, &mono);
                        }
                        if let Ok(mut status) = shared.status.lock() {
                            if !status.has_audio && mono.iter().any(|s| *s != 0.0) {
                                status.has_audio = true;
                            }
                            status.silent_while_audio_playing = system.is_silently_failing();
                        }
                    }
                    Chunk::Idle => {
                        if let Ok(mut status) = shared.status.lock() {
                            status.silent_while_audio_playing = system.is_silently_failing();
                        }
                    }
                    Chunk::Finished => break,
                }
            }
            let _ = system.stop();
        })
        .map_err(AudioError::Io)
}

fn spawn_mic_reader(
    mut mic: MicSource,
    target_rate: u32,
    shared: Arc<Shared>,
) -> Result<JoinHandle<()>> {
    let mic_rate = mic.format().sample_rate;

    std::thread::Builder::new()
        .name("hearsay-read-mic".into())
        .spawn(move || {
            while !shared.stop.load(Ordering::Relaxed) {
                match mic.next_chunk_timeout(POLL_INTERVAL) {
                    Chunk::Samples(samples) => {
                        // Both channels must be at the output file's rate or they drift
                        // apart over the length of a meeting.
                        let aligned = resample_mono(&samples, mic_rate, target_rate);
                        let heard = aligned.iter().any(|s| *s != 0.0);

                        if let Ok(mut mixer) = shared.mixer.lock() {
                            mixer.push(Channel::Mic, &aligned);
                        }
                        if heard {
                            if let Ok(mut status) = shared.status.lock() {
                                status.has_mic_audio = true;
                            }
                        }
                    }
                    Chunk::Idle => {}
                    Chunk::Finished => break,
                }
            }
            let _ = mic.stop();
        })
        .map_err(AudioError::Io)
}

/// Commits elapsed frames to the WAV on a wall clock.
fn spawn_writer(
    mut writer: WavWriter,
    output_format: AudioFormat,
    shared: Arc<Shared>,
    started: Instant,
    delay_frames: usize,
) -> Result<JoinHandle<Result<RecordingOutcome>>> {
    let sample_rate = output_format.sample_rate.max(1) as u64;

    std::thread::Builder::new()
        .name("hearsay-write".into())
        .spawn(move || {
            let mut produced_audio = false;
            let mut next_echo_check = FIRST_ECHO_CHECK;
            let mut warned_about_drops = false;

            loop {
                let stopping = shared.stop.load(Ordering::Relaxed);

                // How many frames should exist by now, if the file matched real time —
                // minus the scrub window, which is deliberately not committed yet.
                let elapsed = started.elapsed();
                let elapsed_frames =
                    (elapsed.as_millis() as u64).saturating_mul(sample_rate) / 1000;
                let target = elapsed_frames.saturating_sub(delay_frames as u64);
                let already = writer.frames_written();
                let due = target.saturating_sub(already) as usize;

                if due > 0 {
                    // Never take more than has aged past the delay, or the scrub window
                    // would silently shrink under a slow reader.
                    let samples = match shared.mixer.lock() {
                        Ok(mut mixer) => {
                            let ready = mixer.committable_frames().min(due);
                            if ready > 0 { mixer.take(ready) } else { Vec::new() }
                        }
                        Err(_) => Vec::new(),
                    };

                    if !samples.is_empty() {
                        let peak = samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
                        if peak > 0.0 {
                            produced_audio = true;
                        }
                        writer.write_samples(&samples)?;

                        // Keep the tail for echo analysis. Only stereo has two channels
                        // to correlate, so listen-only never accumulates anything.
                        if output_format.channels >= 2 {
                            if let Ok(mut analysis) = shared.analysis.lock() {
                                analysis.extend_from_slice(&samples);
                                let cap = ANALYSIS_SECONDS * sample_rate as usize * 2;
                                if analysis.len() > cap {
                                    let excess = analysis.len() - cap;
                                    analysis.drain(..excess);
                                }
                            }
                        }

                        if let Ok(mut status) = shared.status.lock() {
                            status.frames_written = writer.frames_written();
                            status.peak = peak;
                        }
                    }
                }

                // Dropped audio is the one loss that leaves no trace in the file: the
                // frames are simply not there, and nothing downstream can tell they were
                // ever captured. Counting them is useless unless somebody is told.
                let dropped_frames = shared
                    .mixer
                    .lock()
                    .map(|mixer| mixer.dropped_frames())
                    .unwrap_or(0);
                let dropped_ms = output_format.duration_ms(dropped_frames);
                let losing_audio = dropped_ms >= DROPPED_AUDIO_ALARM_MS;

                if losing_audio && !warned_about_drops {
                    warned_about_drops = true;
                    tracing::warn!(
                        "dropped {dropped_ms} ms of captured audio — the writer is not \
                         keeping up, and the dropped stretches will be missing from the \
                         recording with no marker"
                    );
                }

                if let Ok(mut status) = shared.status.lock() {
                    status.elapsed_ms = elapsed.as_millis() as u64;
                    status.dropped_ms = dropped_ms;
                    status.losing_audio = losing_audio;
                }

                if output_format.channels >= 2 && elapsed >= next_echo_check {
                    next_echo_check = elapsed + ECHO_CHECK_INTERVAL;
                    let window = shared
                        .analysis
                        .lock()
                        .map(|analysis| analysis.clone())
                        .unwrap_or_default();

                    if !window.is_empty() {
                        let (mic, system) = split_stereo(&window);
                        let found = detect_bleed(&mic, &system, sample_rate as u32);
                        if let Some(found) = found {
                            tracing::warn!(
                                "system audio is bleeding into the microphone \
                                 ({} ms lag, correlation {:.2}) — headphones would fix it",
                                found.lag_ms,
                                found.correlation
                            );
                        }
                        if let Ok(mut status) = shared.status.lock() {
                            // Sticky: once seen, the banner stays until the recording
                            // ends. An echo that comes and goes is still an echo, and a
                            // banner that flickers is one the user learns to ignore.
                            if status.echo.is_none() {
                                status.echo = found;
                            }
                        }
                    }
                }

                if stopping {
                    break;
                }
                std::thread::sleep(WRITE_INTERVAL);
            }

            // Flush the held-back window. Recording has stopped, so there is no later
            // audio for the scrub to protect — and anything still buffered was captured
            // and not erased, so it belongs in the file.
            let remaining = shared
                .mixer
                .lock()
                .map(|mixer| mixer.pending_frames())
                .unwrap_or(0);
            if remaining > 0 {
                let samples = match shared.mixer.lock() {
                    Ok(mut mixer) => mixer.take(remaining),
                    Err(_) => Vec::new(),
                };
                if !samples.is_empty() {
                    if samples.iter().any(|s| *s != 0.0) {
                        produced_audio = true;
                    }
                    writer.write_samples(&samples)?;
                }
            }

            writer.finalize()?;

            let dropped_frames = shared
                .mixer
                .lock()
                .map(|mixer| mixer.dropped_frames())
                .unwrap_or(0);

            Ok(RecordingOutcome {
                path: writer.path().to_path_buf(),
                frames: writer.frames_written(),
                duration_ms: writer.duration_ms(),
                format: output_format,
                produced_audio,
                mute_spans: Vec::new(),
                dropped_frames,
            })
        })
        .map_err(AudioError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not a behavioural test so much as a standing assertion about the shape of the
    /// code: listen-only must not be able to reach a microphone.
    #[test]
    fn listen_only_declares_one_channel_and_no_microphone() {
        assert_eq!(Mode::ListenOnly.channel_count(), 1);
        assert!(!Mode::ListenOnly.opens_microphone());
    }

    #[test]
    fn conversation_declares_two_channels() {
        assert_eq!(Mode::Conversation.channel_count(), 2);
        assert!(Mode::Conversation.opens_microphone());
    }
}
