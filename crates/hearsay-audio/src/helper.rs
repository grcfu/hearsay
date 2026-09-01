//! Spawning and reading the Swift audio helper.
//!
//! The helper is a dumb pipe: raw interleaved float32 PCM on stdout, JSON events on
//! stderr. Everything on this side — target selection, buffering, the silence verdict,
//! teardown ordering — is policy that deliberately does not live in Swift.
//!
//! Two threads run per session. One drains stdout into a bounded channel of sample
//! buffers; the other reads stderr line by line and turns it into [`HelperEvent`]s. The
//! channel is bounded so a stalled consumer shows up as backpressure rather than as
//! unbounded memory growth.

use crate::process::AudioProcess;
use crate::source::{AudioFormat, AudioSource, Chunk};
use crate::{AudioError, Result};

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long to wait for the helper to report its format before giving up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to let the helper tear down its tap after SIGTERM before forcing the issue.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Buffers held in flight between the helper and the writer. At 48 kHz stereo each
/// buffer is a few thousand frames, so this is a few seconds of slack — enough to ride
/// out a disk hiccup, small enough that a genuinely stuck consumer cannot eat memory.
const CHANNEL_DEPTH: usize = 128;

/// What to tap. Process scope is strongly preferred: it is the difference between
/// recording the meeting and recording the meeting plus whatever music is playing.
#[derive(Debug, Clone, PartialEq)]
pub enum TapTarget {
    Processes(Vec<i32>),
    SystemWide,
}

impl TapTarget {
    fn to_args(&self) -> Vec<String> {
        match self {
            TapTarget::Processes(pids) => pids
                .iter()
                .flat_map(|pid| ["--pid".to_string(), pid.to_string()])
                .collect(),
            TapTarget::SystemWide => vec!["--system".to_string()],
        }
    }
}

/// A structured message from the helper's stderr.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HelperEvent {
    Format { sample_rate: u32, channels: u16 },
    Started,
    Level {
        peak: f32,
        rms: f64,
        frames: u64,
        /// Non-zero samples in the last window. Zero means the tap is handing back
        /// silence *now*, which the cumulative total can never say.
        interval_nonzero_samples: u64,
    },
    /// The tap is producing zeros while audio is provably playing.
    Silence {
        elapsed_seconds: f64,
        /// How long the tap has been handing back nothing but zeros, right now. Reset
        /// when a real sample arrives, so this is the length of the current outage
        /// rather than a total for the run.
        silent_seconds: f64,
        /// Whether the tap ever worked. A tap that captured audio and then stopped is a
        /// different fault from one that never started, and pointing someone at the
        /// permission settings for the first wastes their time.
        captured_audio_earlier: bool,
        message: String,
    },
    Stopped {
        reason: String,
        frames: u64,
        nonzero_samples: u64,
        /// Seconds spent silent while a target was provably playing.
        silent_while_playing_seconds: f64,
        /// Whether the run was still silent when it ended.
        ended_silent: bool,
    },
    Error { kind: String, message: String },
    /// Permission looks missing, but capture is proceeding anyway. Advisory: the
    /// silence check is what actually decides whether a recording is real.
    PermissionWarning { message: String },
    /// A plain, non-JSON line. Kept so nothing the helper says is ever swallowed.
    Log { line: String },
}

/// Live status, updated by the stderr reader for anyone who wants to show a meter.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct HelperStatus {
    pub peak: f32,
    pub rms: f64,
    pub frames: u64,
    /// The tap is silent while audio is playing *right now*. Cleared when real samples
    /// come back, so a tap that recovers stops shouting — a warning that cannot go away
    /// is one people learn to ignore, and this one has to still mean something the next
    /// time it appears.
    pub silent_while_audio_playing: bool,
    /// How long the current outage has been running.
    pub silent_seconds: f64,
    /// Whether the tap was ever working. Separates "stopped" from "never started".
    pub captured_audio_earlier: bool,
    /// Sticky: the worst outage this run has seen, so a recording can be judged at the
    /// end even if the tap recovered before anyone looked.
    pub longest_silence_seconds: f64,
}

