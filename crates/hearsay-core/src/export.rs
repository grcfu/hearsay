//! Saving a copy of a recording's audio somewhere the user picked.
//!
//! A recording lives in `~/Library/Application Support/hearsay/recordings` as a 16-bit
//! WAV, which is right for transcription and wrong for keeping: an hour of stereo is
//! around 600 MB, and nothing on a phone wants to open it. So an export is either the
//! original bytes copied verbatim, or an AAC pass that lands about a twentieth of the
//! size and plays on anything.
//!
//! An export can also be a **span** rather than the whole recording, because the reason to
//! save audio is usually one part of it: the ten minutes where the offer was discussed, not
//! the hour around them. The cut happens on the WAV, where a frame boundary is exact, and
//! only then is the result compressed — so a span is the original samples, not a decode and
//! re-encode of them.
//!
//! The compressed pass is `afconvert`, which ships with macOS. That keeps this to a
//! `Command` spawn with no new dependency and nothing bundled — the same shape as the
//! transcription sidecar, and, like it, entirely local.

use anyhow::{anyhow, bail, Context, Result};
use hearsay_audio::wav;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Bitrate for the AAC pass, in bits per second.
///
/// This is speech, where 96 kbps stereo is past the point of audible loss, and it keeps an
/// hour of conversation to roughly 43 MB. High enough that nobody re-exports at a better
/// setting, low enough to attach to a message.
const AAC_BITRATE: &str = "96000";

/// What an export was asked to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// The recording exactly as captured — lossless, large, and still stereo in
    /// `conversation` mode, so the channel split survives the copy.
    Wav,
    /// AAC in an MP4 container.
    M4a,
}

impl ExportFormat {
    /// Reads the format off the name the user typed into the save sheet.
    ///
    /// MP3 is named explicitly rather than falling into the catch-all. macOS can decode
    /// MP3 and cannot encode it, so the alternative to saying so is writing AAC under an
    /// `.mp3` extension — a file that lies about itself.
    pub fn from_path(path: &Path) -> Result<Self> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();

        match extension.as_str() {
            "wav" | "wave" => Ok(Self::Wav),
            "m4a" | "mp4" | "aac" => Ok(Self::M4a),
            "mp3" => Err(anyhow!(
                "macOS has no MP3 encoder, so Hearsay cannot write one. Save as .m4a \
                 instead — it is the same idea, a little smaller, and opens in Music, \
                 QuickTime, VLC, Windows, and on a phone."
            )),
            "" => Err(anyhow!(
                "give the file an extension: .m4a for a small copy, .wav for the original"
            )),
            other => Err(anyhow!(
                "cannot save audio as .{other} — use .m4a for a small copy or .wav for \
                 the original"
            )),
        }
    }

    /// The extension an export of this format should carry.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::M4a => "m4a",
        }
    }
}

/// Which part of a recording to save. Both ends are milliseconds from its start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start_ms: u64,
    pub end_ms: u64,
}

impl Span {
    /// Builds a span, rejecting the two ways one can be meaningless.
    ///
    /// Rejected here rather than clamped, because a reversed or empty span means the user
    /// typed something they did not mean — and a file quietly containing the whole
    /// recording, or nothing, would not tell them that.
    pub fn new(start_ms: u64, end_ms: u64) -> Result<Self> {
        if end_ms == start_ms {
            bail!("the start and end of the selection are the same moment");
        }
        if end_ms < start_ms {
            bail!("the selection ends before it starts");
        }
        Ok(Self { start_ms, end_ms })
    }

    /// How long the span asks for, before the recording's own length is taken into account.
    pub fn duration_ms(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }
}

