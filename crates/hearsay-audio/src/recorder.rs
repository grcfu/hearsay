//! Running a recording session.
//!
//! Owns a reader thread that pulls from the audio source and writes to the WAV, plus a
//! control channel the UI drives it through. The session outlives any single command, so
//! everything the UI needs to display lives behind a mutex it can read at any time.
//!
//! The microphone guarantee is structural, not a runtime check: in
//! [`Mode::ListenOnly`] there is no branch in this file that constructs a microphone
//! source. There is nothing to disable and nothing to get wrong.

use crate::mix::downmix_to_mono;
use crate::source::{AudioFormat, AudioSource, Chunk};
use crate::wav::WavWriter;
use crate::{AudioError, HelperSource, Mode, Result, TapTarget};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long the reader waits for audio before looping to check for control messages.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// What the UI reads while a recording is running.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RecordingStatus {
    pub elapsed_ms: u64,
    pub frames_written: u64,
    /// Peak level of the most recent buffer, for a meter.
    pub peak: f32,
    /// True once the source has produced at least one non-zero sample. If this stays
    /// false while a recording runs, the recording is silent and the user needs to know
    /// now rather than at playback.
    pub has_audio: bool,
    /// The helper reported capturing zeros while audio was provably playing.
    pub silent_while_audio_playing: bool,
}

/// The result of a finished session.
#[derive(Debug, Clone)]
pub struct RecordingOutcome {
    pub path: PathBuf,
    pub frames: u64,
    pub duration_ms: u64,
    pub format: AudioFormat,
    /// False means every sample written was zero.
    pub produced_audio: bool,
}

/// A running session. Dropping it stops the recording.
pub struct Recording {
    stop_flag: Arc<AtomicBool>,
    status: Arc<Mutex<RecordingStatus>>,
    worker: Option<JoinHandle<Result<RecordingOutcome>>>,
    path: PathBuf,
    mode: Mode,
}

impl Recording {
    /// Starts recording to `path`.
    ///
    /// Returns an error rather than a running-but-useless session if the tap cannot be
    /// obtained: no permission, no such process, no helper.
    pub fn start(mode: Mode, target: TapTarget, path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        // The only source constructed here. `Mode::Conversation` gains a microphone
        // alongside it; `Mode::ListenOnly` has no path to one.
        let system = HelperSource::start(target)?;
        let source_format = system.format();

        if mode.opens_microphone() {
            // Reached only once a microphone source exists to pair with the tap.
            return Err(AudioError::HelperFailed {
                status: 0,
                stderr: "conversation mode needs the microphone source, which is not \
                         wired up in this build"
                    .to_string(),
            });
        }

        // The tap hands us the machine's stereo output; that pair is one voice —
        // "everyone else" — so it is folded to a single channel before being written.
        let output_format = AudioFormat::new(source_format.sample_rate, mode.channel_count());
        let writer = WavWriter::create(&path, output_format)?;

        let stop_flag = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(RecordingStatus::default()));

        let worker = spawn_worker(
            system,
            writer,
            output_format,
            Arc::clone(&stop_flag),
            Arc::clone(&status),
        )?;

        Ok(Self {
            stop_flag,
            status,
            worker: Some(worker),
            path,
            mode,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn status(&self) -> RecordingStatus {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default()
    }

    /// Stops the recording and waits for the file to be finalised.
    pub fn stop(mut self) -> Result<RecordingOutcome> {
        self.stop_flag.store(true, Ordering::Relaxed);
        match self.worker.take() {
            Some(worker) => worker.join().unwrap_or_else(|_| {
                Err(AudioError::HelperFailed {
                    status: 0,
                    stderr: "the recording thread panicked; the audio written before that \
                             point is still on disk"
                        .to_string(),
                })
            }),
            None => Err(AudioError::HelperNoFormat),
        }
    }
}

impl Drop for Recording {
    /// If a session is dropped without `stop`, the worker still finalises the file. The
    /// recording so far is never lost just because nobody asked for it politely.
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn spawn_worker(
    mut system: HelperSource,
    mut writer: WavWriter,
    output_format: AudioFormat,
    stop_flag: Arc<AtomicBool>,
    status: Arc<Mutex<RecordingStatus>>,
) -> Result<JoinHandle<Result<RecordingOutcome>>> {
    let source_channels = system.format().channels;
    let started = Instant::now();

    std::thread::Builder::new()
        .name("hearsay-recorder".into())
        .spawn(move || {
            let mut produced_audio = false;

            while !stop_flag.load(Ordering::Relaxed) {
                match system.next_chunk_timeout(POLL_INTERVAL) {
                    Chunk::Samples(samples) => {
                        let mono = downmix_to_mono(&samples, source_channels);
                        let peak = mono.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
                        if peak > 0.0 {
                            produced_audio = true;
                        }
                        writer.write_samples(&mono)?;

                        if let Ok(mut status) = status.lock() {
                            status.elapsed_ms = started.elapsed().as_millis() as u64;
                            status.frames_written = writer.frames_written();
                            status.peak = peak;
                            status.has_audio = produced_audio;
                            status.silent_while_audio_playing = system.is_silently_failing();
                        }
                    }
                    Chunk::Idle => {
                        // An output device that has gone quiet stops producing buffers
                        // altogether. That is normal, not a failure — keep the elapsed
                        // clock moving so the UI does not look frozen.
                        if let Ok(mut status) = status.lock() {
                            status.elapsed_ms = started.elapsed().as_millis() as u64;
                            status.peak = 0.0;
                            status.silent_while_audio_playing = system.is_silently_failing();
                        }
                    }
                    Chunk::Finished => break,
                }
            }

            system.stop()?;
            writer.finalize()?;

            Ok(RecordingOutcome {
                path: writer.path().to_path_buf(),
                frames: writer.frames_written(),
                duration_ms: writer.duration_ms(),
                format: output_format,
                produced_audio,
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
}