/// Locates the helper binary.
///
/// Checked in order: an explicit override, next to the executable (how a built app
/// ships it), inside the app bundle's Resources, and finally the repo's `bin/` so a
/// `cargo run` from a checkout works without installing anything.
pub fn helper_path() -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(explicit) = std::env::var("HEARSAY_HELPER_PATH") {
        candidates.push(PathBuf::from(explicit));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("hearsay-audio-helper"));
            candidates.push(dir.join("../Resources/hearsay-audio-helper"));
        }
    }
    // Development checkout: crates/hearsay-audio/../../bin
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../bin/hearsay-audio-helper")
            .to_path_buf(),
    );

    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }

    Err(AudioError::HelperMissing {
        path: candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
    })
}

/// Every process Core Audio knows about, via `--list --all`.
pub fn list_processes() -> Result<Vec<AudioProcess>> {
    let output = Command::new(helper_path()?)
        .args(["--list", "--all"])
        .output()?;

    if !output.status.success() {
        return Err(AudioError::HelperFailed {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    serde_json::from_slice(&output.stdout).map_err(|error| AudioError::HelperFailed {
        status: 0,
        stderr: format!("could not parse process list: {error}"),
    })
}

/// Whether macOS currently permits capturing system audio. Does not prompt.
pub fn permission_granted() -> Result<bool> {
    let output = Command::new(helper_path()?)
        .arg("--check-permission")
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "granted")
}

/// Asks macOS to show the permission prompt, and reports the answer.
///
/// macOS shows this once per code identity. If it has already been answered, this
/// returns the stored answer immediately and the user has to change it in System
/// Settings by hand.
pub fn request_permission() -> Result<bool> {
    let output = Command::new(helper_path()?)
        .args(["--check-permission", "--request"])
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "granted")
}

/// A running system-audio capture.
pub struct HelperSource {
    child: Child,
    format: AudioFormat,
    audio: Receiver<Vec<f32>>,
    events: Arc<Mutex<Vec<HelperEvent>>>,
    status: Arc<Mutex<HelperStatus>>,
    nonzero_samples: Arc<AtomicU64>,
    finished: Arc<AtomicBool>,
    readers: Vec<JoinHandle<()>>,
    stopped: bool,
}

impl HelperSource {
    /// Spawns the helper and blocks until it reports the negotiated format.
    ///
    /// Returns an error rather than a silent source if permission is missing, the target
    /// process is gone, or the helper dies during the handshake.
    pub fn start(target: TapTarget) -> Result<Self> {
        let path = helper_path()?;

        let mut child = Command::new(&path)
            .arg("--capture")
            .args(target.to_args())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()?;

        let stdout = child.stdout.take().ok_or(AudioError::HelperNoFormat)?;
        let stderr = child.stderr.take().ok_or(AudioError::HelperNoFormat)?;

        let events = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(Mutex::new(HelperStatus::default()));
        let nonzero_samples = Arc::new(AtomicU64::new(0));
        let finished = Arc::new(AtomicBool::new(false));

        let (handshake_tx, handshake_rx) = sync_channel::<HelperEvent>(4);
        let stderr_thread = spawn_stderr_reader(
            stderr,
            Arc::clone(&events),
            Arc::clone(&status),
            handshake_tx,
        );

        // Wait for the format line, an error, or the helper's death.
        let format = match handshake_rx.recv_timeout(HANDSHAKE_TIMEOUT) {
            Ok(HelperEvent::Format {
                sample_rate,
                channels,
            }) => AudioFormat::new(sample_rate, channels),
            Ok(HelperEvent::Error { kind, message }) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(classify_error(&kind, &message));
            }
            Ok(_) | Err(RecvTimeoutError::Disconnected) | Err(RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                let status_code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
                let collected = events
                    .lock()
                    .map(|events| describe(&events))
                    .unwrap_or_default();
                let _ = stderr_thread.join();
                return Err(match status_code {
                    77 => AudioError::PermissionDenied,
                    69 => AudioError::NoSuchProcess,
                    _ if collected.is_empty() => AudioError::HelperNoFormat,
                    _ => AudioError::HelperFailed {
                        status: status_code,
                        stderr: collected,
                    },
                });
            }
        };

        let (audio_tx, audio_rx) = sync_channel::<Vec<f32>>(CHANNEL_DEPTH);
        let stdout_thread = spawn_stdout_reader(
            stdout,
            audio_tx,
            Arc::clone(&nonzero_samples),
            Arc::clone(&finished),
        );

        Ok(Self {
            child,
            format,
            audio: audio_rx,
            events,
            status,
            nonzero_samples,
            finished,
            readers: vec![stderr_thread, stdout_thread],
            stopped: false,
        })
    }

    /// A snapshot of the live level, for a meter.
    pub fn status(&self) -> HelperStatus {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default()
    }

    /// Everything the helper has said so far, drained.
    pub fn drain_events(&self) -> Vec<HelperEvent> {
        match self.events.lock() {
            Ok(mut events) => std::mem::take(&mut *events),
            Err(_) => Vec::new(),
        }
    }

    /// True once the helper reported capturing zeros while audio was provably playing.
    pub fn is_silently_failing(&self) -> bool {
        self.status
            .lock()
            .map(|status| status.silent_while_audio_playing)
            .unwrap_or(false)
    }

    pub fn nonzero_samples(&self) -> u64 {
        self.nonzero_samples.load(Ordering::Relaxed)
    }
}

