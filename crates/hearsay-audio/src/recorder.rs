//! Running a recording session.
//!
//! One reader thread per source feeds a shared [`Mixer`]; a writer thread drains it on a
//! wall clock and appends to the WAV. The session outlives any single command, so
//! everything the UI needs to display lives behind a mutex it can read at any time.
//!
//! The microphone guarantee is structural, not a runtime check: while a session is in
//! [`Mode::ListenOnly`] there is no branch in this file that holds a microphone. There is
//! nothing to disable and nothing to get wrong. A session can be *moved* to
//! [`Mode::Conversation`] mid-recording — see [`Recording::set_mode`] — and that is the
//! one and only place a microphone comes into existence, on a deliberate press.

use crate::echo::{detect_bleed, split_stereo, EchoDetection};
use crate::mic::MicSource;
use crate::mix::downmix_to_mono;
use crate::mixer::{resample_mono, Channel, Mixer, SCRUB_WINDOW_SECONDS};
use crate::source::{AudioFormat, AudioSource, Chunk};
use crate::wav::{promote_to_stereo, WavWriter};
use crate::{AudioError, HelperSource, Mode, Result, TapTarget};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
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
    /// True once the microphone has produced a non-zero sample. False for as long as a
    /// session has never been in conversation mode, because until then there is no
    /// microphone.
    pub has_mic_audio: bool,
    /// The helper reported capturing zeros while audio was provably playing.
    pub silent_while_audio_playing: bool,
    /// The system tap could not be restarted after a mode switch, so the recording is
    /// carrying on with the microphone alone.
    ///
    /// Switching to conversation mid-recording has to take the tap down to open the
    /// microphone promptly. Almost always it comes straight back; when it does not, the
    /// right channel is over for the rest of the session and saying nothing would leave
    /// the user recording a meeting they can no longer hear.
    pub system_audio_lost: bool,
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

/// A stretch of a recording, in milliseconds from its start.
pub type Span = (i64, i64);

/// A completed mute span, in milliseconds from the start of the recording.
pub type MuteSpan = Span;

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
    /// Every stretch during which no microphone was open at all.
    ///
    /// Empty unless the session's mode changed: a recording that stayed in one mode says
    /// which one it was, and marking the whole of a listen-only transcript as having no
    /// microphone would be noise. Once the mode has moved, though, the transcript has
    /// stretches with a mic channel and stretches without, and the reader cannot tell
    /// which is which from the audio.
    pub no_microphone_spans: Vec<Span>,
    /// Every stretch during which the system tap was down while its aggregate device was
    /// destroyed so a microphone could be opened.
    ///
    /// Under a second in normal operation, and written into the file as true silence so
    /// the timeline still matches the clock. Recorded for the same reason a muted span
    /// is: it is missing speech, and the file cannot say on its own that it was ever
    /// captured.
    pub system_gaps: Vec<Span>,
    /// Frames captured but never written, because a channel outran the clock.
    pub dropped_frames: u64,
}

impl RecordingOutcome {
    /// Milliseconds of captured audio that never reached the file.
    pub fn dropped_ms(&self) -> u64 {
        self.format.duration_ms(self.dropped_frames)
    }

    /// The mode the *file* is in, which is what transcription needs to know.
    ///
    /// Not the mode the session ended in. A recording that gained a microphone and then
    /// closed it again ends in listen-only while its file is stereo for good, and
    /// transcribing that as one mono channel would read the left channel as the whole
    /// recording — silence where the meeting was.
    pub fn layout_mode(&self) -> Mode {
        if self.format.channels >= 2 {
            Mode::Conversation
        } else {
            Mode::ListenOnly
        }
    }
}

