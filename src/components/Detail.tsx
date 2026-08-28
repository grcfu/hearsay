import { useCallback, useEffect, useRef, useState } from "react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ask, save } from "@tauri-apps/plugin-dialog";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import { Transcript } from "./Transcript";
import { AskTab } from "./AskTab";
import {
  formatBytes,
  formatClock,
  formatDuration,
  formatMode,
  formatTime,
  parseClock,
} from "../format";
import type {
  ExportedAudio,
  HearsayEvent,
  CaptureSpan,
  MuteSpan,
  ReclaimedAudio,
  Segment,
  Settings,
} from "../types";

interface EventDetail {
  event: HearsayEvent;
  segments: Segment[];
  mute_spans: MuteSpan[];
  capture_spans: CaptureSpan[];
}

interface TranscriptionEvent {
  event_id: number;
  stage: string;
  percent?: number;
  channel?: string;
  message?: string;
  segments?: number;
  /** How many passes are queued ahead of this one. Only on the "queued" stage. */
  ahead?: number;
  /** One decoded line, on the "segment" stage. Not stored yet — see `live` below. */
  segment?: Omit<Segment, "id" | "event_id">;
}

type Tab = "summary" | "transcript" | "ask" | "audio";

const TAB_LABELS: Record<Tab, string> = {
  summary: "Summary",
  transcript: "Transcript",
  ask: "Ask",
  audio: "Audio",
};

interface Props {
  eventId: number | null;
  seekMs: number | null;
  onChanged: () => void;
}

/** How long a summary must run before finishing it is worth a notification. Below this
 *  the window was almost certainly still being watched, and a banner for something the
 *  user just saw happen is noise. */
const NOTIFY_AFTER_MS = 20_000;

/** How long before the wait is worth explaining rather than just counting. */
const SLOW_AFTER_MS = 45_000;

/** Sends a desktop notification, asking for permission the first time.
 *
 *  Failure is survivable and deliberately silent: the pane says the same thing on screen,
 *  and a recorder that nags about notification permission it does not need is worse than
 *  one that quietly does without. */
async function notify(title: string, body: string) {
  try {
    let allowed = await isPermissionGranted();
    if (!allowed) allowed = (await requestPermission()) === "granted";
    if (allowed) sendNotification({ title, body });
  } catch {
    // Nothing to do — the window already says it.
  }
}

/**
 * What a summary pass can honestly report.
 *
 * Not a percentage, and not a bar. A summary is one request: it is sent, and then either
 * an answer comes back or it does not. There is no intermediate signal to draw a bar
 * from, and drawing one anyway would be inventing a position — the same class of lie as a
 * seek button that does nothing. Transcription has a real bar because it decodes a known
 * number of seconds of audio and says how far it has got.
 *
 * So what is shown is what is true: that it is still going, for how long, and — when a
 * provider is shedding load — that it is being waited out and for how much longer.
 */
function SummaryProgress({ run }: { run: SummaryRun | null }) {
  if (run === null) return null;
  const elapsed = Date.now() - run.startedAt;

  return (
    <div className="banner" style={{ marginBottom: 14 }}>
      <span>
        Writing the summary… <span className="mono">{formatClock(elapsed)}</span>
        {run.waiting ? (
          <>
            {" "}
            The provider is busy; trying again in {run.waiting.seconds}s — attempt{" "}
            {run.waiting.attempt + 1} of {run.waiting.of}.
          </>
        ) : elapsed >= SLOW_AFTER_MS ? (
          <>
            {" "}
            Still waiting on the provider. There is no progress to report from a single
            request — you can leave this tab, and Hearsay will notify you when it lands.
          </>
        ) : null}
      </span>
    </div>
  );
}

/** A summary pass in flight. */
interface SummaryRun {
  /** `Date.now()` when the press happened, so the elapsed time is real rather than a tick
   *  count that stalls whenever the tab is hidden. */
  startedAt: number;
  /** Set while a busy provider is being waited out (§8a), so the delay is explained. */
  waiting: { attempt: number; of: number; seconds: number } | null;
}

/**
 * Pane three: one recording, with its summary, transcript, and audio as peer tabs.
 *
 * Peers rather than a hierarchy — none of the three is the "real" view. Someone
 * skimming wants the summary, someone checking a quote wants the transcript, and someone
 * who needs to hear the tone wants the audio.
 */