/// Copies or converts `source` to `destination`, returning the size of what was written.
///
/// `span` of `None` saves the whole recording.
///
/// Blocks for as long as the conversion takes — a few seconds for a long meeting — so
/// call it off the UI thread.
pub fn export_audio(source: &Path, destination: &Path, span: Option<Span>) -> Result<u64> {
    if !source.is_file() {
        bail!(
            "the audio file for this recording is missing from {}",
            source.display()
        );
    }

    // Saving over the original would convert a recording into a copy of itself and lose
    // the source in the process. There is no reason to allow it.
    if same_file(source, destination) {
        bail!("that is the recording's own file — pick somewhere else to save the copy");
    }

    let format = ExportFormat::from_path(destination)?;

    match (format, span) {
        (ExportFormat::Wav, None) => {
            std::fs::copy(source, destination).with_context(|| {
                format!("could not write {}", destination.display())
            })?;
        }
        (ExportFormat::M4a, None) => convert_to_m4a(source, destination)?,
        (ExportFormat::Wav, Some(span)) => {
            cut(source, destination, span)?;
        }
        (ExportFormat::M4a, Some(span)) => {
            // The cut has to happen on the WAV — `afconvert` converts whole files and
            // cannot trim — so the span is written to a scratch file first and compressed
            // from there. The scratch file goes in Hearsay's own directory, not `/tmp`,
            // and is removed however this call ends.
            let scratch = Scratch::new(span)?;
            cut(source, scratch.path(), span)?;
            convert_to_m4a(scratch.path(), destination)?;
        }
    }

    let written = std::fs::metadata(destination)
        .with_context(|| format!("could not read back {}", destination.display()))?
        .len();

    if written == 0 {
        // An empty file is a failure that reported success. Do not leave it behind for
        // the user to discover when they try to play it.
        let _ = std::fs::remove_file(destination);
        bail!("the export produced an empty file");
    }

    Ok(written)
}

/// Writes just `span` of `source` to `destination` as a WAV.
///
/// A span that lands entirely past the end of the recording is reported rather than written:
/// an empty file that opens and plays nothing is the kind of quiet failure this project
/// treats as a bug.
fn cut(source: &Path, destination: &Path, span: Span) -> Result<()> {
    let extracted = wav::extract(source, destination, span.start_ms, span.end_ms)
        .with_context(|| format!("could not read {}", source.display()))?;

    if extracted.frames == 0 {
        let _ = std::fs::remove_file(destination);
        bail!(
            "that selection holds no audio — the recording is shorter than {}",
            clock(span.start_ms)
        );
    }

    Ok(())
}

/// A working file in Hearsay's own directory, removed when it goes out of scope.
///
/// The removal is in `Drop` so it happens on the error paths too. A failed export that left
/// a few hundred megabytes of scratch WAV behind would be a slow leak nobody would think to
/// look for.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(span: Span) -> Result<Self> {
        let dir = crate::paths::data_dir()?;
        let path = dir.join(format!(
            "export-in-progress-{}-{}-{}.wav",
            std::process::id(),
            span.start_ms,
            span.end_ms
        ));
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.path.exists() {
            if let Err(error) = std::fs::remove_file(&self.path) {
                tracing::warn!(
                    "could not remove the export scratch file {}: {error}",
                    self.path.display()
                );
            }
        }
    }
}

/// Runs the AAC pass, cleaning up after itself if it fails.
fn convert_to_m4a(source: &Path, destination: &Path) -> Result<()> {
    let output = Command::new("/usr/bin/afconvert")
        .arg("-f")
        .arg("m4af")
        .arg("-d")
        .arg("aac")
        .arg("-b")
        .arg(AAC_BITRATE)
        .arg(source)
        .arg(destination)
        .output()
        .context("could not run /usr/bin/afconvert, the macOS audio converter")?;

    if !output.status.success() {
        // A failed run can still have created the file, and a half-written m4a is worse
        // than none: it opens, plays part way, and stops.
        let _ = std::fs::remove_file(destination);
        bail!("could not convert the audio: {}", diagnosis(&output));
    }

    Ok(())
}

