//! The microphone.
//!
//! **This module is only ever reachable from [`Mode::Conversation`].** There is no call
//! into it from the listen-only path — not a muted one, not a discarded one. That is the
//! whole guarantee behind listen-only mode, and it is enforced by there being nothing to
//! call rather than by a flag someone could get wrong.
//!
//! Constructing a [`MicSource`] opens the input device, which is what triggers the macOS
//! microphone permission prompt. Nothing here is constructed until a conversation-mode
//! recording actually starts.

use crate::source::{AudioFormat, AudioSource, Chunk};
use crate::{AudioError, Result};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::Duration;

/// Buffers held between the audio callback and the recorder.
const CHANNEL_DEPTH: usize = 128;

/// How long to wait for the input device to deliver its first buffer before calling the
/// microphone dead.
///
/// `stream.play()` returning `Ok` is not proof the device runs. CoreAudio starts the IO
/// context asynchronously, and when that fails — `StartAndWaitForState returned error
/// 35`, seen in the wild — the failure is logged inside CoreAudio and never surfaces
/// through cpal. The stream sits there delivering nothing, and in conversation mode that
/// writes a whole meeting of digital silence to the left channel.
///
/// A healthy device delivers inside 200 ms. The rest of this budget is for a first-run
/// TCC prompt still waiting to be answered, and for Bluetooth inputs that take their
/// time coming up.
const FIRST_BUFFER_TIMEOUT: Duration = Duration::from_secs(5);

/// How often to check while waiting.
const FIRST_BUFFER_POLL: Duration = Duration::from_millis(20);

/// A live microphone capture, downmixed to one channel.
///
/// The device may hand us stereo (some interfaces do); the microphone is one voice in
/// Hearsay's model, so it is folded to mono before it reaches the recorder.
pub struct MicSource {
    // Held to keep the stream alive: dropping it closes the device.
    stream: Option<cpal::Stream>,
    format: AudioFormat,
    audio: Receiver<Vec<f32>>,
    nonzero_samples: Arc<AtomicU64>,
    /// Buffers the device has handed over, silent ones included.
    ///
    /// Counted apart from `nonzero_samples` because the two answer different questions.
    /// A quiet room and a dead device both produce no non-zero samples; only this one
    /// separates them, and §6 turns on that distinction — a microphone that never came
    /// up must not read as somebody sitting quietly.
    delivered_buffers: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
}

impl MicSource {
    /// Opens the default input device.
    ///
    /// Fails loudly rather than returning a silent source: in conversation mode the user
    /// is relying on their own voice being recorded.
    pub fn start() -> Result<Self> {
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or(AudioError::NoInputDevice)?;

        let config = device
            .default_input_config()
            .map_err(|error| AudioError::InputFailed {
                message: format!("could not read the microphone's default format: {error}"),
            })?;

        let sample_rate = config.sample_rate();
        let source_channels = config.channels();
        // One channel out, whatever the device gives us.
        let format = AudioFormat::new(sample_rate, 1);

        let (audio_tx, audio_rx) = sync_channel::<Vec<f32>>(CHANNEL_DEPTH);
        let nonzero_samples = Arc::new(AtomicU64::new(0));
        let delivered_buffers = Arc::new(AtomicU64::new(0));
        let stopped = Arc::new(AtomicBool::new(false));

        let stream = build_stream(
            &device,
            &config,
            source_channels,
            audio_tx,
            Arc::clone(&nonzero_samples),
            Arc::clone(&delivered_buffers),
        )?;

        stream.play().map_err(|error| AudioError::InputFailed {
            message: format!("could not start the microphone: {error}"),
        })?;

        // `play()` returning `Ok` only means the request was accepted. Wait for the
        // device to prove it by handing over a buffer, and refuse the recording if it
        // never does — a microphone that reports success and delivers nothing is the
        // §3 failure wearing the other channel's clothes, and in conversation mode it
        // costs the user their own voice for the whole meeting.
        let waited = wait_for_first_buffer(&delivered_buffers);
        if delivered_buffers.load(Ordering::Relaxed) == 0 {
            // Dropping the stream here closes the device: a refused start never leaves
            // the microphone open.
            drop(stream);
            return Err(AudioError::InputFailed {
                message: format!(
                    "the microphone was opened but delivered no audio in {} seconds.                      macOS accepted the request and then never started the device. If                      it asked for microphone permission, allow it and start again;                      otherwise unplug and reconnect the input, or pick a different one                      in System Settings → Sound → Input.",
                    FIRST_BUFFER_TIMEOUT.as_secs()
                ),
            });
        }

        tracing::info!(
            "microphone open: {} Hz, {} channel(s) in, 1 out, first buffer after {} ms",
            sample_rate,
            source_channels,
            waited.as_millis()
        );

        Ok(Self {
            stream: Some(stream),
            format,
            audio: audio_rx,
            nonzero_samples,
            delivered_buffers,
            stopped,
        })
    }

    pub fn nonzero_samples(&self) -> u64 {
        self.nonzero_samples.load(Ordering::Relaxed)
    }