export function Detail({ eventId, seekMs, onChanged }: Props) {
  const [detail, setDetail] = useState<EventDetail | null>(null);
  const [tab, setTab] = useState<Tab>("transcript");
  const [progress, setProgress] = useState<TranscriptionEvent | null>(null);
  // Lines decoded by a pass that has not finished. Held here rather than in the database:
  // the stored transcript is only replaced once the whole pass succeeds, so a pass that
  // fails leaves the previous one intact. Shown in place of the stored rows while a pass
  // runs, which is what will happen to them for real when it finishes.
  const [live, setLive] = useState<Segment[]>([]);
  const [error, setError] = useState<string | null>(null);
  // The summary pass lives here rather than inside the tab, for the reason the
  // transcription pass does: the tabs unmount when another is selected, and a listener
  // that goes with them misses the result. A summary started and then looked away from
  // used to finish into nothing — no refresh, no state, nothing to say it had ever run.
  const [summaryRun, setSummaryRun] = useState<SummaryRun | null>(null);
  const [summaryError, setSummaryError] = useState<string | null>(null);
  const [playheadMs, setPlayheadMs] = useState<number | null>(null);
  const [settings, setSettings] = useState<Settings | null>(null);
  const audioRef = useRef<HTMLAudioElement | null>(null);
  /** When the running pass started. A ref because the listener below reads it from a
   *  closure made once per subscription, where the state would always be its initial
   *  value — and an elapsed time of zero would silence the notification entirely. */
  const startedAtRef = useRef<number | null>(null);

  // Which provider is configured, and what to call the recorder. Read once: both are
  // changed from Settings, which is a different view entirely.
  useEffect(() => {
    void invoke<Settings>("settings")
      .then(setSettings)
      .catch(() => undefined);
  }, []);

  const load = useCallback(async () => {
    if (eventId === null) {
      setDetail(null);
      return;
    }
    try {
      setDetail(await invoke<EventDetail>("event_detail", { eventId }));
      setError(null);
    } catch (problem) {
      setError(String((problem as { message?: string })?.message ?? problem));
    }
  }, [eventId]);

  useEffect(() => {
    void load();
    setProgress(null);
    setLive([]);
  }, [load]);

  // Transcription runs long after the recording stops, so the detail pane refreshes
  // itself when it finishes rather than making the user go and come back.
  useEffect(() => {
    const unlisten = listen<TranscriptionEvent>("transcription", (message) => {
      const payload = message.payload;
      if (payload.event_id !== eventId) return;

      // A decoded line, arriving while the pass runs. Deliberately does not touch
      // `progress`: the percentage comes from its own events, which interleave with
      // these, and letting a line overwrite it would stall the bar between them.
      if (payload.stage === "segment") {
        if (payload.segment) {
          const decoded = payload.segment;
          // Negative ids, so nothing can mistake one of these for a stored row. They
          // exist only to key the list.
          setLive((shown) => [
            ...shown,
            { ...decoded, id: -(shown.length + 1), event_id: payload.event_id },
          ]);
        }
        return;
      }

      setProgress(payload);
      // A pass starting over discards whatever the last one had shown.
      if (payload.stage === "started") setLive([]);
      if (payload.stage === "done") {
        void load();
        onChanged();
      }
    });
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, [eventId, load, onChanged]);

  // The summary pass, listened for here for the same reason: it outlives the tab.
  useEffect(() => {
    const unlisten = listen<{
      event_id: number;
      stage: string;
      message?: string;
      attempt?: number;
      of?: number;
      seconds?: number;
    }>("summary", (message) => {
      const payload = message.payload;
      if (payload.event_id !== eventId) return;

      if (payload.stage === "started") {
        setSummaryError(null);
        startedAtRef.current = Date.now();
        setSummaryRun({ startedAt: startedAtRef.current, waiting: null });
        return;
      }
      if (payload.stage === "waiting") {
        setSummaryRun((run) =>
          run === null
            ? run
            : {
                ...run,
                waiting: {
                  attempt: payload.attempt ?? 1,
                  of: payload.of ?? 1,
                  seconds: payload.seconds ?? 0,
                },
              },
        );
        return;
      }
      if (payload.stage === "done" || payload.stage === "failed") {
        const failed = payload.stage === "failed";
        // Only when it ran long enough to have been looked away from. A notification for
        // something that finished while the window was still being watched is noise.
        const startedAt = startedAtRef.current;
        const elapsed = startedAt === null ? 0 : Date.now() - startedAt;
        startedAtRef.current = null;
        if (elapsed >= NOTIFY_AFTER_MS) {
          void notify(
            failed ? "The summary could not be written" : "The summary is ready",
            failed
              ? (payload.message ?? "Open Hearsay to see what happened.")
              : (detail?.event.title ?? "Open Hearsay to read it."),
          );
        }
        setSummaryRun(null);
        if (failed) setSummaryError(payload.message ?? "Summary failed.");
        else {
          void load();
          onChanged();
        }
      }
    });
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, [eventId, load, onChanged, detail?.event.title]);

  // Ticks once a second while a pass runs, so the elapsed time on screen moves. Nothing
  // subscribes when nothing is running.
  const [, setTick] = useState(0);
  useEffect(() => {
    if (summaryRun === null) return;
    const timer = window.setInterval(() => setTick((count) => count + 1), 1000);
    return () => window.clearInterval(timer);
  }, [summaryRun]);

  const seek = useCallback((ms: number) => {
    const audio = audioRef.current;
    if (!audio) return;
    audio.currentTime = ms / 1000;
    void audio.play();
  }, []);

  // A pass belongs to the recording it was started for. Left in place, switching
  // recordings would show one recording's elapsed time against another's summary.
  useEffect(() => {
    startedAtRef.current = null;
    setSummaryRun(null);
    setSummaryError(null);
  }, [eventId]);

  // Arriving from a search result: open the transcript at the moment that was matched.
  useEffect(() => {
    if (seekMs === null || !detail) return;
    setTab("transcript");
    const timer = window.setTimeout(() => seek(seekMs), 60);
    return () => window.clearTimeout(timer);
  }, [seekMs, detail, seek]);

  if (eventId === null) {
    return (
      <div className="empty">
        <div>Nothing selected</div>
        <div className="small">Pick a recording to see its summary and transcript.</div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="empty">
        <div>Could not open this recording</div>
        <div className="small">{error}</div>
      </div>
    );
  }

  if (!detail) {
    return (
      <div className="empty">
        <div>Loading…</div>
      </div>
    );
  }

  const { event, segments, mute_spans: muteSpans, capture_spans: captureSpans } = detail;

  // While a pass is running, the lines it has decoded stand in for the stored ones — the
  // same substitution the pass will make for real when it finishes. If it fails instead,
  // this goes false and the previous transcript comes back, which is the truth: it is
  // still the only one in the database.
  const passRunning =
    progress !== null && progress.stage !== "done" && progress.stage !== "failed";
  const shownSegments = passRunning && live.length > 0 ? live : segments;
  const duration = event.ended_at
    ? new Date(event.ended_at).getTime() - new Date(event.started_at).getTime()
    : null;
  const audioSrc = event.audio_path ? convertFileSrc(event.audio_path) : null;

  const rename = async (title: string) => {
    if (title.trim() === event.title) return;
    try {
      await invoke("rename_event", { eventId: event.id, title });
      await load();
      onChanged();
    } catch (problem) {
      setError(String((problem as { message?: string })?.message ?? problem));
    }
  };

  const remove = async () => {
    const confirmed = await ask(
      `Delete "${event.title}"? The recording, its transcript, and its audio file are removed from this machine. This cannot be undone.`,
      { title: "Delete recording", kind: "warning", okLabel: "Delete", cancelLabel: "Keep" },
    );
    if (!confirmed) return;
    try {
      await invoke("delete_event", { eventId: event.id });
      onChanged();
    } catch (problem) {
      setError(String((problem as { message?: string })?.message ?? problem));
    }
  };

  return (
    <>
      <div className="detail-header">
        <input
          className="detail-title"
          defaultValue={event.title}
          key={`${event.id}-${event.title}`}
          aria-label="Recording title"
          onBlur={(changed) => void rename(changed.target.value)}
          onKeyDown={(pressed) => {
            if (pressed.key === "Enter") pressed.currentTarget.blur();
          }}
        />
        <div className="detail-meta">
          <span>{new Date(event.started_at).toLocaleDateString(undefined, {
            weekday: "long",
            month: "long",
            day: "numeric",
          })}</span>
          <span aria-hidden>·</span>
          <span>{formatTime(event.started_at)}</span>
          <span aria-hidden>·</span>
          <span>{formatDuration(duration)}</span>
          <span className="mode-badge">{formatMode(event.mode)}</span>
          <span className="spacer" />
          <button type="button" className="button destructive small" onClick={remove}>
            Delete
          </button>
        </div>

        <div className="tabs" role="tablist">
          {(["summary", "transcript", "ask", "audio"] as Tab[]).map((name) => (
            <button
              type="button"
              key={name}
              role="tab"
              aria-selected={tab === name}
              className={`tab${tab === name ? " active" : ""}`}
              onClick={() => setTab(name)}
            >
              {TAB_LABELS[name]}
            </button>
          ))}
        </div>
      </div>

      <div className="detail-body">
        <TranscriptionProgress
          progress={progress}
          channels={event.mode === "conversation" ? 2 : 1}
          liveCount={passRunning ? live.length : 0}
        />

        {/* Above the tabs, not inside the summary one: a pass that is still running is
            worth knowing about from the transcript or the audio too. */}
        <SummaryProgress run={summaryRun} />

        {tab === "summary" ? (
          <SummaryTab
            event={event}
            segmentCount={segments.length}
            running={summaryRun !== null}
            error={summaryError}
            onError={setSummaryError}
            // The `started` event sets this too, but the press should show immediately
            // rather than after a round trip to the worker thread.
            onStart={() => {
              startedAtRef.current = Date.now();
              setSummaryRun({ startedAt: startedAtRef.current, waiting: null });
            }}
            onStop={() => {
              startedAtRef.current = null;
              setSummaryRun(null);
            }}
          />
        ) : tab === "transcript" ? (
          <Transcript
            segments={shownSegments}
            muteSpans={muteSpans}
            captureSpans={captureSpans}
            onSeek={seek}
            activeMs={playheadMs}
            speakerName={settings?.speaker_name}
            canSeek={audioSrc !== null}
          />
        ) : tab === "ask" ? (
          <AskTab
            eventId={event.id}
            segmentCount={segments.length}
            settings={settings}
            onSeek={seek}
            canSeek={audioSrc !== null}
          />
        ) : (
          <AudioTab
            src={audioSrc}
            audioRef={audioRef}
            onTime={setPlayheadMs}
            event={event}
            onChanged={() => {
              void load();
              onChanged();
            }}
          />
        )}

        {/* The player lives outside the tabs so playback survives switching to the
            transcript to read along. */}
        {audioSrc && tab !== "audio" ? (
          <audio
            ref={audioRef}
            src={audioSrc}
            preload="metadata"
            style={{ display: "none" }}
            onTimeUpdate={(changed) =>
              setPlayheadMs(changed.currentTarget.currentTime * 1000)
            }
          />
        ) : null}
      </div>
    </>
  );
}

/**
 * Where transcription has got to.
 *
 * Transcribing a long meeting takes minutes, and the previous version said only
 * "Transcribing…" — indistinguishable from being stuck. This shows how far along it is,
 * and for a conversation recording it accounts for both channels: the microphone pass
 * fills the first half of the bar and the system pass the second, so the bar tracks the
 * whole job rather than restarting halfway.
 *
 * The fill is sapphire. Gold means recording and nothing else.
 */
function TranscriptionProgress({
  progress,
  channels,
  liveCount,
}: {
  progress: TranscriptionEvent | null;
  channels: number;
  /** Lines decoded so far by the running pass. Zero when there is nothing to read yet. */
  liveCount: number;
}) {
  // How many channel passes have finished, so the bar keeps climbing across them.
  const [done, setDone] = useState(0);

  useEffect(() => {
    if (!progress) return;
    if (progress.stage === "started") setDone(0);
    if (progress.stage === "channel_done") setDone((n) => n + 1);
  }, [progress]);

  if (!progress || progress.stage === "done") return null;

  if (progress.stage === "failed") {
    return (
      <div className="banner problem">
        Transcription failed: {progress.message ?? "unknown error"}
      </div>
    );
  }

  const downloading = progress.stage === "downloading";
  const transcribing = progress.stage === "transcribing";
  const queued = progress.stage === "queued";

  // Downloads report their own percentage. Transcription reports per channel, so it is
  // scaled into the overall job.
  const percent = downloading
    ? (progress.percent ?? 0)
    : transcribing
      ? Math.min(100, ((done + (progress.percent ?? 0) / 100) / Math.max(channels, 1)) * 100)
      : null;

  // Queued deserves its own words. One transcription runs at a time, so a recording made
  // while an earlier one is still being transcribed waits — and a bar labelled
  // "Transcribing" that sat at zero for ten minutes would look broken rather than patient.
  const label = downloading
    ? "Downloading the speech model — this happens once"
    : progress.stage === "queued"
      ? (progress.ahead ?? 1) > 1
        ? `Waiting for ${progress.ahead} earlier transcriptions to finish`
        : "Waiting for the transcription before it to finish"
      : progress.stage === "started"
        ? "Getting ready"
        : progress.stage === "model_ready"
          ? "Model loaded, starting to listen"
          : transcribing
            ? channels > 1
              ? `Transcribing ${progress.channel === "left" ? "your side" : "their side"} (${Math.min(done + 1, channels)} of ${channels})`
              : "Transcribing"
            : "Finishing up";

  return (
    <div className="progress-card">
      <div className="progress-head">
        <span className="progress-label">{label}</span>
        {percent === null ? (
          <span className="progress-percent">{queued ? "in line" : "working…"}</span>
        ) : (
          <span className="progress-percent mono">{Math.round(percent)}%</span>
        )}
      </div>
      <div
        className={`progress-track${percent === null ? " indeterminate" : ""}`}
        role="progressbar"
        aria-valuenow={percent === null ? undefined : Math.round(percent)}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-label={label}
      >
        <div
          className="progress-fill"
          style={percent === null ? undefined : { width: `${percent}%` }}
        />
      </div>
      {/* Said out loud, because a transcript that grows while a progress bar climbs
          looks like it might be a partial view of a finished result. It is neither
          finished nor final: the echo pass at the end can still remove lines. */}
      {liveCount > 0 ? (
        <div className="progress-note">
          Readable in the transcript as it arrives — <span className="mono">{liveCount}</span>{" "}
          {liveCount === 1 ? "line" : "lines"} so far, and still being checked against the
          other channel.
        </div>
      ) : null}
    </div>
  );
}

function SummaryTab({
  event,
  segmentCount,
  running,
  error,
  onError,
  onStart,
  onStop,
}: {
  event: HearsayEvent;
  segmentCount: number;
  /** Owned by `Detail`, which listens for the pass — a listener inside this component
   *  would be torn down the moment another tab was selected, and the result missed. */
  running: boolean;
  error: string | null;
  onError: (message: string | null) => void;
  onStart: () => void;
  onStop: () => void;
}) {
  const [copied, setCopied] = useState(false);

  const generate = async () => {
    onError(null);
    onStart();
    try {
      await invoke("generate_summary", { eventId: event.id });
    } catch (problem) {
      // The pass never started, so nothing will arrive to end it. Clear it here or the
      // elapsed time counts up against a request that was never sent.
      onStop();
      onError(String((problem as { message?: string })?.message ?? problem));
    }
  };

  const canGenerate = segmentCount > 0;

  return (
    <div>
      {error ? (
        <div className="banner problem" style={{ marginBottom: 14 }}>
          {error}
        </div>
      ) : null}

      {event.summary_md ? (
        <>
          <div className="summary">{renderMarkdown(event.summary_md)}</div>
          <div className="row" style={{ marginTop: 24 }}>
            <button
              type="button"
              className="button"
              onClick={async () => {
                // Two flavours go on the clipboard at once, and the destination picks.
                // Google Docs, Word and Notion take the HTML and paste real headings and
                // bullets; a text editor or a terminal takes the markdown. One button, no
                // choice to make, and nothing lost either way.
                const markdown = event.summary_md ?? "";
                try {
                  await copyRichText(summaryToHtml(markdown), markdown);
                  setCopied(true);
                  window.setTimeout(() => setCopied(false), 2500);
                } catch {
                  onError("Could not copy — select the text and copy it manually.");
                }
              }}
            >
              {copied ? "Copied" : "Copy summary"}
            </button>
            <button type="button" className="button" onClick={generate} disabled={running}>
              {running ? "Regenerating…" : "Regenerate"}
            </button>
            <span className="small muted">
              Pastes with formatting into Docs, and as markdown into a text editor.
              {event.model_used ? ` Last written by ${event.model_used}.` : ""}
            </span>
          </div>
        </>
      ) : (
        <div className="panel">
          <p style={{ marginTop: 0 }}>
            {running ? "Writing the summary…" : "No summary yet."}
          </p>
          <p className="small muted">
            {canGenerate
              ? "Summaries are the only feature that sends anything off this machine, and only when you ask."
              : "Summaries are written from the transcript, so this needs a transcript first."}
          </p>
          <button
            type="button"
            className="button primary"
            onClick={generate}
            disabled={running || !canGenerate}
            style={{ marginTop: 6 }}
          >
            {running ? "Writing…" : "Write a summary"}
          </button>
        </div>
      )}
    </div>
  );
}

function AudioTab({
  src,
  audioRef,
  onTime,
  event,
  onChanged,
}: {
  src: string | null;
  audioRef: React.MutableRefObject<HTMLAudioElement | null>;
  onTime: (ms: number) => void;
  event: HearsayEvent;
  onChanged: () => void;
}) {
  const [retranscribing, setRetranscribing] = useState(false);
  const [duration, setDuration] = useState<number | null>(null);

  // Deliberately deleted, which is not the same as never having had a file. Saying which
  // is what makes the inert seek buttons elsewhere read as a consequence of a decision
  // rather than as something broken.
  if (event.audio_deleted_at) {
    return (
      <div className="panel">
        <p style={{ marginTop: 0 }}>
          The audio was deleted on{" "}
          {new Date(event.audio_deleted_at).toLocaleDateString(undefined, {
            month: "long",
            day: "numeric",
            year: "numeric",
          })}
          .
        </p>
        <p className="small muted">
          Its transcript, summary and questions are all still here, and still searchable —
          none of them were read from the file. What is gone is playing it back, jumping to
          a timestamp, saving a copy, and transcribing it again.
        </p>
      </div>
    );
  }

  if (!src) {
    return <p className="muted">This recording has no audio file.</p>;
  }

  return (
    <div>
      <audio
        ref={audioRef}
        src={src}
        controls
        preload="metadata"
        style={{ width: "100%" }}
        onTimeUpdate={(changed) => onTime(changed.currentTarget.currentTime * 1000)}
        onLoadedMetadata={(loaded) => {
          const seconds = loaded.currentTarget.duration;
          // A file still being measured reports Infinity, and a span cannot be offered
          // against a length nobody knows yet.
          setDuration(Number.isFinite(seconds) ? seconds * 1000 : null);
        }}
      />

      <SaveCopy event={event} audioRef={audioRef} durationMs={duration} />

      <div className="row" style={{ marginTop: 20 }}>
        <button
          type="button"
          className="button"
          disabled={retranscribing}
          onClick={async () => {
            setRetranscribing(true);
            try {
              await invoke("retranscribe", { eventId: event.id });
            } finally {
              setRetranscribing(false);
            }
          }}
        >
          {retranscribing ? "Started…" : "Transcribe again"}
        </button>
        <span className="small muted">
          Re-runs transcription from the audio. Replaces the existing transcript.
        </span>
      </div>

      <DeleteAudio event={event} onDeleted={onChanged} />
    </div>
  );
}

/**
 * Throwing away the audio and keeping everything written from it.
 *
 * The recording is the largest thing Hearsay stores by a wide margin, and once a
 * transcript exists it is often no longer wanted. This is the only way to reclaim that
 * space, and it is deliberately a per-recording decision made by hand: nothing here runs
 * on a timer, sweeps the archive, or deletes anything the user did not point at.
 *
 * Untranscribed audio is refused rather than confirmed away. Deleting it would leave the
 * recording as an empty row — not a saving, but the loss of the whole thing.
 */
function DeleteAudio({ event, onDeleted }: { event: HearsayEvent; onDeleted: () => void }) {
  const [working, setWorking] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [freed, setFreed] = useState<number | null>(null);

  const transcribed = event.transcribed_at !== null;

  const remove = async () => {
    // Every consequence named, because none of them can be undone and one of them —
    // never being able to transcribe this recording again — is easy not to think of.
    const confirmed = await ask(
      `Delete the audio for "${event.title}"?\n\nThe transcript, summary and questions stay, and stay searchable. Playback, jumping to a timestamp, saving a copy and transcribing this recording again all go, permanently.`,
      {
        title: "Delete the audio",
        kind: "warning",
        okLabel: "Delete the audio",
        cancelLabel: "Keep it",
      },
    );
    if (!confirmed) return;

    setWorking(true);
    setProblem(null);
    try {
      const reclaimed = await invoke<ReclaimedAudio>("delete_audio", { eventId: event.id });
      setFreed(reclaimed.bytes);
      onDeleted();
    } catch (failure) {
      setProblem(String((failure as { message?: string })?.message ?? failure));
    } finally {
      setWorking(false);
    }
  };

  return (
    <div className="row" style={{ marginTop: 20 }}>
      <button
        type="button"
        className="button destructive small"
        onClick={remove}
        disabled={working || !transcribed}
      >
        {working ? "Deleting…" : "Delete the audio"}
      </button>
      <span className="small muted">
        {problem ??
          (freed !== null
            ? `Freed ${formatBytes(freed)}.`
            : transcribed
              ? "Frees the space, keeps the transcript. This cannot be undone."
              : "Needs a transcript first — deleting the audio now would leave nothing.")}
      </span>
    </div>
  );
}

/**
 * Saving the audio out of Hearsay, whole or in part.
 *
 * The span picker is folded away by default. Most saves are of a whole recording, and two
 * time fields presented to everyone who only wanted the file would be a decision where none
 * was needed. Opened, it fills itself in from the playhead, because the moment someone wants
 * a clip of is usually the moment they were just listening to.
 */
function SaveCopy({
  event,
  audioRef,
  durationMs,
}: {
  event: HearsayEvent;
  audioRef: React.MutableRefObject<HTMLAudioElement | null>;
  durationMs: number | null;
}) {
  const [partial, setPartial] = useState(false);
  const [startText, setStartText] = useState("00:00");
  const [endText, setEndText] = useState("");
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState<ExportedAudio | null>(null);
  const [problem, setProblem] = useState<string | null>(null);

  const startMs = parseClock(startText);
  const endMs = parseClock(endText);
  const spanIsUsable = startMs !== null && endMs !== null && endMs > startMs;
  const spanLength = spanIsUsable ? endMs - startMs : null;

  const playheadMs = () => Math.round((audioRef.current?.currentTime ?? 0) * 1000);

  const openPicker = (wanted: boolean) => {
    setPartial(wanted);
    setProblem(null);
    if (!wanted) return;
    // From here to the end is the common case — someone listening to the part they want to
    // keep sets the start and rarely needs to touch the end.
    setStartText(formatClock(playheadMs()));
    setEndText(formatClock(durationMs ?? playheadMs()));
  };

  // The save sheet is the only place this can go: Hearsay picks no folder of its own, so
  // the copy lands wherever the user says and nowhere else.
  const saveCopy = async () => {
    setProblem(null);
    setSaved(null);
    const bounds = partial ? { startMs, endMs } : { startMs: null, endMs: null };
    try {
      const fileName = await invoke<string>("export_file_name", {
        eventId: event.id,
        ...bounds,
      });
      const destination = await save({
        defaultPath: fileName,
        title: partial ? "Save this part of the audio" : "Save a copy of the audio",
        filters: [
          { name: "Compressed audio", extensions: ["m4a"] },
          { name: "Original recording", extensions: ["wav"] },
        ],
      });
      if (!destination) return;

      setSaving(true);
      setSaved(
        await invoke<ExportedAudio>("export_audio", {
          eventId: event.id,
          destination,
          ...bounds,
        }),
      );
    } catch (failure) {
      setProblem(String((failure as { message?: string })?.message ?? failure));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="export">
      <div className="row">
        <button
          type="button"
          className="button"
          disabled={saving || (partial && !spanIsUsable)}
          onClick={saveCopy}
        >
          {saving ? "Saving…" : partial ? "Save this part" : "Save a copy"}
        </button>
        <label className="check small">
          <input
            type="checkbox"
            checked={partial}
            onChange={(changed) => openPicker(changed.currentTarget.checked)}
          />
          Just part of it
        </label>
      </div>

      {partial ? (
        <div className="export-span">
          <span className="small muted">From</span>
          <input
            className={`time-input${startMs === null ? " invalid" : ""}`}
            value={startText}
            aria-label="Start of the part to save"
            onChange={(changed) => setStartText(changed.currentTarget.value)}
          />
          <button
            type="button"
            className="button small"
            onClick={() => setStartText(formatClock(playheadMs()))}
          >
            Use playhead
          </button>
          <span className="small muted">to</span>
          <input
            className={`time-input${endMs === null ? " invalid" : ""}`}
            value={endText}
            aria-label="End of the part to save"
            onChange={(changed) => setEndText(changed.currentTarget.value)}
          />
          <button
            type="button"
            className="button small"
            onClick={() => setEndText(formatClock(playheadMs()))}
          >
            Use playhead
          </button>
        </div>
      ) : null}

      <p className="small muted">
        {partial ? (
          spanLength === null ? (
            "Times read as minutes and seconds — 4:20, or 1:04:20 past an hour."
          ) : (
            <>
              <span className="mono">{formatClock(spanLength)}</span> of audio, about{" "}
              {formatBytes(estimateAacBytes(spanLength))} as an .m4a.
            </>
          )
        ) : (
          <>
            Writes an .m4a small enough to keep or send — or a .wav if you name one, which is
            the recording untouched.
            {event.mode === "conversation"
              ? " Either way the channels stay as recorded: you on the left, everyone else on the right."
              : ""}
          </>
        )}
      </p>

      {saved ? (
        <p className="small muted">
          Saved {formatBytes(saved.bytes)} to <span className="mono">{saved.path}</span>
        </p>
      ) : null}

      {problem ? <div className="banner problem">{problem}</div> : null}
    </div>
  );
}

/** Roughly what a span will weigh once compressed: 96 kbps, the rate the export uses. */
function estimateAacBytes(ms: number): number {
  return Math.round((ms / 1000) * 12_000);
}

/**
 * A deliberately small markdown renderer: headings, bullets, bold, and paragraphs.
 *
 * Summaries are generated by a model from a fixed prompt, so the markdown they contain is
 * predictable. Pulling in a full parser to handle tables and footnotes that will never
 * appear would be more dependency than the job needs — and this renders text nodes, so
 * nothing in a summary can inject markup.
 */
/// Puts formatted and plain versions of the same text on the clipboard together.
///
/// Falls back to plain text where `ClipboardItem` is missing, so the button always does
/// something rather than failing on the fancier path.
async function copyRichText(html: string, plain: string): Promise<void> {
  if (typeof ClipboardItem !== "undefined" && navigator.clipboard?.write) {
    try {
      await navigator.clipboard.write([
        new ClipboardItem({
          "text/html": new Blob([html], { type: "text/html" }),
          "text/plain": new Blob([plain], { type: "text/plain" }),
        }),
      ]);
      return;
    } catch {
      // Fall through: some paste targets reject multi-flavour writes.
    }
  }
  await navigator.clipboard.writeText(plain);
}

function escapeHtml(text: string): string {
  return text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

function inlineToHtml(text: string): string {
  return text
    .split(/(\*\*[^*]+\*\*)/g)
    .map((part) =>
      part.startsWith("**") && part.endsWith("**") && part.length > 4
        ? `<strong>${escapeHtml(part.slice(2, -2))}</strong>`
        : escapeHtml(part),
    )
    .join("");
}

/// The same grammar [`renderMarkdown`] understands, emitted as HTML for the clipboard.
///
/// Inline styles as well as semantic tags, because some editors keep the tag and ignore the
/// style while others do the reverse.
///
/// **Section headings paste as bold body text, not as headings.** A `<h2>` is claimed by
/// Google Docs' own Heading 2 style — a different face at a larger size, listed in the
/// document outline — and no inline style overrides that, because Docs matches on the tag.
/// A summary is usually pasted *into* a document that already has its own headings, where a
/// borrowed outline level fights the surrounding structure. A bold line with space above it
/// reads as a section wherever it lands and belongs to whatever it was pasted into.
function summaryToHtml(markdown: string): string {
  const out: string[] = [];
  let bullets: string[] = [];

  const flushBullets = () => {
    if (bullets.length === 0) return;
    out.push(
      `<ul>${bullets.map((item) => `<li>${inlineToHtml(item)}</li>`).join("")}</ul>`,
    );
    bullets = [];
  };

  for (const raw of markdown.split("\n")) {
    const line = raw.trimEnd();
    const bullet = /^\s*[-*]\s+(.*)$/.exec(line);
    if (bullet?.[1] !== undefined) {
      bullets.push(bullet[1]);
      continue;
    }
    flushBullets();

    const heading = /^(#{1,4})\s+(.*)$/.exec(line);
    if (heading?.[1] && heading[2] !== undefined) {
      // A paragraph carrying <strong>, at body size. Every heading level renders the same
      // way: the summary prompt only ever writes one level of section, so a hierarchy here
      // would be inventing a distinction the text does not make.
      out.push(
        `<p style="margin:16px 0 6px"><strong>${inlineToHtml(heading[2])}</strong></p>`,
      );
      continue;
    }

    if (line.trim() === "") continue;
    out.push(`<p style="margin:0 0 8px">${inlineToHtml(line)}</p>`);
  }
  flushBullets();

  return `<div>${out.join("")}</div>`;
}

function renderMarkdown(markdown: string): React.ReactNode {
  const blocks: React.ReactNode[] = [];
  const lines = markdown.split("\n");
  let bullets: string[] = [];

  const flushBullets = () => {
    if (bullets.length === 0) return;
    blocks.push(
      <ul key={`ul-${blocks.length}`}>
        {bullets.map((item, index) => (
          <li key={index}>{renderInline(item)}</li>
        ))}
      </ul>,
    );
    bullets = [];
  };

  for (const raw of lines) {
    const line = raw.trimEnd();
    const bullet = /^\s*[-*]\s+(.*)$/.exec(line);
    if (bullet?.[1] !== undefined) {
      bullets.push(bullet[1]);
      continue;
    }
    flushBullets();

    const heading = /^(#{1,4})\s+(.*)$/.exec(line);
    if (heading?.[1] && heading[2] !== undefined) {
      const level = heading[1].length;
      const text = renderInline(heading[2]);
      blocks.push(
        level <= 2 ? (
          <h2 key={blocks.length}>{text}</h2>
        ) : (
          <h3 key={blocks.length}>{text}</h3>
        ),
      );
      continue;
    }

    if (line.trim() === "") continue;
    blocks.push(<p key={blocks.length}>{renderInline(line)}</p>);
  }
  flushBullets();

  return blocks;
}

/** Bold only. Everything else stays literal text. */
function renderInline(text: string): React.ReactNode {
  const parts = text.split(/(\*\*[^*]+\*\*)/g);
  return parts.map((part, index) =>
    part.startsWith("**") && part.endsWith("**") && part.length > 4 ? (
      <strong key={index}>{part.slice(2, -2)}</strong>
    ) : (
      <span key={index}>{part}</span>
    ),
  );
}
