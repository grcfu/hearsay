import { useCallback, useEffect, useRef, useState } from "react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ask } from "@tauri-apps/plugin-dialog";
import { Transcript } from "./Transcript";
import { formatDuration, formatMode, formatTime } from "../format";
import type { HearsayEvent, MuteSpan, Segment } from "../types";

interface EventDetail {
  event: HearsayEvent;
  segments: Segment[];
  mute_spans: MuteSpan[];
}

interface TranscriptionEvent {
  event_id: number;
  stage: string;
  percent?: number;
  channel?: string;
  message?: string;
  segments?: number;
}

type Tab = "summary" | "transcript" | "audio";

interface Props {
  eventId: number | null;
  seekMs: number | null;
  onChanged: () => void;
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
  const [error, setError] = useState<string | null>(null);
  const [playheadMs, setPlayheadMs] = useState<number | null>(null);
  const audioRef = useRef<HTMLAudioElement | null>(null);

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
  }, [load]);

  // Transcription runs long after the recording stops, so the detail pane refreshes
  // itself when it finishes rather than making the user go and come back.
  useEffect(() => {
    const unlisten = listen<TranscriptionEvent>("transcription", (message) => {
      if (message.payload.event_id !== eventId) return;
      setProgress(message.payload);
      if (message.payload.stage === "done") {
        void load();
        onChanged();
      }
    });
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, [eventId, load, onChanged]);

  const seek = useCallback((ms: number) => {
    const audio = audioRef.current;
    if (!audio) return;
    audio.currentTime = ms / 1000;
    void audio.play();
  }, []);

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

  const { event, segments, mute_spans: muteSpans } = detail;
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
          {(["summary", "transcript", "audio"] as Tab[]).map((name) => (
            <button
              type="button"
              key={name}
              role="tab"
              aria-selected={tab === name}
              className={`tab${tab === name ? " active" : ""}`}
              onClick={() => setTab(name)}
            >
              {name === "summary" ? "Summary" : name === "transcript" ? "Transcript" : "Audio"}
            </button>
          ))}
        </div>
      </div>

      <div className="detail-body">
        <TranscriptionProgress
          progress={progress}
          channels={event.mode === "conversation" ? 2 : 1}
        />

        {tab === "summary" ? (
          <SummaryTab
            event={event}
            segmentCount={segments.length}
            onChanged={() => {
              void load();
              onChanged();
            }}
          />
        ) : tab === "transcript" ? (
          <Transcript
            segments={segments}
            muteSpans={muteSpans}
            onSeek={seek}
            activeMs={playheadMs}
          />
        ) : (
          <AudioTab
            src={audioSrc}
            audioRef={audioRef}
            onTime={setPlayheadMs}
            eventId={event.id}
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
}: {
  progress: TranscriptionEvent | null;
  channels: number;
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

  // Downloads report their own percentage. Transcription reports per channel, so it is
  // scaled into the overall job.
  const percent = downloading
    ? (progress.percent ?? 0)
    : transcribing
      ? Math.min(100, ((done + (progress.percent ?? 0) / 100) / Math.max(channels, 1)) * 100)
      : null;

  const label = downloading
    ? "Downloading the speech model — this happens once"
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
          <span className="progress-percent">working…</span>
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
    </div>
  );
}

function SummaryTab({
  event,
  segmentCount,
  onChanged,
}: {
  event: HearsayEvent;
  segmentCount: number;
  onChanged: () => void;
}) {
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // The summary runs on a worker thread and reports back by event, so this listens
  // rather than awaiting the invoke.
  useEffect(() => {
    const unlisten = listen<{ event_id: number; stage: string; message?: string }>(
      "summary",
      (message) => {
        if (message.payload.event_id !== event.id) return;
        if (message.payload.stage === "done") {
          setRunning(false);
          onChanged();
        } else if (message.payload.stage === "failed") {
          setRunning(false);
          setError(message.payload.message ?? "Summary failed.");
        }
      },
    );
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, [event.id, onChanged]);

  const generate = async () => {
    setRunning(true);
    setError(null);
    try {
      await invoke("generate_summary", { eventId: event.id });
    } catch (problem) {
      setRunning(false);
      setError(String((problem as { message?: string })?.message ?? problem));
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
            <button type="button" className="button" onClick={generate} disabled={running}>
              {running ? "Regenerating…" : "Regenerate"}
            </button>
            <span className="small muted">
              Rewritten from the transcript. The recording is not touched.
              {event.model_used ? ` Last written by ${event.model_used}.` : ""}
            </span>
          </div>
        </>
      ) : (
        <div className="panel">
          <p style={{ marginTop: 0 }}>{running ? "Writing the summary…" : "No summary yet."}</p>
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
  eventId,
}: {
  src: string | null;
  audioRef: React.MutableRefObject<HTMLAudioElement | null>;
  onTime: (ms: number) => void;
  eventId: number;
}) {
  const [retranscribing, setRetranscribing] = useState(false);

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
      />
      <div className="row" style={{ marginTop: 14 }}>
        <button
          type="button"
          className="button"
          disabled={retranscribing}
          onClick={async () => {
            setRetranscribing(true);
            try {
              await invoke("retranscribe", { eventId });
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
    </div>
  );
}

/**
 * A deliberately small markdown renderer: headings, bullets, bold, and paragraphs.
 *
 * Summaries are generated by a model from a fixed prompt, so the markdown they contain is
 * predictable. Pulling in a full parser to handle tables and footnotes that will never
 * appear would be more dependency than the job needs — and this renders text nodes, so
 * nothing in a summary can inject markup.
 */
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
