//! Running transcription in the background and reporting progress.
//!
//! Transcription is slow — minutes for a long meeting — and entirely detached from the
//! recording that produced it. It runs on its own thread, reports progress as events,
//! and writes segments when it finishes. Nothing about the app is blocked while it runs,
//! and a failure leaves the recording itself untouched.
//!
//! **One pass at a time.** Each pass runs a `faster-whisper` process that will use every
//! core it can get. Two of them alongside a live recording starve the writer thread, and
//! audio the mixer has to drop is gone with no marker in the transcript — a worse outcome
//! than a transcript arriving later. So passes queue, oldest first.

use crate::state::{AppState, CommandError, CommandResult};
use chrono::Utc;
use hearsay_audio::Mode;
use hearsay_core::db::NewSegment;
use hearsay_core::transcribe::{transcribe_recording, TranscribeEvent, DEFAULT_MODEL};
use hearsay_core::Database;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use tauri::{AppHandle, Emitter, State};

/// A ticket queue: threads are served in the order they arrive.
///
/// Order matters because the recording the user is looking at is usually the one they
/// just made. A plain mutex would let a later pass jump ahead of it.
struct Queue {
    state: Mutex<Tickets>,
    served: Condvar,
}

struct Tickets {
    next: u64,
    serving: u64,
}

static QUEUE: Queue = Queue {
    state: Mutex::new(Tickets { next: 0, serving: 0 }),
    served: Condvar::new(),
};

impl Queue {
    /// A poisoned lock here guards two counters, not data that can be left half-written,
    /// so recovering is safe and far better than wedging transcription for the session.
    fn tickets(&self) -> MutexGuard<'_, Tickets> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Joins the queue. Returns the ticket and how many passes are ahead of it.
    fn join(&self) -> (u64, u64) {
        let mut tickets = self.tickets();
        let ticket = tickets.next;
        tickets.next += 1;
        (ticket, ticket - tickets.serving)
    }

