//! Writing recordings to disk.
//!
//! A WAV file states its own length in a header written before any audio exists. The
//! usual approach — keep the sizes in memory and patch them when the file is closed —
//! produces a file that is unplayable if the process never gets to close it. A recording
//! that ends in a crash, a force-quit, or a laptop lid closing at the wrong moment is
//! exactly the recording the user most wants back.
//!
//! So the header is rewritten in place as the recording runs. At any instant the file on
//! disk is a valid WAV of everything committed so far, and [`repair`] can rebuild the
//! sizes from the file's own length if even that was interrupted mid-update.
//!
//! Samples arrive as float32 and are written as 16-bit PCM: half the size, and playable
//! by every audio element and editor without qualification.

use crate::source::AudioFormat;
use crate::Result;

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Bytes of header before the first sample.
const HEADER_LEN: u64 = 44;
/// Byte offset of the RIFF chunk size field.
const RIFF_SIZE_OFFSET: u64 = 4;
/// Byte offset of the data chunk size field.
const DATA_SIZE_OFFSET: u64 = 40;
/// Output is always 16-bit PCM regardless of what the source produced.
const BITS_PER_SAMPLE: u16 = 16;

/// How much audio may accumulate before the header is brought up to date. One second is
/// short enough that an abrupt end costs almost nothing and long enough that the extra
/// seeks are irrelevant next to the audio itself.
const HEADER_SYNC_INTERVAL_FRAMES: u64 = 48_000;

/// Streams interleaved float32 samples to a 16-bit PCM WAV file.
pub struct WavWriter {
    path: PathBuf,
    inner: BufWriter<File>,
    format: AudioFormat,
    frames_written: u64,
    frames_since_sync: u64,
    finalized: bool,
}

impl WavWriter {
    /// Creates the file and writes a header describing an empty recording.
    pub fn create(path: impl AsRef<Path>, format: AudioFormat) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = File::create(&path)?;
        let mut writer = Self {
            path,
            inner: BufWriter::new(file),
            format,
            frames_written: 0,
            frames_since_sync: 0,
            finalized: false,
        };
        writer.write_header(0)?;
        writer.inner.flush()?;
        Ok(writer)
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    pub fn frames_written(&self) -> u64 {
        self.frames_written
    }

    /// Milliseconds of audio committed so far.
    pub fn duration_ms(&self) -> u64 {
        self.format.duration_ms(self.frames_written)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends interleaved samples. The slice length should be a whole number of frames;
    /// a trailing partial frame is written as-is rather than dropped, since dropping it
    /// would shift every later sample across the channels.
    pub fn write_samples(&mut self, samples: &[f32]) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }

        // Convert in a scratch buffer so the file sees one write per call.
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            bytes.extend_from_slice(&to_i16(*sample).to_le_bytes());
        }
        self.inner.write_all(&bytes)?;

        let frames = self.format.frames(samples.len()) as u64;
        self.frames_written += frames;
        self.frames_since_sync += frames;

        if self.frames_since_sync >= HEADER_SYNC_INTERVAL_FRAMES {
            self.sync_header()?;
        }
        Ok(())
    }

    /// Appends `frames` frames of digital silence across every channel.
    ///
    /// Used to keep the two channels aligned when one of them is delayed — see the mic
    /// ring buffer in `CLAUDE.md` §6.
    pub fn write_silence(&mut self, frames: u64) -> Result<()> {
        if frames == 0 {
            return Ok(());
        }
        let samples = frames as usize * self.format.channels.max(1) as usize;
        let zeros = vec![0u8; samples * 2];
        self.inner.write_all(&zeros)?;
        self.frames_written += frames;
        self.frames_since_sync += frames;
        if self.frames_since_sync >= HEADER_SYNC_INTERVAL_FRAMES {
            self.sync_header()?;
        }
        Ok(())
    }

    /// Flushes buffered audio and rewrites the header so the file on disk is a valid,
    /// complete WAV of everything written so far.
    pub fn sync_header(&mut self) -> Result<()> {
        self.inner.flush()?;
        let data_len = self.frames_written * self.bytes_per_frame();

        let file = self.inner.get_mut();
        file.seek(SeekFrom::Start(RIFF_SIZE_OFFSET))?;
        file.write_all(&((HEADER_LEN - 8 + data_len) as u32).to_le_bytes())?;
        file.seek(SeekFrom::Start(DATA_SIZE_OFFSET))?;
        file.write_all(&(data_len as u32).to_le_bytes())?;
        file.seek(SeekFrom::End(0))?;

        self.frames_since_sync = 0;
        Ok(())
    }

    /// Final flush. Idempotent, and safe to call from a shutdown path.
    pub fn finalize(&mut self) -> Result<()> {
        if self.finalized {
            return Ok(());
        }
        self.sync_header()?;
        self.inner.flush()?;
        self.inner.get_mut().sync_all()?;
        self.finalized = true;
        Ok(())
    }

    fn bytes_per_frame(&self) -> u64 {
        self.format.channels.max(1) as u64 * (BITS_PER_SAMPLE as u64 / 8)
    }

    fn write_header(&mut self, data_len: u32) -> Result<()> {
        self.inner.write_all(&header_bytes(self.format, data_len))?;
        Ok(())
    }
}