/// The most useful line of an `afconvert` failure.
///
/// It prints its usage screen on a bad argument, so the whole of stderr is mostly noise;
/// the first line is the actual complaint.
fn diagnosis(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| match output.status.code() {
            Some(code) => format!("afconvert exited with status {code}"),
            None => "afconvert was killed".to_string(),
        })
}

/// Whether two paths name the same file, without requiring the destination to exist yet.
fn same_file(source: &Path, destination: &Path) -> bool {
    match (source.canonicalize(), destination.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        // The destination is usually a name that does not exist yet, so fall back to
        // comparing its directory and file name.
        _ => match (
            source.canonicalize(),
            destination.parent().map(Path::canonicalize),
        ) {
            (Ok(left), Some(Ok(parent))) => {
                Some(left) == destination.file_name().map(|name| parent.join(name))
            }
            _ => false,
        },
    }
}

/// A file name for the export, built from a recording's title, date, and span.
///
/// Titles are free text the user typed, so this strips what a filesystem or a mail client
/// would object to rather than trusting it. The date is part of the name because an export
/// leaves Hearsay and loses the list it was sitting in, and a span is named in the file too —
/// a clip that does not say which part of the meeting it is cannot be placed again later.
pub fn suggested_file_name(
    title: &str,
    date: &str,
    span: Option<Span>,
    format: ExportFormat,
) -> String {
    let cleaned = sanitize(title);
    let named = if cleaned.is_empty() {
        format!("Recording {date}")
    } else {
        format!("{cleaned} {date}")
    };
    let stem = match span {
        Some(span) => format!("{named} {} to {}", stamp(span.start_ms), stamp(span.end_ms)),
        None => named,
    };
    format!("{stem}.{}", format.extension())
}

/// An instant as a file name can carry it: `5m12s`, or `1h04m12s` past an hour.
///
/// Not `MM:SS`, because a colon is a path separator as far as the Finder is concerned — it
/// displays one as a slash, and a file named `05:12` is a small mystery on disk.
fn stamp(ms: u64) -> String {
    let seconds = ms / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else {
        format!("{minutes}m{seconds:02}s")
    }
}

