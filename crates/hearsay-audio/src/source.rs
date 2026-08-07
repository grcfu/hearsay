//! The boundary every audio producer sits behind.
//!
//! Two things implement [`AudioSource`]: the system-audio tap (a subprocess reading from
//! a Core Audio process tap) and the microphone. Keeping them behind one trait is what
//! lets the recorder treat "one channel" and "two channels" as the same problem, and
//! what makes it checkable that `listen_only` never constructs the microphone at all.

use crate::Result;

/// The PCM format a source produces. Always float32, always interleaved.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioFormat {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate,
            channels,
        }
    }

    /// Number of frames in a buffer of `samples` interleaved samples.
    pub fn frames(&self, samples: usize) -> usize {
        if self.channels == 0 {
            0
        } else {
            samples / self.channels as usize
        }
    }

    /// Milliseconds of audio represented by `frames` frames.
    pub fn duration_ms(&self, frames: u64) -> u64 {
        if self.sample_rate == 0 {
            0
        } else {
            frames * 1000 / self.sample_rate as u64
        }
    }
}

/// The outcome of asking a source for audio with a deadline.
///
/// `Idle` is a first-class answer, not a failure. A tap on an output device that has
/// gone quiet delivers nothing at all — macOS stops running the IO callback rather than
/// feeding zeros — so "no audio yet" and "finished" must stay distinguishable.
#[derive(Debug)]
pub enum Chunk {
    Samples(Vec<f32>),
    Idle,
    Finished,
}

/// A live source of interleaved float32 audio.
///
/// Implementations are expected to be driven from a dedicated reader thread — nothing
/// here is async.
pub trait AudioSource: Send {
    /// The negotiated format. Known before the first chunk arrives.
    fn format(&self) -> AudioFormat;

    /// Blocks for the next buffer of interleaved samples. `None` means the source is
    /// finished and will produce nothing further.
    ///
    /// This can block indefinitely on a device that has gone idle. Prefer
    /// [`AudioSource::next_chunk_timeout`] anywhere that also has periodic work to do.
    fn next_chunk(&mut self) -> Option<Vec<f32>>;

    /// Like [`AudioSource::next_chunk`], but gives up after `timeout` and says so.
    fn next_chunk_timeout(&mut self, timeout: std::time::Duration) -> Chunk;

    /// Stops the source and releases its resources. Idempotent.
    fn stop(&mut self) -> Result<()>;

    /// Whether this source has produced any non-zero sample.
    ///
    /// A tap that runs without permission delivers buffers at exactly the right rate,
    /// all of them zero. Every source reports this so a recording can never end without
    /// someone having asked the question.
    fn has_produced_audio(&self) -> bool;
}