/// The 44 bytes that precede the samples, for a file of `data_len` audio bytes.
fn header_bytes(format: AudioFormat, data_len: u32) -> Vec<u8> {
    let channels = format.channels.max(1);
    let block_align = channels * (BITS_PER_SAMPLE / 8);
    let byte_rate = format.sample_rate * u32::from(block_align);

    let mut header = Vec::with_capacity(HEADER_LEN as usize);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(HEADER_LEN as u32 - 8 + data_len).to_le_bytes());
    header.extend_from_slice(b"WAVE");
    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk length
    header.extend_from_slice(&1u16.to_le_bytes()); // format tag: PCM
    header.extend_from_slice(&channels.to_le_bytes());
    header.extend_from_slice(&format.sample_rate.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_len.to_le_bytes());
    header
}

/// Reads the sample rate and channel count out of a file this module wrote.
///
/// `None` means the file is shorter than a header, so nothing was ever written to it.
fn read_format(file: &mut File) -> Result<Option<AudioFormat>> {
    use std::io::Read;

    if file.metadata()?.len() < HEADER_LEN {
        return Ok(None);
    }

    // Channel count and sample rate live at fixed offsets in the fmt chunk we wrote.
    let mut fmt = [0u8; 6];
    file.seek(SeekFrom::Start(22))?;
    file.read_exact(&mut fmt)?;

    Ok(Some(AudioFormat::new(
        u32::from_le_bytes([fmt[2], fmt[3], fmt[4], fmt[5]]),
        u16::from_le_bytes([fmt[0], fmt[1]]).max(1),
    )))
}

impl Drop for WavWriter {
    /// Best-effort finalisation. The error is swallowed because a destructor has nowhere
    /// to report it — call [`WavWriter::finalize`] explicitly to find out whether the
    /// last flush succeeded.
    fn drop(&mut self) {
        if !self.finalized {
            if let Err(error) = self.finalize() {
                tracing::error!("could not finalise {}: {error}", self.path.display());
            }
        }
    }
}

/// What a repaired file turned out to contain.
///
/// The format comes back alongside the frame count because the only reason to repair a
/// file is to find out how long it is, and frames alone cannot answer that.
#[derive(Debug, Clone, Copy)]
pub struct Repaired {
    pub frames: u64,
    pub format: AudioFormat,
}

impl Repaired {
    pub fn duration_ms(&self) -> u64 {
        self.format.duration_ms(self.frames)
    }
}

/// Rebuilds a WAV header's size fields from the file's actual length.
///
/// For a recording cut short between header syncs: the audio is all there, but the
/// header understates it by up to a second. This is what turns a recording interrupted
/// by sleep, a crash, or a force-quit back into a file that states its own true length.
pub fn repair(path: impl AsRef<Path>) -> Result<Repaired> {
    let path = path.as_ref();
    let mut file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;

    let Some(format) = read_format(&mut file)? else {
        // Too short to have a header at all: nothing was ever written.
        return Ok(Repaired {
            frames: 0,
            format: AudioFormat::new(0, 1),
        });
    };

    let file_len = file.metadata()?.len();
    let bytes_per_frame = format.channels.max(1) as u64 * (BITS_PER_SAMPLE as u64 / 8);
    let data_len = file_len - HEADER_LEN;
    let frames = data_len / bytes_per_frame.max(1);
    // Never claim a partial frame exists.
    let aligned_data_len = frames * bytes_per_frame;

    file.seek(SeekFrom::Start(RIFF_SIZE_OFFSET))?;
    file.write_all(&((HEADER_LEN - 8 + aligned_data_len) as u32).to_le_bytes())?;
    file.seek(SeekFrom::Start(DATA_SIZE_OFFSET))?;
    file.write_all(&(aligned_data_len as u32).to_le_bytes())?;
    file.sync_all()?;

    Ok(Repaired { frames, format })
}

/// What an extracted span turned out to hold.
#[derive(Debug, Clone, Copy)]
pub struct Extracted {
    pub frames: u64,
    pub format: AudioFormat,
}

impl Extracted {
    pub fn duration_ms(&self) -> u64 {
        self.format.duration_ms(self.frames)
    }
}