/// Shared control surface, readable and writable from any thread.
struct Shared {
    mixer: Mutex<Mixer>,
    /// The output file.
    ///
    /// Behind a mutex rather than owned by the writer thread because a mode switch has to
    /// close it, rewrite it, and reopen it at a new channel count. Taking this lock is
    /// what parks the writer for the duration: it blocks on the same lock and comes back
    /// to the file it is about to append to. `None` only between those two moments.
    ///
    /// Lock order is **this before the mixer**, everywhere, without exception.
    writer: Mutex<Option<WavWriter>>,
    status: Mutex<RecordingStatus>,
    mute_spans: Mutex<Vec<MuteSpan>>,
    /// Start of the mute span currently open, if any.
    mute_started_ms: Mutex<Option<i64>>,
    no_microphone_spans: Mutex<Vec<Span>>,
    /// When the current stretch without a microphone began, if there is one.
    mic_closed_since: Mutex<Option<i64>>,
    system_gaps: Mutex<Vec<Span>>,
    stop: AtomicBool,
    /// Set by a mode switch to bring the next echo check forward. A user who has just
    /// opened their microphone is exactly who the headphones banner is for.
    recheck_echo: AtomicBool,
    /// Samples the microphone erased before they reached disk.
    scrubbed_samples: AtomicU64,
    /// The most recently committed stereo audio, kept for echo analysis.
    analysis: Mutex<Vec<f32>>,
}

/// One reader thread and the flag that retires it.
///
/// Retiring one source without ending the recording is what a mode switch does, so a
/// reader needs a stop signal of its own rather than sharing the session's.
struct Reader {
    handle: JoinHandle<()>,
    retire: Arc<AtomicBool>,
}

impl Reader {
    /// Winds the thread down and waits for it.
    ///
    /// The thread stops its source on the way out, which for the tap means the helper
    /// destroys the tap and the aggregate device — and `HelperSource::stop` waits for the
    /// process to exit, so once this returns those are provably gone.
    fn retire(self) {
        self.retire.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
    }
}

/// A running session. Dropping it stops the recording.
pub struct Recording {
    shared: Arc<Shared>,
    system: Option<Reader>,
    mic: Option<Reader>,
    writer: Option<JoinHandle<Result<RecordingOutcome>>>,
    path: PathBuf,
    /// What to tap, kept so the tap can be started again after a mode switch has had to
    /// take it down.
    target: TapTarget,
    /// The rate of the output file. Every source is resampled to it, including a tap that
    /// comes back from a restart having negotiated something different.
    sample_rate: u32,
    mode: Mutex<Mode>,
    /// Whether the mode has ever changed. Decides whether the stretches with and without
    /// a microphone are worth writing down at all.
    switched: AtomicBool,
    started: Instant,
}