    /// Blocks until this ticket's turn. The returned guard yields the turn when dropped.
    fn wait_for(&self, ticket: u64) -> Turn {
        let mut tickets = self.tickets();
        while tickets.serving != ticket {
            tickets = self
                .served
                .wait(tickets)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        Turn
    }
}

/// Holds the queue's turn. Yields it on drop, so a panicking pass cannot wedge the queue.
struct Turn;

impl Drop for Turn {
    fn drop(&mut self) {
        QUEUE.tickets().serving += 1;
        QUEUE.served.notify_all();
    }
}

/// Recordings with a transcription pass queued or running.
///
/// The ticket queue above orders passes but does not say whose they are, and something
/// has to: a pass reads the WAV from beginning to end, so deleting that file while it is
/// in flight turns a working transcription into an `unreadable_audio` failure partway
/// through. Refusing up front explains the situation; letting it race produces a
/// half-finished pass and an error about a file the user just asked to remove.
fn in_flight() -> &'static Mutex<HashSet<i64>> {
    static IN_FLIGHT: OnceLock<Mutex<HashSet<i64>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

/// A poisoned lock here guards a set of ids, not data that can be left half-written, so
/// recovering is safe and better than wedging every later check for the session.
fn in_flight_set() -> MutexGuard<'static, HashSet<i64>> {
    in_flight()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether a transcription pass for this recording is queued or running.
pub fn is_transcribing(event_id: i64) -> bool {
    in_flight_set().contains(&event_id)
}

/// Marks a recording as having a pass in flight, clearing it on drop.
///
/// Dropped rather than cleared by hand so a failing or panicking pass cannot leave a
/// recording marked busy for the rest of the session — which would make its audio
/// undeletable with no way to explain why.
struct InFlight(i64);

impl InFlight {
    fn mark(event_id: i64) -> Self {
        in_flight_set().insert(event_id);
        Self(event_id)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        in_flight_set().remove(&self.0);
    }
}

/// Starts transcription on a worker thread, queued behind any pass already running.
pub fn spawn_transcription(
    app: AppHandle,
    db: Arc<Database>,
    event_id: i64,
    audio_path: PathBuf,
    mode: Mode,
) {
    // Marked here rather than inside the thread: `retranscribe` returning has to mean the
    // pass is already accounted for, or a delete arriving immediately after would slip
    // through the gap before the worker started.
    let busy = InFlight::mark(event_id);

    std::thread::Builder::new()
        .name(format!("hearsay-transcribe-{event_id}"))
        .spawn(move || {
            let _busy = busy;
            let (ticket, ahead) = QUEUE.join();
            if ahead > 0 {
                // Say so, or the pane sits blank and the transcription looks lost.
                tracing::info!("transcription of {event_id} is queued behind {ahead} pass(es)");
                let _ = app.emit(
                    "transcription",
                    serde_json::json!({
                        "event_id": event_id,
                        "stage": "queued",
                        "ahead": ahead,
                    }),
                );
            }
            let _turn = QUEUE.wait_for(ticket);

            if let Err(error) = run(&app, &db, event_id, &audio_path, mode) {
                tracing::error!("transcription of {event_id} failed: {error:#}");
                let _ = app.emit(
                    "transcription",
                    serde_json::json!({
                        "event_id": event_id,
                        "stage": "failed",
                        "message": format!("{error:#}"),
                    }),
                );
            }
        })
        .map(|_| ())
        .unwrap_or_else(|error| {
            tracing::error!("could not start the transcription thread: {error}");
        });
}

fn run(
    app: &AppHandle,
    db: &Database,
    event_id: i64,
    audio_path: &std::path::Path,
    mode: Mode,
) -> anyhow::Result<()> {
    let models_dir = hearsay_core::paths::models_dir()?;

    let _ = app.emit(
        "transcription",
        serde_json::json!({ "event_id": event_id, "stage": "started" }),
    );

    let segments = transcribe_recording(audio_path, mode, DEFAULT_MODEL, &models_dir, |event| {
        let payload = match &event {
            TranscribeEvent::Download { file, percent } => serde_json::json!({
                "event_id": event_id, "stage": "downloading",
                "file": file, "percent": percent,
            }),
            TranscribeEvent::DownloadDone | TranscribeEvent::ModelReady => serde_json::json!({
                "event_id": event_id, "stage": "model_ready",
            }),
            TranscribeEvent::Progress { channel, percent } => serde_json::json!({
                "event_id": event_id, "stage": "transcribing",
                "channel": channel, "percent": percent,
            }),
            TranscribeEvent::Done { channel, segments } => serde_json::json!({
                "event_id": event_id, "stage": "channel_done",
                "channel": channel, "segments": segments,
            }),
            TranscribeEvent::Error { kind, message } => serde_json::json!({
                "event_id": event_id, "stage": "failed",
                "kind": kind, "message": message,
            }),
            TranscribeEvent::Log { line } => {
                tracing::debug!("transcribe: {line}");
                return;
            }
        };
        let _ = app.emit("transcription", payload);
    })?;

    // Drop microphone lines that are echoes of the other party. Only meaningful in
    // conversation mode; a listen-only transcript has one channel and nothing to compare.
    let (segments, echoes_dropped) = if mode.opens_microphone() {
        hearsay_core::dedupe::drop_echoed_segments(segments)
    } else {
        (segments, 0)
    };
    if echoes_dropped > 0 {
        tracing::info!("dropped {echoes_dropped} echoed mic segment(s) from {event_id}");
    }

    let rows: Vec<NewSegment> = segments
        .into_iter()
        .map(|segment| NewSegment {
            channel: segment.channel,
            start_ms: segment.start_ms,
            end_ms: segment.end_ms,
            text: segment.text,
        })
        .collect();

    let count = rows.len();
    db.replace_segments(event_id, &rows)?;
    // Marked even when `rows` is empty: a recording with nothing to transcribe has been
    // transcribed, and must not be picked up by recovery on every launch from now on.
    db.mark_transcribed(event_id, Utc::now())?;

    let _ = app.emit(
        "transcription",
        serde_json::json!({
            "event_id": event_id, "stage": "done", "segments": count,
            "echoes_dropped": echoes_dropped,
        }),
    );
    Ok(())
}

/// Re-runs transcription for a recording that already exists.
///
/// Useful after setting up the Python sidecar for the first time, or when a recording
/// was made before it was installed.
#[tauri::command]
pub fn retranscribe(
    app: AppHandle,
    state: State<'_, AppState>,
    event_id: i64,
) -> CommandResult<()> {
    let event = state.db.event(event_id)?.ok_or_else(|| CommandError {
        message: format!("no recording with id {event_id}"),
    })?;

    // Deleted audio is a different situation from audio that never arrived, and the
    // difference is the whole point of saying so: one is a fault to look into, the other
    // is a choice the user made and the reason this can never work again.
    if event.audio_was_deleted() {
        return Err(CommandError {
            message: "the audio for this recording was deleted, so it cannot be \
                      transcribed again. The transcript it produced is still here."
                .to_string(),
        });
    }

    let path = event.audio_path.clone().ok_or_else(|| CommandError {
        message: "this recording has no audio file".to_string(),
    })?;
    let path = PathBuf::from(path);
    if !path.is_file() {
        return Err(CommandError {
            message: format!("the audio file is missing: {}", path.display()),
        });
    }

    let mode = if event.mode == "conversation" {
        Mode::Conversation
    } else {
        Mode::ListenOnly
    };

    spawn_transcription(app, state.db.clone(), event_id, path, mode);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// The queue is a process-wide static, and the test harness runs tests in parallel.
    /// Two tests interleaving their tickets would deadlock waiting on each other's turns,
    /// so they take this first. Each leaves the queue balanced for the next.
    static ONE_TEST_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn exclusive() -> MutexGuard<'static, ()> {
        ONE_TEST_AT_A_TIME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn passes_run_one_at_a_time_in_the_order_they_arrived() {
        let _exclusive = exclusive();

        let (first, ahead_first) = QUEUE.join();
        let (second, ahead_second) = QUEUE.join();
        let (third, _) = QUEUE.join();

        assert_eq!(ahead_first, 0, "the first pass should not have to wait");
        assert_eq!(ahead_second, 1, "the second should report one pass ahead of it");

        let (finished_tx, finished_rx) = mpsc::channel();

        // Start the later two first, so passing only proves the queue ordered them and
        // not that they happened to be spawned in order.
        let handles: Vec<_> = [(third, 3u8), (second, 2)]
            .into_iter()
            .map(|(ticket, label)| {
                let finished_tx = finished_tx.clone();
                std::thread::spawn(move || {
                    let _turn = QUEUE.wait_for(ticket);
                    let _ = finished_tx.send(label);
                })
            })
            .collect();

        // Nothing can proceed while the first ticket holds its turn.
        assert!(
            finished_rx.recv_timeout(std::time::Duration::from_millis(50)).is_err(),
            "a queued pass ran while an earlier one still held its turn"
        );

        {
            let _turn = QUEUE.wait_for(first);
            let _ = finished_tx.send(1);
        }

        for handle in handles {
            let _ = handle.join();
        }

        let order: Vec<u8> = (0..3)
            .filter_map(|_| finished_rx.recv_timeout(std::time::Duration::from_secs(5)).ok())
            .collect();
        assert_eq!(order, vec![1, 2, 3], "passes should be served oldest first");
    }

    /// A pass that panics must not wedge every later transcription for the session.
    #[test]
    fn a_panicking_pass_still_yields_its_turn() {
        let _exclusive = exclusive();

        let (ticket, _) = QUEUE.join();
        let (next, _) = QUEUE.join();

        let panicked = std::thread::spawn(move || {
            let _turn = QUEUE.wait_for(ticket);
            panic!("transcription fell over");
        })
        .join();
        assert!(panicked.is_err(), "the test needs the pass to have panicked");

        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _turn = QUEUE.wait_for(next);
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "the queue never recovered from a panicking pass"
        );
    }

    // ---- knowing which recordings have a pass in flight ----

    #[test]
    fn a_recording_is_busy_only_while_its_pass_is_in_flight() {
        let _exclusive = exclusive();

        assert!(!is_transcribing(4_101), "nothing should be busy to begin with");
        {
            let _busy = InFlight::mark(4_101);
            assert!(is_transcribing(4_101));
            assert!(
                !is_transcribing(4_102),
                "one recording being busy must not implicate another"
            );
        }
        assert!(
            !is_transcribing(4_101),
            "the mark must clear when the pass finishes"
        );
    }

    /// The reason this is a drop guard. A pass that falls over must not leave its
    /// recording marked busy for the rest of the session — the audio would be
    /// undeletable, with nothing to explain why.
    #[test]
    fn a_panicking_pass_stops_being_busy() {
        let _exclusive = exclusive();

        let panicked = std::thread::spawn(|| {
            let _busy = InFlight::mark(4_103);
            assert!(is_transcribing(4_103));
            panic!("transcription fell over");
        })
        .join();
        assert!(panicked.is_err(), "the test needs the pass to have panicked");

        assert!(
            !is_transcribing(4_103),
            "a recording stayed busy after its pass died"
        );
    }
}
