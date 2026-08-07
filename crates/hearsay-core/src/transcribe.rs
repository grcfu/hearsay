//! Driving the faster-whisper sidecar.
//!
//! Transcription runs as a Python subprocess, not as linked Rust bindings. The sidecar
//! can be interrupted, can crash, and can take minutes on a long recording without any
//! of that touching the app's process. Its contract mirrors the audio helper's: JSON
//! events on stderr, one JSON result on stdout.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Which channel of a recording to transcribe.
///
/// The channel travels with every segment because it is the entire basis for speaker
/// attribution: left is the user, right is everyone else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// A single-channel recording, or a stereo one averaged down.
    Mono,
    /// Left channel — the microphone.
    Left,
    /// Right channel — system audio.
    Right,
}

impl Channel {
    fn as_arg(self) -> &'static str {
        match self {
            Channel::Mono => "mono",
            Channel::Left => "left",
            Channel::Right => "right",
        }
    }

    /// How this channel is recorded in the database.
    ///
    /// `listen_only` recordings are mono system audio, so mono and right both mean
    /// "everyone else".
    pub fn db_channel(self) -> &'static str {
        match self {
            Channel::Left => "mic",
            Channel::Mono | Channel::Right => "system",
        }
    }
}

/// One transcribed span, in milliseconds from the start of the recording.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    #[serde(default = "default_channel")]
    pub channel: String,
}

fn default_channel() -> String {
    "system".to_string()
}

/// What the sidecar prints on stdout when it finishes.
#[derive(Debug, Clone, Deserialize)]
pub struct TranscriptionResult {
    pub segments: Vec<TranscriptSegment>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(default)]
    pub model: String,
}

/// Progress from the sidecar, for the UI to show.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscribeEvent {
    /// Weights are being fetched. The only step in the app that touches the network
    /// without an API key, and it happens once per model.
    Download { file: String, percent: u8 },
    DownloadDone,
    ModelReady,
    Progress { channel: String, percent: u8 },
    Done { channel: String, segments: usize },
    Error { kind: String, message: String },
    Log { line: String },
}

/// Where the sidecar and its interpreter live.
#[derive(Debug, Clone)]
pub struct SidecarPaths {
    pub python: PathBuf,
    pub script: PathBuf,
}

impl SidecarPaths {
    /// Locates the venv interpreter and `transcribe.py`.
    ///
    /// Checked in order: explicit overrides, next to the executable (how a built app
    /// ships them), then the repo checkout so development works with no install step.
    pub fn discover() -> Result<Self> {
        let mut roots: Vec<PathBuf> = Vec::new();

        if let Ok(explicit) = std::env::var("HEARSAY_PYTHON_DIR") {
            roots.push(PathBuf::from(explicit));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                roots.push(dir.join("python"));
                roots.push(dir.join("../Resources/python"));
            }
        }
        roots.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../python"));

        for root in &roots {
            let python = root.join(".venv/bin/python");
            let script = root.join("transcribe.py");
            if python.is_file() && script.is_file() {
                return Ok(Self { python, script });
            }
        }

        Err(anyhow!(
            "could not find the transcription sidecar. Run ./python/setup_venv.sh — \
             looked in: {}",
            roots
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    /// Whether the sidecar is installed, for the UI to report before a recording starts.
    pub fn is_available() -> bool {
        Self::discover().is_ok()
    }
}

/// Default model. Distilled large-v3 is roughly six times faster than large-v3 on CPU
/// with negligible quality loss on meeting speech, which is what makes on-device
/// transcription practical on a laptop.
pub const DEFAULT_MODEL: &str = "distil-large-v3";

/// Transcribes one channel of a recording, reporting progress through `on_event`.
///
/// Blocks until the sidecar exits. Intended to be called from a worker thread.
pub fn transcribe_channel(
    audio_path: &Path,
    channel: Channel,
    model: &str,
    models_dir: &Path,
    mut on_event: impl FnMut(TranscribeEvent),
) -> Result<TranscriptionResult> {
    let paths = SidecarPaths::discover()?;

    let mut child = Command::new(&paths.python)
        .arg(&paths.script)
        .arg("--audio")
        .arg(audio_path)
        .arg("--models-dir")
        .arg(models_dir)
        .arg("--model")
        .arg(model)
        .arg("--channel")
        .arg(channel.as_arg())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("could not start {}", paths.script.display()))?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("sidecar produced no stderr"))?;

    // Read progress on this thread's stderr reader while stdout accumulates in the pipe.
    // The result is a single small JSON object, so the pipe buffer is ample.
    let reader = BufReader::new(stderr);
    let mut last_error: Option<(String, String)> = None;

    for line in reader.lines() {
        let Ok(line) = line else { break };
        let event = parse_event(&line);
        if let TranscribeEvent::Error { kind, message } = &event {
            last_error = Some((kind.clone(), message.clone()));
        }
        on_event(event);
    }