impl AudioSource for HelperSource {
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

    /// SIGTERM, not SIGKILL. The helper's handler stops the device and destroys the tap
    /// and aggregate device in order; killing it outright would leak both until the
    /// process table catches up.
    fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;

        // Safety: sending SIGTERM to a pid we own. The child cannot have been reaped
        // yet — `wait` is only called below, and only from here.
        #[allow(clippy::cast_possible_wrap)]
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
        }

        let deadline = Instant::now() + SHUTDOWN_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    tracing::warn!("audio helper ignored SIGTERM; killing it");
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(error) => return Err(AudioError::Io(error)),
            }
        }

        self.finished.store(true, Ordering::Relaxed);
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
        Ok(())
    }

    fn has_produced_audio(&self) -> bool {
        self.nonzero_samples() > 0
    }
}

impl Drop for HelperSource {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Maps the helper's error vocabulary onto our own.
fn classify_error(kind: &str, message: &str) -> AudioError {
    match kind {
        "permission_denied" => AudioError::PermissionDenied,
        "no_such_process" => AudioError::NoSuchProcess,
        _ => AudioError::HelperFailed {
            status: 0,
            stderr: message.to_string(),
        },
    }
}

fn describe(events: &[HelperEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            HelperEvent::Log { line } => Some(line.clone()),
            HelperEvent::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Turns each stderr line into an event. Lines that are not JSON are kept verbatim as
/// [`HelperEvent::Log`] rather than dropped — a diagnostic nobody reads is no better
/// than no diagnostic.
fn spawn_stderr_reader(
    stderr: std::process::ChildStderr,
    events: Arc<Mutex<Vec<HelperEvent>>>,
    status: Arc<Mutex<HelperStatus>>,
    handshake: SyncSender<HelperEvent>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("hearsay-helper-stderr".into())
        .spawn(move || {
            let reader = BufReader::new(stderr);
            let mut handshake = Some(handshake);

            for line in reader.lines() {
                let Ok(line) = line else { break };
                let event = parse_event(&line);

                match &event {
                    HelperEvent::Format { .. } | HelperEvent::Error { .. } => {
                        if let Some(sender) = handshake.take() {
                            let _ = sender.send(event.clone());
                        }
                    }
                    HelperEvent::Level {
                        peak,
                        rms,
                        frames,
                        interval_nonzero_samples,
                    } => {
                        if let Ok(mut status) = status.lock() {
                            status.peak = *peak;
                            status.rms = *rms;
                            status.frames = *frames;
                            // Real audio in the last window clears the alarm. The
                            // longest-outage tally stays, so the recording can still be
                            // judged afterwards.
                            if *interval_nonzero_samples > 0 {
                                status.silent_while_audio_playing = false;
                                status.silent_seconds = 0.0;
                                status.captured_audio_earlier = true;
                            }
                        }
                    }
                    HelperEvent::PermissionWarning { message } => {
                        tracing::warn!("{message}");
                    }
                    HelperEvent::Silence {
                        message,
                        silent_seconds,
                        captured_audio_earlier,
                        ..
                    } => {
                        tracing::error!("audio helper reports silence: {message}");
                        if let Ok(mut status) = status.lock() {
                            status.silent_while_audio_playing = true;
                            status.silent_seconds = *silent_seconds;
                            status.captured_audio_earlier = *captured_audio_earlier;
                            status.longest_silence_seconds =
                                status.longest_silence_seconds.max(*silent_seconds);
                        }
                    }
                    _ => {}
                }

                if let Ok(mut events) = events.lock() {
                    events.push(event);
                }
            }
        })
        .expect("spawning a named thread cannot fail on macOS")
}

fn parse_event(line: &str) -> HelperEvent {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') {
        return HelperEvent::Log {
            line: trimmed.to_string(),
        };
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return HelperEvent::Log {
            line: trimmed.to_string(),
        };
    };

    let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let number = |key: &str| value.get(key).and_then(serde_json::Value::as_f64);
    let text = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let flag = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };

    match kind {
        "format" => HelperEvent::Format {
            sample_rate: number("sample_rate").unwrap_or(0.0) as u32,
            channels: number("channels").unwrap_or(0.0) as u16,
        },
        "started" => HelperEvent::Started,
        "level" => HelperEvent::Level {
            peak: number("peak").unwrap_or(0.0) as f32,
            rms: number("rms").unwrap_or(0.0),
            frames: number("frames").unwrap_or(0.0) as u64,
            interval_nonzero_samples: number("interval_nonzero_samples").unwrap_or(0.0) as u64,
        },
        "silence" => HelperEvent::Silence {
            elapsed_seconds: number("elapsed_seconds").unwrap_or(0.0),
            silent_seconds: number("silent_seconds").unwrap_or(0.0),
            captured_audio_earlier: flag("captured_audio_earlier"),
            message: text("message"),
        },
        "stopped" => HelperEvent::Stopped {
            reason: text("reason"),
            frames: number("frames").unwrap_or(0.0) as u64,
            nonzero_samples: number("nonzero_samples").unwrap_or(0.0) as u64,
            silent_while_playing_seconds: number("silent_while_playing_seconds").unwrap_or(0.0),
            ended_silent: flag("ended_silent"),
        },
        "error" => HelperEvent::Error {
            kind: text("kind"),
            message: text("message"),
        },
        "permission_warning" => HelperEvent::PermissionWarning {
            message: text("message"),
        },
        _ => HelperEvent::Log {
            line: trimmed.to_string(),
        },
    }
}

/// Drains stdout into sample buffers, counting non-zero samples as it goes.
///
/// The helper writes whole buffers, but a pipe read can land mid-sample, so any trailing
/// bytes are carried into the next read. Dropping them would shift every subsequent
/// sample by a byte and turn the recording into noise.
fn spawn_stdout_reader(
    mut stdout: std::process::ChildStdout,
    audio: SyncSender<Vec<f32>>,
    nonzero_samples: Arc<AtomicU64>,
    finished: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("hearsay-helper-stdout".into())
        .spawn(move || {
            let mut buffer = vec![0u8; 64 * 1024];
            let mut carry: Vec<u8> = Vec::with_capacity(4);

            loop {
                let read = match stdout.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        tracing::warn!("audio helper stdout ended: {error}");
                        break;
                    }
                };

                let mut bytes: &[u8] = &buffer[..read];
                let mut samples: Vec<f32> = Vec::with_capacity((carry.len() + read) / 4);

                if !carry.is_empty() {
                    let needed = 4 - carry.len();
                    if bytes.len() < needed {
                        carry.extend_from_slice(bytes);
                        continue;
                    }
                    carry.extend_from_slice(&bytes[..needed]);
                    samples.push(f32::from_le_bytes([carry[0], carry[1], carry[2], carry[3]]));
                    carry.clear();
                    bytes = &bytes[needed..];
                }

                let whole = bytes.len() - (bytes.len() % 4);
                for chunk in bytes[..whole].chunks_exact(4) {
                    samples.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                }
                carry.extend_from_slice(&bytes[whole..]);

                let counted = samples.iter().filter(|sample| **sample != 0.0).count() as u64;
                if counted > 0 {
                    nonzero_samples.fetch_add(counted, Ordering::Relaxed);
                }

                if audio.send(samples).is_err() {
                    // The consumer hung up; nothing left to deliver to.
                    break;
                }
                if finished.load(Ordering::Relaxed) {
                    break;
                }
            }
        })
        .expect("spawning a named thread cannot fail on macOS")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this guard was rebuilt for: a level report carries the *windowed*
    /// count, so a tap that has gone quiet is visible even after it produced audio
    /// earlier in the run.
    #[test]
    fn a_level_report_carries_the_windowed_nonzero_count() {
        let event = parse_event(
            r#"{"type":"level","peak":0.0,"rms":0.0,"frames":48000,
                "nonzero_samples":144000,"interval_nonzero_samples":0}"#,
        );
        match event {
            HelperEvent::Level {
                interval_nonzero_samples,
                ..
            } => assert_eq!(interval_nonzero_samples, 0),
            other => panic!("expected a level event, got {other:?}"),
        }
    }

    /// A tap that worked and then stopped must not be reported as one that never
    /// started: they send the reader to different places to fix it.
    #[test]
    fn a_silence_report_says_whether_the_tap_ever_worked() {
        let event = parse_event(
            r#"{"type":"silence","elapsed_seconds":600.0,"silent_seconds":57.0,
                "captured_audio_earlier":true,"message":"stopped"}"#,
        );
        match event {
            HelperEvent::Silence {
                silent_seconds,
                captured_audio_earlier,
                ..
            } => {
                assert_eq!(silent_seconds, 57.0);
                assert!(captured_audio_earlier);
            }
            other => panic!("expected a silence event, got {other:?}"),
        }
    }

    /// An older helper emits neither field. Recording must survive the mismatch rather
    /// than refusing to parse the line at all.
    #[test]
    fn a_helper_without_the_new_fields_still_parses() {
        let event = parse_event(r#"{"type":"silence","elapsed_seconds":5.0,"message":"only zeros"}"#);
        match event {
            HelperEvent::Silence {
                silent_seconds,
                captured_audio_earlier,
                ..
            } => {
                assert_eq!(silent_seconds, 0.0);
                assert!(!captured_audio_earlier);
            }
            other => panic!("expected a silence event, got {other:?}"),
        }
    }

    #[test]
    fn a_stopped_report_carries_the_silence_tally() {
        let event = parse_event(
            r#"{"type":"stopped","reason":"signal","frames":1,"nonzero_samples":2,
                "silent_while_playing_seconds":941.0,"ended_silent":true}"#,
        );
        match event {
            HelperEvent::Stopped {
                silent_while_playing_seconds,
                ended_silent,
                ..
            } => {
                assert_eq!(silent_while_playing_seconds, 941.0);
                assert!(ended_silent);
            }
            other => panic!("expected a stopped event, got {other:?}"),
        }
    }

    #[test]
    fn process_targets_become_repeated_pid_flags() {
        let args = TapTarget::Processes(vec![10, 20]).to_args();
        assert_eq!(args, vec!["--pid", "10", "--pid", "20"]);
    }

    #[test]
    fn system_wide_is_a_single_flag() {
        assert_eq!(TapTarget::SystemWide.to_args(), vec!["--system"]);
    }

    #[test]
    fn format_lines_are_parsed() {
        let event = parse_event(r#"{"type":"format","sample_rate":48000,"channels":2}"#);
        match event {
            HelperEvent::Format {
                sample_rate,
                channels,
            } => {
                assert_eq!(sample_rate, 48_000);
                assert_eq!(channels, 2);
            }
            other => panic!("expected a format event, got {other:?}"),
        }
    }

    #[test]
    fn non_json_stderr_is_kept_rather_than_dropped() {
        match parse_event("tapping system-wide output") {
            HelperEvent::Log { line } => assert_eq!(line, "tapping system-wide output"),
            other => panic!("expected a log event, got {other:?}"),
        }
    }

    #[test]
    fn silence_reports_survive_parsing() {
        let event = parse_event(
            r#"{"type":"silence","elapsed_seconds":5.0,"message":"only zeros"}"#,
        );
        match event {
            HelperEvent::Silence { message, .. } => assert_eq!(message, "only zeros"),
            other => panic!("expected a silence event, got {other:?}"),
        }
    }
}
