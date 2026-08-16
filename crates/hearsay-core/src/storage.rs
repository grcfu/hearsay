//! What Hearsay's recordings weigh on disk.
//!
//! Audio is the only thing the app writes that grows without bound. A stereo
//! `conversation` recording is roughly 700 MB an hour and a `listen_only` one about half
//! that — 16-bit PCM at whatever rate the devices negotiated, which is usually 48 kHz.
//! Transcripts, summaries and questions are text and round to nothing beside it.
//!
//! This exists so the app can say which recordings those megabytes are actually in.
//! Deleting audio to reclaim space is a judgement about a particular recording, and it
//! cannot be made against a list that shows every one of them as the same size.

use std::path::Path;

/// What one recording's audio occupies, if it is still there.
///
/// `None` for a recording with no path, and for a path that no longer resolves. A file
/// that has gone missing is reported the same way as one that was never there: the size
/// is unknown either way, and this module does not speculate about why.
pub fn audio_bytes(path: Option<&str>) -> Option<u64> {
    let path = path?;
    std::fs::metadata(Path::new(path))
        .ok()
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
}

/// Roughly how long a WAV of this size holds, at the given rate and channel count.
///
/// Only an estimate: it assumes 16-bit samples and ignores the header, which is 44 bytes
/// against a file measured in hundreds of megabytes. Used for explaining a size in the
/// interface, never for seeking or trimming — those read the real header.
pub fn approximate_ms(bytes: u64, sample_rate: u32, channels: u16) -> Option<u64> {
    let bytes_per_second = u64::from(sample_rate) * u64::from(channels) * 2;
    if bytes_per_second == 0 {
        return None;
    }
    Some(bytes * 1000 / bytes_per_second)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_missing_path_has_no_size_rather_than_a_size_of_zero() {
        assert_eq!(audio_bytes(None), None);
        assert_eq!(audio_bytes(Some("/nonexistent/nothing-here.wav")), None);
    }

    #[test]
    fn a_real_file_reports_its_length() {
        let dir = std::env::temp_dir().join("hearsay-storage-test");
        std::fs::create_dir_all(&dir).expect("temp dir is made");
        let path = dir.join("sized.wav");
        let mut file = std::fs::File::create(&path).expect("file is created");
        file.write_all(&[0u8; 2048]).expect("bytes are written");
        drop(file);

        assert_eq!(audio_bytes(Some(&path.to_string_lossy())), Some(2048));
        let _ = std::fs::remove_file(&path);
    }

    /// A directory is not a recording. Reporting its size would put a number beside
    /// something that cannot be played or deleted as audio.
    #[test]
    fn a_directory_is_not_reported_as_audio() {
        let dir = std::env::temp_dir();
        assert_eq!(audio_bytes(Some(&dir.to_string_lossy())), None);
    }

    #[test]
    fn a_stereo_hour_is_about_seven_hundred_megabytes() {
        let hour = 3_600_000;
        let bytes = 48_000u64 * 2 * 2 * 3_600;
        assert_eq!(approximate_ms(bytes, 48_000, 2), Some(hour));
        assert!(bytes > 690_000_000 && bytes < 700_000_000, "got {bytes}");
    }

    #[test]
    fn a_nonsense_format_gives_no_duration_rather_than_dividing_by_zero() {
        assert_eq!(approximate_ms(1_000, 0, 2), None);
        assert_eq!(approximate_ms(1_000, 48_000, 0), None);
    }
}