    let output = child.wait_with_output().context("sidecar did not exit")?;

    if !output.status.success() {
        if let Some((kind, message)) = last_error {
            return Err(anyhow!("transcription failed ({kind}): {message}"));
        }
        return Err(anyhow!(
            "transcription failed with status {}",
            output.status.code().unwrap_or(-1)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("transcription produced no result"));
    }

    serde_json::from_str(trimmed).with_context(|| "could not parse the transcription result")
}

/// Transcribes a whole recording, one pass per channel, and returns every segment
/// tagged with who said it.
///
/// A `listen_only` recording is one mono channel and everything in it is `system`. A
/// `conversation` recording is two passes: the left channel is the user, the right is
/// everyone else. That split is the entire speaker-attribution mechanism, and it costs
/// nothing beyond the second pass because the channels were never mixed.
///
/// The passes run one after another rather than in parallel: both are CPU-bound on the
/// same cores, so running them together makes the whole job slower, not faster, and
/// makes the progress reporting meaningless.
pub fn transcribe_recording(
    audio_path: &Path,
    mode: hearsay_audio::Mode,
    model: &str,
    models_dir: &Path,
    mut on_event: impl FnMut(TranscribeEvent),
) -> Result<Vec<TranscriptSegment>> {
    let channels: &[Channel] = match mode {
        hearsay_audio::Mode::ListenOnly => &[Channel::Mono],
        hearsay_audio::Mode::Conversation => &[Channel::Left, Channel::Right],
    };

    let mut all: Vec<TranscriptSegment> = Vec::new();
    for channel in channels {
        let result = transcribe_channel(audio_path, *channel, model, models_dir, &mut on_event)
            .with_context(|| format!("transcribing the {} channel", channel.as_arg()))?;

        // Overwrite whatever the sidecar reported: the channel a segment came from is
        // decided here, where the recording mode is known.
        for mut segment in result.segments {
            segment.channel = channel.db_channel().to_string();
            all.push(segment);
        }
    }

    // Interleave the two channels into one timeline so the transcript reads as a
    // conversation rather than as two monologues.
    all.sort_by_key(|segment| (segment.start_ms, segment.end_ms));
    Ok(all)
}

fn parse_event(line: &str) -> TranscribeEvent {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') {
        return TranscribeEvent::Log {
            line: trimmed.to_string(),
        };
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return TranscribeEvent::Log {
            line: trimmed.to_string(),
        };
    };

    let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let text = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let percent = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            .min(100) as u8
    };

    match kind {
        "download" => TranscribeEvent::Download {
            file: text("file"),
            percent: percent("percent"),
        },
        "download_done" | "model_cached" => TranscribeEvent::DownloadDone,
        "model_ready" => TranscribeEvent::ModelReady,
        "progress" => TranscribeEvent::Progress {
            channel: text("channel"),
            percent: percent("percent"),
        },
        "transcribe_done" => TranscribeEvent::Done {
            channel: text("channel"),
            segments: value
                .get("segments")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as usize,
        },
        "error" => TranscribeEvent::Error {
            kind: text("kind"),
            message: text("message"),
        },
        _ => TranscribeEvent::Log {
            line: trimmed.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_is_the_user_and_right_is_everyone_else() {
        assert_eq!(Channel::Left.db_channel(), "mic");
        assert_eq!(Channel::Right.db_channel(), "system");
    }

    #[test]
    fn a_listen_only_recording_is_attributed_to_the_system() {
        assert_eq!(Channel::Mono.db_channel(), "system");
    }

    #[test]
    fn download_progress_is_parsed() {
        let event = parse_event(r#"{"type":"download","file":"model.bin","percent":42}"#);
        match event {
            TranscribeEvent::Download { file, percent } => {
                assert_eq!(file, "model.bin");
                assert_eq!(percent, 42);
            }
            other => panic!("expected download progress, got {other:?}"),
        }
    }

    #[test]
    fn sidecar_errors_carry_their_kind() {
        let event =
            parse_event(r#"{"type":"error","kind":"download_failed","message":"no network"}"#);
        match event {
            TranscribeEvent::Error { kind, message } => {
                assert_eq!(kind, "download_failed");
                assert_eq!(message, "no network");
            }
            other => panic!("expected an error event, got {other:?}"),
        }
    }

    #[test]
    fn results_deserialise_from_the_sidecar_shape() {
        let raw = r#"{"segments":[{"start_ms":0,"end_ms":1500,"text":"hello","channel":"mic"}],
                      "language":"en","duration_ms":1500,"model":"distil-large-v3"}"#;
        let result: TranscriptionResult =
            serde_json::from_str(raw).expect("result should parse");
        assert_eq!(result.segments.len(), 1);
        assert_eq!(result.segments[0].text, "hello");
        assert_eq!(result.segments[0].channel, "mic");
        assert_eq!(result.language.as_deref(), Some("en"));
    }
}