/// An instant as the rest of the app writes it, for error messages: `MM:SS`, or `H:MM:SS`.
fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// Reduces a title to something safe to write to disk: no separators, no control
/// characters, no leading dot, and short enough for any filesystem.
fn sanitize(title: &str) -> String {
    let mut out = String::new();
    let mut last_was_space = false;

    for character in title.chars() {
        let replacement = match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            other if other.is_control() => ' ',
            other => other,
        };
        if replacement == ' ' {
            if !last_was_space && !out.is_empty() {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(replacement);
            last_was_space = false;
        }
    }

    let trimmed = out.trim().trim_start_matches('.').trim();
    trimmed.chars().take(80).collect::<String>().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn reads_the_format_from_the_extension() {
        assert_eq!(
            ExportFormat::from_path(Path::new("/tmp/a.m4a")).expect("m4a is known"),
            ExportFormat::M4a
        );
        assert_eq!(
            ExportFormat::from_path(Path::new("/tmp/a.WAV")).expect("wav is known"),
            ExportFormat::Wav
        );
    }

    #[test]
    fn refuses_mp3_rather_than_mislabelling_aac() {
        let error = ExportFormat::from_path(Path::new("/tmp/a.mp3"))
            .expect_err("mp3 cannot be written")
            .to_string();
        assert!(error.contains("MP3"), "{error}");
        assert!(error.contains(".m4a"), "{error}");
    }

    #[test]
    fn refuses_a_name_with_no_extension() {
        assert!(ExportFormat::from_path(Path::new("/tmp/recording")).is_err());
    }

    #[test]
    fn names_a_file_after_the_recording() {
        assert_eq!(
            suggested_file_name("Standup with Dana", "2026-08-12", None, ExportFormat::M4a),
            "Standup with Dana 2026-08-12.m4a"
        );
    }

    #[test]
    fn strips_what_a_filesystem_would_object_to() {
        assert_eq!(
            suggested_file_name("1:1 / review \"notes\"", "2026-08-12", None, ExportFormat::Wav),
            "1 1 review notes 2026-08-12.wav"
        );
    }

    #[test]
    fn falls_back_when_a_title_is_all_punctuation() {
        assert_eq!(
            suggested_file_name("///", "2026-08-12", None, ExportFormat::M4a),
            "Recording 2026-08-12.m4a"
        );
    }

    #[test]
    fn reports_a_missing_source_rather_than_writing_nothing() {
        let error = export_audio(
            Path::new("/nonexistent/hearsay/never.wav"),
            Path::new("/tmp/hearsay-export-test.m4a"),
            None,
        )
        .expect_err("a missing source cannot be exported")
        .to_string();
        assert!(error.contains("missing"), "{error}");
    }

    #[test]
    fn refuses_to_save_over_the_recording_itself() {
        let path = std::env::temp_dir().join(format!("hearsay-export-self-{}.wav", std::process::id()));
        std::fs::write(&path, b"RIFF").expect("the fixture should be written");
        let error = export_audio(&path, &path, None)
            .expect_err("saving over the source is refused")
            .to_string();
        std::fs::remove_file(&path).ok();
        assert!(error.contains("own file"), "{error}");
    }

    /// The real conversion, end to end, against a WAV written for the purpose.
    #[test]
    fn converts_a_wav_to_a_playable_m4a() {
        let dir = std::env::temp_dir();
        let source: PathBuf = dir.join(format!("hearsay-export-{}.wav", std::process::id()));
        let destination: PathBuf = dir.join(format!("hearsay-export-{}.m4a", std::process::id()));

        write_test_wav(&source);
        let written = export_audio(&source, &destination, None).expect("the conversion should succeed");

        assert!(written > 0, "the export should not be empty");
        let header = std::fs::read(&destination).expect("the export should be readable");
        assert_eq!(&header[4..8], b"ftyp", "the export should be an MP4 container");

        std::fs::remove_file(&source).ok();
        std::fs::remove_file(&destination).ok();
    }

    #[test]
    fn copies_a_wav_byte_for_byte() {
        let dir = std::env::temp_dir();
        let source = dir.join(format!("hearsay-copy-{}.wav", std::process::id()));
        let destination = dir.join(format!("hearsay-copy-out-{}.wav", std::process::id()));

        write_test_wav(&source);
        export_audio(&source, &destination, None).expect("the copy should succeed");

        assert_eq!(
            std::fs::read(&source).expect("source"),
            std::fs::read(&destination).expect("destination"),
            "a wav export should be the original bytes"
        );

        std::fs::remove_file(&source).ok();
        std::fs::remove_file(&destination).ok();
    }

    #[test]
    fn names_a_clip_after_the_part_it_holds() {
        let span = Span::new(192_000, 525_000).expect("a forward span is valid");
        assert_eq!(
            suggested_file_name("Standup with Dana", "2026-08-12", Some(span), ExportFormat::M4a),
            "Standup with Dana 2026-08-12 3m12s to 8m45s.m4a"
        );
    }

    #[test]
    fn names_an_hour_in_without_a_colon() {
        // A colon is displayed as a slash by the Finder, so a stamp never contains one.
        assert_eq!(stamp(3_872_000), "1h04m32s");
        assert!(!stamp(3_872_000).contains(':'));
    }

    #[test]
    fn refuses_a_span_that_cannot_mean_anything() {
        assert!(Span::new(5_000, 5_000).is_err(), "an empty span is refused");
        assert!(Span::new(9_000, 4_000).is_err(), "a reversed span is refused");
        assert_eq!(
            Span::new(1_000, 4_000).expect("a forward span is valid").duration_ms(),
            3_000
        );
    }

    #[test]
    fn saves_only_the_selected_span_as_a_wav() {
        let dir = std::env::temp_dir();
        let source = dir.join(format!("hearsay-span-{}.wav", std::process::id()));
        let destination = dir.join(format!("hearsay-span-out-{}.wav", std::process::id()));

        write_test_wav_ms(&source, 2_000);
        let span = Span::new(500, 1_500).expect("a forward span is valid");
        let written = export_audio(&source, &destination, Some(span)).expect("the cut succeeds");

        // One second of 48 kHz stereo 16-bit, plus the header.
        assert_eq!(written, 44 + 48_000 * 2 * 2);

        std::fs::remove_file(&source).ok();
        std::fs::remove_file(&destination).ok();
    }

    #[test]
    fn a_span_compressed_is_smaller_than_the_whole_recording_compressed() {
        let dir = std::env::temp_dir();
        let source = dir.join(format!("hearsay-span-aac-{}.wav", std::process::id()));
        let clip = dir.join(format!("hearsay-span-aac-clip-{}.m4a", std::process::id()));
        let whole = dir.join(format!("hearsay-span-aac-whole-{}.m4a", std::process::id()));

        write_test_wav_ms(&source, 4_000);
        let span = Span::new(0, 1_000).expect("a forward span is valid");
        let clip_bytes = export_audio(&source, &clip, Some(span)).expect("the clip is written");
        let whole_bytes = export_audio(&source, &whole, None).expect("the whole is written");

        assert!(
            clip_bytes * 2 < whole_bytes,
            "a quarter of the recording should compress to well under half the size: \
             {clip_bytes} against {whole_bytes}"
        );
        assert_eq!(
            &std::fs::read(&clip).expect("the clip is readable")[4..8],
            b"ftyp",
            "a clip is still an MP4 container"
        );

        // The scratch WAV the cut went through is not left behind.
        let leftovers = std::fs::read_dir(crate::paths::data_dir().expect("a data dir"))
            .expect("the data dir is readable")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("export-in-progress-")
            })
            .count();
        assert_eq!(leftovers, 0, "the scratch file should be gone");

        std::fs::remove_file(&source).ok();
        std::fs::remove_file(&clip).ok();
        std::fs::remove_file(&whole).ok();
    }

    #[test]
    fn reports_a_selection_that_starts_past_the_end() {
        let dir = std::env::temp_dir();
        let source = dir.join(format!("hearsay-span-past-{}.wav", std::process::id()));
        let destination = dir.join(format!("hearsay-span-past-out-{}.wav", std::process::id()));

        write_test_wav_ms(&source, 500);
        let span = Span::new(9_000, 12_000).expect("a forward span is valid");
        let error = export_audio(&source, &destination, Some(span))
            .expect_err("a span past the end holds nothing")
            .to_string();

        assert!(error.contains("no audio"), "{error}");
        assert!(!destination.exists(), "no empty file should be left behind");

        std::fs::remove_file(&source).ok();
    }

    /// Half a second of stereo tone, written by hand so the test needs no fixture file.
    fn write_test_wav(path: &Path) {
        write_test_wav_ms(path, 500);
    }

    fn write_test_wav_ms(path: &Path, ms: u32) {
        let (rate, channels) = (48_000u32, 2u16);
        let frames = rate / 1000 * ms;
        let data_len = frames * channels as u32 * 2;
        let mut bytes = Vec::with_capacity(44 + data_len as usize);

        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        bytes.extend_from_slice(&(channels * 2).to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());

        for frame in 0..frames {
            let value = ((frame as f32 * 0.05).sin() * 8_000.0) as i16;
            bytes.extend_from_slice(&value.to_le_bytes());
            bytes.extend_from_slice(&(-value).to_le_bytes());
        }

        std::fs::write(path, bytes).expect("the fixture wav should be written");
    }
}