/// Writes the span between two instants of `source` into `destination` as its own WAV.
///
/// The samples are copied as bytes — 16-bit PCM in, the same 16-bit PCM out, with no
/// decode and re-encode in between, so a span of a recording is exactly the bytes it was
/// in the original. The cut lands on a frame boundary, which is what keeps a `conversation`
/// recording's two channels aligned: half a frame of slippage would swap the mic and system
/// samples for the rest of the file.
///
/// Bounds are clamped rather than refused. `end_ms` past the end of the recording means "to
/// the end", which is what a request built from a rounded duration asks for. A span that
/// clamps down to nothing returns zero frames for the caller to report — writing an empty
/// WAV and calling it a success would be the same silent failure a dead tap produces.
pub fn extract(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    start_ms: u64,
    end_ms: u64,
) -> Result<Extracted> {
    use std::io::Read;

    let mut input = File::open(source.as_ref())?;
    let Some(format) = read_format(&mut input)? else {
        return Ok(Extracted {
            frames: 0,
            format: AudioFormat::new(0, 1),
        });
    };

    let bytes_per_frame = format.channels.max(1) as u64 * (BITS_PER_SAMPLE as u64 / 8);
    let total_frames = input
        .metadata()?
        .len()
        .saturating_sub(HEADER_LEN)
        / bytes_per_frame.max(1);

    let frame_at = |ms: u64| ms.saturating_mul(format.sample_rate as u64) / 1000;
    let first = frame_at(start_ms).min(total_frames);
    let last = frame_at(end_ms).min(total_frames);

    if last <= first {
        return Ok(Extracted { frames: 0, format });
    }

    // A WAV states its sizes in 32-bit fields, so a span cannot exceed 4 GB however long
    // the recording is. At 48 kHz stereo that is six hours, and clamping keeps the header
    // honest rather than wrapping it around to a small number.
    let max_frames = (u32::MAX as u64) / bytes_per_frame.max(1);
    let frames = (last - first).min(max_frames);
    let data_len = frames * bytes_per_frame;

    let mut output = BufWriter::new(File::create(destination.as_ref())?);
    output.write_all(&header_bytes(format, data_len as u32))?;

    input.seek(SeekFrom::Start(HEADER_LEN + first * bytes_per_frame))?;
    let mut buffer = vec![0u8; 128 * 1024];
    let mut remaining = data_len;
    while remaining > 0 {
        let want = remaining.min(buffer.len() as u64) as usize;
        input.read_exact(&mut buffer[..want])?;
        output.write_all(&buffer[..want])?;
        remaining -= want as u64;
    }

    output.flush()?;
    output.get_mut().sync_all()?;

    Ok(Extracted { frames, format })
}