impl Recording {
    /// Starts recording to `path`.
    ///
    /// Returns an error rather than a running-but-useless session if a device cannot be
    /// obtained: no permission, no such process, no helper, no microphone.
    pub fn start(mode: Mode, target: TapTarget, path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        // The only place a microphone is constructed at start. `Mode::ListenOnly` has no
        // path here — the match has no arm that opens an input device.
        //
        // Order matters, and not for a subtle reason: opening an input device *after*
        // the tap's aggregate device exists takes over four minutes on macOS, against
        // under 200 ms before it. Creating the aggregate device evidently leaves the
        // HAL in a state where opening a new input stream crawls. Microphone first, tap
        // second, and both open promptly.
        //
        // It is also the reason switching mode mid-recording costs a gap in the system
        // channel: there is no way to open a microphone later without taking the
        // aggregate device down first. See [`Recording::upgrade_to_conversation`].
        //
        // If the tap then fails, `mic` is dropped on the way out and `MicSource::drop`
        // closes the device — a failed start never leaves the microphone open.
        let mic = match mode {
            Mode::ListenOnly => None,
            Mode::Conversation => Some(MicSource::start()?),
        };

        let system = HelperSource::start(target.clone())?;
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
            writer: Mutex::new(Some(writer)),
            status: Mutex::new(RecordingStatus::default()),
            mute_spans: Mutex::new(Vec::new()),
            mute_started_ms: Mutex::new(None),
            no_microphone_spans: Mutex::new(Vec::new()),
            // Listen-only starts a stretch with no microphone at the very first sample.
            // It is only ever written down if the mode later changes.
            mic_closed_since: Mutex::new(match mode {
                Mode::ListenOnly => Some(0),
                Mode::Conversation => None,
            }),
            system_gaps: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            recheck_echo: AtomicBool::new(false),
            scrubbed_samples: AtomicU64::new(0),
            analysis: Mutex::new(Vec::new()),
        });

        let system_reader = spawn_system_reader(system, sample_rate, Arc::clone(&shared))?;
        let mic_reader = match mic {
            Some(mic) => Some(spawn_mic_reader(mic, sample_rate, Arc::clone(&shared))?),
            None => None,
        };

        let started = Instant::now();
        let writer_thread = spawn_writer(Arc::clone(&shared), started, sample_rate)?;

        Ok(Self {
            shared,
            system: Some(system_reader),
            mic: mic_reader,
            writer: Some(writer_thread),
            path,
            target,
            sample_rate,
            mode: Mutex::new(mode),
            switched: AtomicBool::new(false),
            started,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The mode the session is in *now*, which a switch can have changed.
    pub fn mode(&self) -> Mode {
        self.mode.lock().map(|mode| *mode).unwrap_or(Mode::ListenOnly)
    }

    /// Whether the file has a microphone channel, whatever mode the session is in now.
    /// Once true, always true: frames already written cannot be narrowed.
    pub fn has_mic_channel(&self) -> bool {
        self.shared
            .mixer
            .lock()
            .map(|mixer| mixer.channels() > 1)
            .unwrap_or(false)
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

    // ---- switching mode mid-recording -------------------------------------------

    /// Moves a running recording to `mode`.
    ///
    /// Both directions are a deliberate press and neither is silent about what it costs.
    /// Going up opens a microphone that was never open, which is a change to what the
    /// recording can hear and is written into the transcript as one; going down closes it
    /// outright rather than muting it, so §4's guarantee applies for the rest of the
    /// session.
    pub fn set_mode(&mut self, mode: Mode) -> Result<Mode> {
        match mode {
            Mode::Conversation => self.upgrade_to_conversation(),
            Mode::ListenOnly => self.downgrade_to_listen_only(),
        }
    }

    /// Opens the microphone on a recording that has been listening only.
    ///
    /// This costs a sub-second gap in the system channel and cannot avoid it. The tap's
    /// aggregate device has to be destroyed before an input device will open in
    /// milliseconds rather than minutes, so for as long as that takes there is no tap.
    /// The gap is padded into the file as true silence and recorded as a span, because a
    /// stretch of missing speech that the file cannot account for is the one loss with
    /// nothing to show for it.
    ///
    /// The recording survives either device failing. If the microphone will not open, the
    /// tap goes back and the session stays in listen-only — which is what the user still
    /// believes it is. If the tap will not come back, the session carries on with the
    /// microphone alone and says so through [`RecordingStatus::system_audio_lost`].
    fn upgrade_to_conversation(&mut self) -> Result<Mode> {
        if self.mode() == Mode::Conversation {
            return Ok(Mode::Conversation);
        }
        let at_ms = self.elapsed_ms();

        // Held for the whole switch. The writer thread blocks on it and resumes against
        // the file this leaves behind. Taken through a handle of its own so the guard
        // does not borrow `self` for as long as it lives — the switch has readers to
        // swap out, and those need `&mut self`.
        let shared = Arc::clone(&self.shared);
        let mut slot = shared.writer.lock().map_err(|_| poisoned())?;

        // Everything captured so far belongs in the file as it stands, and nothing may
        // be left buffered across the boundary: a backlog on either side at the moment
        // the output widens would offset the two channels for the rest of the recording.
        let tail = self.shared.mixer.lock().map_err(|_| poisoned())?.drain();
        {
            let writer = slot.as_mut().ok_or_else(poisoned)?;
            writer.write_samples(&tail)?;
            // A valid, complete mono wav on disk before anything touches it, so an
            // interruption during the switch costs nothing already captured.
            writer.finalize()?;
        }
        // Close the descriptor: the rewrite renames a new file over this path.
        drop(slot.take());

        // The tap goes down first. This is the whole cost of the feature — see the note
        // in `Recording::start` about what happens to an input device opened while an
        // aggregate device exists.
        if let Some(system) = self.system.take() {
            system.retire();
        }

        let mic = match MicSource::start() {
            Ok(mic) => mic,
            Err(error) => {
                // Nothing has changed except that the tap is down. Put it back and leave
                // the recording where the user thinks it is.
                self.restore_tap(&mut slot, false, at_ms)?;
                return Err(error);
            }
        };

        let tap = HelperSource::start(self.target.clone());

        // The file becomes stereo whether or not the tap came back: the microphone is
        // open, so from here there are two channels to keep apart.
        self.resume_file(&mut slot, true)?;

        {
            let mut mixer = self.shared.mixer.lock().map_err(|_| poisoned())?;
            // Both queues were drained above, so the first stereo frame pairs microphone
            // and system audio captured at the same instant.
            mixer.set_channels(2);
            mixer.set_mic_present(true);
            // There is something to scrub from now on, so start holding audio back for
            // it. §6 is not optional in conversation mode.
            mixer.set_delay_frames(self.sample_rate as usize * SCRUB_WINDOW_SECONDS as usize);
        }

        // The window held mono frames. Correlating those against a stereo split would
        // compare the recording with itself.
        if let Ok(mut analysis) = self.shared.analysis.lock() {
            analysis.clear();
        }
        self.shared.recheck_echo.store(true, Ordering::Relaxed);

        self.mic = Some(spawn_mic_reader(
            mic,
            self.sample_rate,
            Arc::clone(&self.shared),
        )?);

        let lost_system_audio = match tap {
            Ok(tap) => {
                self.system = Some(spawn_system_reader(
                    tap,
                    self.sample_rate,
                    Arc::clone(&self.shared),
                )?);
                false
            }
            Err(error) => {
                // Not fatal, and not quiet either. The meeting is still being recorded
                // from the microphone; the other party is not.
                tracing::error!(
                    "the system audio tap did not come back after switching to \
                     conversation mode: {error}. The microphone is recording; system \
                     audio is not."
                );
                true
            }
        };

        self.close_no_microphone_span(at_ms)?;
        self.record_gap(at_ms)?;
        *self.mode.lock().map_err(|_| poisoned())? = Mode::Conversation;
        self.switched.store(true, Ordering::Relaxed);

        if let Ok(mut status) = self.shared.status.lock() {
            status.system_audio_lost = lost_system_audio;
        }

        tracing::info!(
            "switched to conversation mode {} ms in; the microphone is open and the file \
             is stereo from here",
            at_ms
        );
        Ok(Mode::Conversation)
    }

    /// Closes the microphone on a running conversation recording.
    ///
    /// Not the same as muting. Mute leaves the input device open and writes zeros; this
    /// releases the device, so for the rest of the session §4's guarantee holds and it is
    /// physically impossible for room audio to reach disk.
    ///
    /// Costs nothing: the tap is untouched, and the file stays stereo because the frames
    /// already written have two channels and cannot be narrowed. The left channel goes to
    /// true zeros once the microphone's buffered audio runs out.
    fn downgrade_to_listen_only(&mut self) -> Result<Mode> {
        if self.mode() == Mode::ListenOnly {
            return Ok(Mode::ListenOnly);
        }
        let at_ms = self.elapsed_ms();

        // Close any open mute span while there is still a microphone to have muted. Left
        // open, it would be lost.
        if self.is_muted() {
            let _ = self.set_muted(false);
        }

        if let Some(mic) = self.mic.take() {
            mic.retire();
        }

        // The commit delay deliberately stays where it is. What the microphone captured
        // in the last minute is still inside the scrub window and still erasable, and
        // dropping the delay to zero would commit all of it to disk on the next tick —
        // taking the scrub away at the exact moment someone is most likely to want it.
        self.shared
            .mixer
            .lock()
            .map_err(|_| poisoned())?
            .set_mic_present(false);

        *self.shared.mic_closed_since.lock().map_err(|_| poisoned())? = Some(at_ms);
        *self.mode.lock().map_err(|_| poisoned())? = Mode::ListenOnly;
        self.switched.store(true, Ordering::Relaxed);

        if let Ok(mut status) = self.shared.status.lock() {
            status.muted = false;
            // Nothing left for the other party's voice to bleed into.
            status.echo = None;
        }

        tracing::info!(
            "switched to listen-only mode {at_ms} ms in; the microphone is closed and \
             the file's left channel is silent from here"
        );
        Ok(Mode::ListenOnly)
    }

    /// Brings the tap back after a switch that could not be completed, leaving the
    /// recording as it was.
    fn restore_tap(
        &mut self,
        slot: &mut MutexGuard<'_, Option<WavWriter>>,
        stereo: bool,
        at_ms: i64,
    ) -> Result<()> {
        self.resume_file(slot, stereo)?;
        match HelperSource::start(self.target.clone()) {
            Ok(tap) => {
                self.system = Some(spawn_system_reader(
                    tap,
                    self.sample_rate,
                    Arc::clone(&self.shared),
                )?);
            }
            Err(error) => {
                tracing::error!(
                    "the system audio tap did not come back after an abandoned mode \
                     switch: {error}"
                );
                if let Ok(mut status) = self.shared.status.lock() {
                    status.system_audio_lost = true;
                }
            }
        }
        self.record_gap(at_ms)
    }

    /// Reopens the output file after a switch, padding the stretch during which nothing
    /// was being captured so that the file's length still matches the clock.
    ///
    /// The padding is the difference between how long the recording has been running and
    /// how much audio is in the file, so it covers both the deliberate gap and the last
    /// fraction of a second the readers had not delivered when the drain happened. Without
    /// it, the audio captured after the switch would be written where the gap belongs and
    /// every timestamp for the rest of the recording would sit early by its length.
    fn resume_file(
        &self,
        slot: &mut MutexGuard<'_, Option<WavWriter>>,
        stereo: bool,
    ) -> Result<()> {
        if stereo {
            promote_to_stereo(&self.path)?;
        }
        let mut writer = WavWriter::append(&self.path)?;

        let elapsed = self.elapsed_ms().max(0) as u64;
        let should_hold = elapsed * self.sample_rate as u64 / 1000;
        let missing = should_hold.saturating_sub(writer.frames_written());
        writer.write_silence(missing)?;
        writer.sync_header()?;

        **slot = Some(writer);
        Ok(())
    }

    /// Writes down the stretch the tap was down for, if it was down for any measurable
    /// time.
    fn record_gap(&self, from_ms: i64) -> Result<()> {
        let to_ms = self.elapsed_ms();
        if to_ms <= from_ms {
            return Ok(());
        }
        self.shared
            .system_gaps
            .lock()
            .map_err(|_| poisoned())?
            .push((from_ms, to_ms));
        tracing::warn!(
            "system audio was not captured between {from_ms} ms and {to_ms} ms while the \
             microphone was opened; the file carries silence there"
        );
        Ok(())
    }

    /// Ends the open stretch without a microphone, now that there is one.
    fn close_no_microphone_span(&self, at_ms: i64) -> Result<()> {
        if let Some(start) = self
            .shared
            .mic_closed_since
            .lock()
            .map_err(|_| poisoned())?
            .take()
        {
            self.shared
                .no_microphone_spans
                .lock()
                .map_err(|_| poisoned())?
                .push((start, at_ms));
        }
        Ok(())
    }

    // ---- mute and scrub ---------------------------------------------------------

    /// Turns the microphone channel to zeros, or back on.
    ///
    /// Only meaningful while a microphone is open. The input device is never stopped or
    /// reopened — muting writes zeros, so the timeline stays continuous and macOS never
    /// re-prompts. Returns the new state.
    pub fn set_muted(&self, muted: bool) -> Result<bool> {
        if !self.mode().opens_microphone() {
            // Nothing to mute: no microphone is open.
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
    ///
    /// Gated on the file having a microphone channel rather than on the current mode: a
    /// recording that has just been switched *out* of conversation mode still holds up to
    /// a minute of microphone audio that has not reached disk, and that is exactly the
    /// audio someone closing their microphone is most likely to want gone.
    pub fn scrub_microphone(&self) -> Result<usize> {
        let erased = {
            let mut mixer = self.shared.mixer.lock().map_err(|_| poisoned())?;
            if mixer.channels() <= 1 {
                return Ok(0);
            }
            mixer.scrub_mic()
        };
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
        let ended_ms = self.elapsed_ms();
        self.shared.stop.store(true, Ordering::Relaxed);

        if let Some(system) = self.system.take() {
            system.retire();
        }
        if let Some(mic) = self.mic.take() {
            mic.retire();
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

        // Only worth writing down if the mode moved. A recording that stayed in one mode
        // already says which, and marking the whole of a listen-only transcript as
        // having no microphone would tell the reader nothing they cannot see.
        if self.switched.load(Ordering::Relaxed) {
            let _ = self.close_no_microphone_span(ended_ms);
            outcome.no_microphone_spans = self
                .shared
                .no_microphone_spans
                .lock()
                .map(|spans| spans.clone())
                .unwrap_or_default();
            outcome.system_gaps = self
                .shared
                .system_gaps
                .lock()
                .map(|spans| spans.clone())
                .unwrap_or_default();
        }
        Ok(outcome)
    }
}

impl Drop for Recording {
    /// If a session is dropped without `stop`, the threads still wind down and the file
    /// is still finalised. A recording is never lost just because nobody asked politely.
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(system) = self.system.take() {
            system.retire();
        }
        if let Some(mic) = self.mic.take() {
            mic.retire();
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
    target_rate: u32,
    shared: Arc<Shared>,
) -> Result<Reader> {
    let channels = system.format().channels;
    let tap_rate = system.format().sample_rate;
    let retire = Arc::new(AtomicBool::new(false));
    let retire_flag = Arc::clone(&retire);

    let handle = std::thread::Builder::new()
        .name("hearsay-read-system".into())
        .spawn(move || {
            while !shared.stop.load(Ordering::Relaxed) && !retire_flag.load(Ordering::Relaxed) {
                match system.next_chunk_timeout(POLL_INTERVAL) {
                    Chunk::Samples(samples) => {
                        // The tap delivers the machine's stereo output. That pair is one
                        // voice — "everyone else" — so it folds to a single channel
                        // before being written. This is not mixing mic and system
                        // together; those never meet.
                        let mono = downmix_to_mono(&samples, channels);
                        // A tap restarted by a mode switch can come back at a different
                        // rate from the one the file was opened at.
                        let aligned = resample_mono(&mono, tap_rate, target_rate);
                        if let Ok(mut mixer) = shared.mixer.lock() {
                            mixer.push(Channel::System, &aligned);
                        }
                        if let Ok(mut status) = shared.status.lock() {
                            if !status.has_audio && aligned.iter().any(|s| *s != 0.0) {
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
        .map_err(AudioError::Io)?;

    Ok(Reader { handle, retire })
}

fn spawn_mic_reader(mut mic: MicSource, target_rate: u32, shared: Arc<Shared>) -> Result<Reader> {
    let mic_rate = mic.format().sample_rate;
    let retire = Arc::new(AtomicBool::new(false));
    let retire_flag = Arc::clone(&retire);

    let handle = std::thread::Builder::new()
        .name("hearsay-read-mic".into())
        .spawn(move || {
            while !shared.stop.load(Ordering::Relaxed) && !retire_flag.load(Ordering::Relaxed) {
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
        .map_err(AudioError::Io)?;

    Ok(Reader { handle, retire })
}

/// Commits elapsed frames to the WAV on a wall clock.
fn spawn_writer(
    shared: Arc<Shared>,
    started: Instant,
    sample_rate: u32,
) -> Result<JoinHandle<Result<RecordingOutcome>>> {
    let rate = sample_rate.max(1) as u64;

    std::thread::Builder::new()
        .name("hearsay-write".into())
        .spawn(move || {
            let mut produced_audio = false;
            let mut next_echo_check = FIRST_ECHO_CHECK;
            let mut warned_about_drops = false;

            loop {
                let stopping = shared.stop.load(Ordering::Relaxed);
                let elapsed = started.elapsed();

                // The file lock is held for the whole tick, and the mixer is taken inside
                // it. A mode switch takes the two in the same order, which is what stops
                // one landing between reading the file's position and appending to it.
                {
                    let mut slot = shared.writer.lock().map_err(|_| poisoned())?;
                    if let Some(writer) = slot.as_mut() {
                        let format = writer.format();

                        // How many frames should exist by now, if the file matched real
                        // time — minus the commit delay, which is deliberately not
                        // committed yet. Read from the mixer rather than captured,
                        // because a switch to conversation raises it from zero.
                        let delay_frames = shared
                            .mixer
                            .lock()
                            .map(|mixer| mixer.delay_frames())
                            .unwrap_or(0);
                        let elapsed_frames =
                            (elapsed.as_millis() as u64).saturating_mul(rate) / 1000;
                        let target = elapsed_frames.saturating_sub(delay_frames as u64);
                        let due = target.saturating_sub(writer.frames_written()) as usize;

                        if due > 0 {
                            // Never take more than has aged past the delay, or the scrub
                            // window would silently shrink under a slow reader.
                            let samples = match shared.mixer.lock() {
                                Ok(mut mixer) => {
                                    let ready = mixer.committable_frames().min(due);
                                    if ready > 0 { mixer.take(ready) } else { Vec::new() }
                                }
                                Err(_) => Vec::new(),
                            };

                            if !samples.is_empty() {
                                let peak =
                                    samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
                                if peak > 0.0 {
                                    produced_audio = true;
                                }
                                writer.write_samples(&samples)?;

                                // Keep the tail for echo analysis. Only stereo has two
                                // channels to correlate, so a listen-only stretch never
                                // accumulates anything.
                                if format.channels >= 2 {
                                    if let Ok(mut analysis) = shared.analysis.lock() {
                                        analysis.extend_from_slice(&samples);
                                        let cap = ANALYSIS_SECONDS * rate as usize * 2;
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
                    }
                }

                // Dropped audio is the one loss that leaves no trace in the file: the
                // frames are simply not there, and nothing downstream can tell they were
                // ever captured. Counting them is useless unless somebody is told.
                let (dropped_frames, stereo) = shared
                    .mixer
                    .lock()
                    .map(|mixer| (mixer.dropped_frames(), mixer.channels() >= 2))
                    .unwrap_or((0, false));
                // `rate` is clamped to at least 1 where it is bound, so this cannot divide by zero.
                let dropped_ms = dropped_frames * 1000 / rate;
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

                // A switch to conversation asks for the check to come round again soon:
                // someone who has just opened their microphone is exactly who the
                // headphones banner is for.
                if shared.recheck_echo.swap(false, Ordering::Relaxed) {
                    next_echo_check = elapsed + FIRST_ECHO_CHECK;
                }

                if stereo && elapsed >= next_echo_check {
                    next_echo_check = elapsed + ECHO_CHECK_INTERVAL;
                    let window = shared
                        .analysis
                        .lock()
                        .map(|analysis| analysis.clone())
                        .unwrap_or_default();

                    if !window.is_empty() {
                        let (mic, system) = split_stereo(&window);
                        let found = detect_bleed(&mic, &system, rate as u32);
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
            let mut slot = shared.writer.lock().map_err(|_| poisoned())?;
            let mut writer = slot.take().ok_or_else(poisoned)?;

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
                // From the file, not from how the recording started: a session that
                // gained a microphone is stereo from that point on.
                format: writer.format(),
                produced_audio,
                mute_spans: Vec::new(),
                no_microphone_spans: Vec::new(),
                system_gaps: Vec::new(),
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

    fn outcome_with(channels: u16) -> RecordingOutcome {
        RecordingOutcome {
            path: PathBuf::from("/tmp/example.wav"),
            frames: 48_000,
            duration_ms: 1_000,
            format: AudioFormat::new(48_000, channels),
            produced_audio: true,
            mute_spans: Vec::new(),
            no_microphone_spans: Vec::new(),
            system_gaps: Vec::new(),
            dropped_frames: 0,
        }
    }

    /// The trap this guards: a recording that gained a microphone and then closed it
    /// again ends in listen-only while its file is stereo for good. Transcribing that as
    /// one mono channel reads the left channel as the whole recording, which is silence
    /// where the meeting was.
    #[test]
    fn the_file_decides_how_a_recording_is_transcribed() {
        assert_eq!(outcome_with(2).layout_mode(), Mode::Conversation);
        assert_eq!(outcome_with(1).layout_mode(), Mode::ListenOnly);
    }
}