    /// Buffers the device has handed over, silent ones included. Zero while a recording
    /// runs means the device has stopped, not that the room is quiet.
    pub fn delivered_buffers(&self) -> u64 {
        self.delivered_buffers.load(Ordering::Relaxed)
    }
}

/// Blocks until the device hands over its first buffer, or the budget runs out.
///
/// Polls rather than waiting on the channel, so the caller keeps the receiver it will
/// need for the recording — taking a buffer out here to prove liveness would drop the
/// first fraction of a second of the meeting.
fn wait_for_first_buffer(delivered: &Arc<AtomicU64>) -> Duration {
    let started = std::time::Instant::now();
    while started.elapsed() < FIRST_BUFFER_TIMEOUT {
        if delivered.load(Ordering::Relaxed) > 0 {
            break;
        }
        std::thread::sleep(FIRST_BUFFER_POLL);
    }
    started.elapsed()
}

/// Builds the input stream for whichever sample format the device negotiated.
///
/// cpal hands back i16 or u32 on some devices; everything is converted to f32 here so
/// nothing downstream has to know or care.
fn build_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    source_channels: u16,
    audio: SyncSender<Vec<f32>>,
    nonzero_samples: Arc<AtomicU64>,
    delivered_buffers: Arc<AtomicU64>,
) -> Result<cpal::Stream> {
    let stream_config: cpal::StreamConfig = config.config();

    let on_error = |error| {
        // The recorder notices the source has stopped producing; log it so the reason is
        // recoverable from the log rather than being a mystery gap.
        tracing::error!("microphone stream error: {error}");
    };

    macro_rules! build {
        ($sample:ty) => {{
            let audio = audio.clone();
            let nonzero_samples = Arc::clone(&nonzero_samples);
            let delivered_buffers = Arc::clone(&delivered_buffers);
            device.build_input_stream(
                stream_config.clone(),
                move |data: &[$sample], _: &cpal::InputCallbackInfo| {
                    let samples: Vec<f32> =
                        data.iter().map(|s| cpal::Sample::to_float_sample(*s)).collect();
                    let mono = crate::mix::downmix_to_mono(&samples, source_channels);

                    // Counted before anything else, and unconditionally: this is the
                    // proof the device is alive, and it must not depend on the room
                    // making a sound.
                    delivered_buffers.fetch_add(1, Ordering::Relaxed);

                    let counted = mono.iter().filter(|s| **s != 0.0).count() as u64;
                    if counted > 0 {
                        nonzero_samples.fetch_add(counted, Ordering::Relaxed);
                    }
                    // try_send, not send: blocking here would stall the audio thread. A
                    // full channel means the recorder is wedged, and dropping is better
                    // than deadlocking the device.
                    let _ = audio.try_send(mono);
                },
                on_error,
                None,
            )
        }};
    }

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build!(f32),
        cpal::SampleFormat::I16 => build!(i16),
        cpal::SampleFormat::U16 => build!(u16),
        cpal::SampleFormat::I32 => build!(i32),
        other => {
            return Err(AudioError::InputFailed {
                message: format!("the microphone uses an unsupported sample format: {other:?}"),
            })
        }
    };

    stream.map_err(|error| AudioError::InputFailed {
        message: format!("could not open the microphone: {error}"),
    })
}

impl AudioSource for MicSource {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn next_chunk(&mut self) -> Option<Vec<f32>> {
        self.audio.recv().ok()
    }

    fn next_chunk_timeout(&mut self, timeout: Duration) -> Chunk {
        match self.audio.recv_timeout(timeout) {
            Ok(samples) => Chunk::Samples(samples),
            Err(RecvTimeoutError::Timeout) => Chunk::Idle,
            Err(RecvTimeoutError::Disconnected) => Chunk::Finished,
        }
    }

    fn stop(&mut self) -> Result<()> {
        if self.stopped.swap(true, Ordering::Relaxed) {
            return Ok(());
        }
        // Dropping the stream closes the device. This is the only place the microphone
        // is released, and it always runs — `Recording` owns the source and stops it on
        // both the normal and the panicking path.
        if let Some(stream) = self.stream.take() {
            drop(stream);
        }
        tracing::info!("microphone closed");
        Ok(())
    }

    fn has_produced_audio(&self) -> bool {
        self.nonzero_samples() > 0
    }

    fn delivered_buffers(&self) -> u64 {
        self.delivered_buffers()
    }
}

impl Drop for MicSource {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// The stream handle is not Send on some platforms, but the source is only ever moved
// between threads before the stream starts producing and after it stops. Recording owns
// it exclusively.
//
// Safety: `cpal::Stream` is `!Send` because some backends tie it to the thread that
// created it. On macOS (CoreAudio) the stream is driven by its own HAL thread and the
// handle is only used for `play`/`drop`, both of which are safe from any thread.
#[cfg(target_os = "macos")]
unsafe impl Send for MicSource {}

#[cfg(test)]
mod tests {
    use crate::Mode;

    /// The guarantee restated where the microphone lives: nothing in listen-only mode
    /// may reach this module.
    #[test]
    fn listen_only_mode_never_opens_the_microphone() {
        assert!(!Mode::ListenOnly.opens_microphone());
    }
}