/// Float to 16-bit PCM, clamped. Values outside [-1, 1] would wrap around and turn a
/// loud passage into noise, so they are clipped instead.
#[inline]
fn to_i16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    (clamped * i16::MAX as f32).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("hearsay-wav-test-{}-{name}.wav", std::process::id()));
        path
    }

    fn read_back(path: &Path) -> (hound::WavSpec, Vec<i16>) {
        let reader = hound::WavReader::open(path).expect("file should be a readable wav");
        let spec = reader.spec();
        let samples = reader
            .into_samples::<i16>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("samples should decode");
        (spec, samples)
    }

    #[test]
    fn writes_a_playable_stereo_file() {
        let path = temp_path("stereo");
        let format = AudioFormat::new(48_000, 2);
        let mut writer = WavWriter::create(&path, format).expect("writer should be created");

        // Two frames: left full scale, right silent.
        writer
            .write_samples(&[1.0, 0.0, -1.0, 0.0])
            .expect("samples should write");
        writer.finalize().expect("finalise should succeed");

        let (spec, samples) = read_back(&path);
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(samples, vec![32767, 0, -32767, 0]);
        assert_eq!(writer.frames_written(), 2);

        let _ = std::fs::remove_file(&path);
    }

    /// The point of the whole design: kill the writer without finalising and the file is
    /// still a valid WAV containing the audio written before the last sync.
    #[test]
    fn a_file_abandoned_without_finalising_is_still_playable() {
        let path = temp_path("interrupted");
        let format = AudioFormat::new(48_000, 1);
        let mut writer = WavWriter::create(&path, format).expect("writer should be created");

        // Two full sync intervals, so the header has been rewritten at least once.
        let block = vec![0.5f32; HEADER_SYNC_INTERVAL_FRAMES as usize];
        writer.write_samples(&block).expect("first block writes");
        writer.write_samples(&block).expect("second block writes");

        // Simulate a process that never gets to clean up: forget the writer, so neither
        // finalize nor Drop runs.
        std::mem::forget(writer);

        let (spec, samples) = read_back(&path);
        assert_eq!(spec.channels, 1);
        assert!(
            samples.len() >= HEADER_SYNC_INTERVAL_FRAMES as usize,
            "expected at least one synced interval, got {} samples",
            samples.len()
        );
        assert!(samples.iter().all(|s| *s == 16384));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn repair_recovers_audio_written_after_the_last_sync() {
        let path = temp_path("repair");
        let format = AudioFormat::new(48_000, 1);
        let mut writer = WavWriter::create(&path, format).expect("writer should be created");
        writer
            .write_samples(&vec![0.25f32; 1000])
            .expect("samples write");
        // Flush the audio but deliberately leave the header stale.
        writer.inner.flush().expect("flush should succeed");
        std::mem::forget(writer);

        let (_, before) = read_back(&path);
        assert!(before.is_empty(), "header should still claim an empty file");

        let repaired = repair(&path).expect("repair should succeed");
        assert_eq!(repaired.frames, 1000);

        let (_, after) = read_back(&path);
        assert_eq!(after.len(), 1000);

        let _ = std::fs::remove_file(&path);
    }

    /// Repair has to recover the format too, or a recovered recording has no duration.
    #[test]
    fn repair_reports_the_format_it_found() {
        let path = temp_path("repair-format");
        let format = AudioFormat::new(44_100, 2);
        let mut writer = WavWriter::create(&path, format).expect("writer should be created");
        writer
            .write_samples(&vec![0.1f32; 44_100 * 2])
            .expect("samples write");
        writer.inner.flush().expect("flush should succeed");
        std::mem::forget(writer);

        let repaired = repair(&path).expect("repair should succeed");
        assert_eq!(repaired.format.sample_rate, 44_100);
        assert_eq!(repaired.format.channels, 2);
        assert_eq!(repaired.frames, 44_100);
        assert_eq!(repaired.duration_ms(), 1_000);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn silence_is_written_as_true_zero() {
        let path = temp_path("silence");
        let format = AudioFormat::new(48_000, 2);
        let mut writer = WavWriter::create(&path, format).expect("writer should be created");
        writer.write_silence(3).expect("silence writes");
        writer.finalize().expect("finalise should succeed");

        let (_, samples) = read_back(&path);
        assert_eq!(samples, vec![0, 0, 0, 0, 0, 0]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn samples_beyond_full_scale_clip_rather_than_wrap() {
        assert_eq!(to_i16(2.0), 32767);
        assert_eq!(to_i16(-2.0), -32767);
        assert_eq!(to_i16(0.0), 0);
    }

    /// One second of stereo where every frame is numbered, so an extracted span can be
    /// checked against the frame it should have started at.
    fn numbered_stereo(path: &Path, rate: u32) {
        let mut writer = WavWriter::create(path, AudioFormat::new(rate, 2))
            .expect("writer should be created");
        for frame in 0..rate {
            // Frame index in the left channel, its negation in the right.
            let value = (frame % 20_000) as f32 / i16::MAX as f32;
            writer
                .write_samples(&[value, -value])
                .expect("samples should write");
        }
        writer.finalize().expect("finalise should succeed");
    }

    #[test]
    fn extracts_a_span_on_frame_boundaries() {
        let source = temp_path("extract-source");
        let span = temp_path("extract-span");
        numbered_stereo(&source, 48_000);

        let extracted = extract(&source, &span, 250, 750).expect("the span should be written");

        assert_eq!(extracted.frames, 24_000, "half a second at 48 kHz");
        assert_eq!(extracted.duration_ms(), 500);

        let (spec, samples) = read_back(&span);
        assert_eq!(spec.channels, 2, "a stereo span stays stereo");
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(samples.len(), 48_000, "two channels of 24,000 frames");

        // The first frame of the span is frame 12,000 of the recording, and its two
        // channels are still the right way round.
        assert_eq!(samples[0], 12_000);
        assert_eq!(samples[1], -12_000);
        assert_eq!(samples[2], 12_001);

        let _ = std::fs::remove_file(&source);
        let _ = std::fs::remove_file(&span);
    }

    #[test]
    fn an_end_past_the_recording_means_to_the_end() {
        let source = temp_path("extract-clamp");
        let span = temp_path("extract-clamp-out");
        numbered_stereo(&source, 48_000);

        let extracted = extract(&source, &span, 900, 60_000).expect("the span should be written");

        assert_eq!(extracted.frames, 4_800, "the last 100 ms and no more");

        let _ = std::fs::remove_file(&source);
        let _ = std::fs::remove_file(&span);
    }

    #[test]
    fn a_span_that_selects_nothing_reports_zero_frames() {
        let source = temp_path("extract-empty");
        let span = temp_path("extract-empty-out");
        numbered_stereo(&source, 48_000);

        let extracted = extract(&source, &span, 5_000, 6_000).expect("the call should succeed");
        assert_eq!(extracted.frames, 0, "a span past the end holds nothing");

        let _ = std::fs::remove_file(&source);
        let _ = std::fs::remove_file(&span);
    }
}
