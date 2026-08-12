import { useState } from "react";
import { formatClock } from "../format";
import type { MuteSpan, Segment } from "../types";

interface Props {
  segments: Segment[];
  muteSpans: MuteSpan[];
  onSeek: (ms: number) => void;
  activeMs: number | null;
  /** What to call the person who recorded. Falls back to "You" when unset. */
  speakerName?: string;
}

type Line =
  | { kind: "segment"; segment: Segment }
  | { kind: "mute"; span: MuteSpan };

/**
 * The transcript, with muted stretches shown rather than skipped.
 *
 * A gap in a transcript is ambiguous: nobody spoke, or the microphone was off? Every
 * muted span is rendered in place as an explicit marker, so time is never silently
 * missing from the record.
 */
export function Transcript({ segments, muteSpans, onSeek, activeMs, speakerName }: Props) {
  const lines = interleave(segments, muteSpans);
  const [copied, setCopied] = useState(false);
  const [copyError, setCopyError] = useState(false);
  const me = speakerName?.trim() ? speakerName.trim() : "You";

  if (lines.length === 0) {
    return (
      <p className="muted">
        No transcript yet. It appears here once transcription finishes.
      </p>
    );
  }

  const copy = async () => {
    setCopyError(false);
    try {
      await navigator.clipboard.writeText(toPlainText(lines, me));
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2500);
    } catch {
      setCopyError(true);
    }
  };

  return (
    <>
      {/* Above the transcript rather than below it: an hour-long meeting is a long scroll,
          and a button at the end is a button nobody finds. */}
      <div className="row" style={{ marginBottom: 12 }}>
        <button type="button" className="button" onClick={copy}>
          {copied ? "Copied" : "Copy transcript"}
        </button>
        <span className="small muted">
          {copyError
            ? "Could not copy — select the text and copy it manually."
            : "Plain text with timestamps, muted stretches marked."}
        </span>
      </div>

      <div className="transcript">
        {lines.map((line) =>
          line.kind === "mute" ? (
            <div className="mute-marker" key={`mute-${line.span.id}`}>
              [mic muted — {formatClock(line.span.start_ms)} to {formatClock(line.span.end_ms)}]
            </div>
          ) : (
            <button
              type="button"
              key={`segment-${line.segment.id}`}
              className={`transcript-line${isActive(line.segment, activeMs) ? " active" : ""}`}
              onClick={() => onSeek(line.segment.start_ms)}
            >
              <span className="transcript-time mono">{formatClock(line.segment.start_ms)}</span>
              <span className={`speaker speaker-${line.segment.channel}`}>
                {line.segment.channel === "mic" ? me : "Them"}
              </span>
              <span className="transcript-text">{line.segment.text}</span>
            </button>
          ),
        )}
      </div>
    </>
  );
}

/**
 * The transcript as text, in the same shape the model is given.
 *
 * Muted stretches are carried across as markers rather than dropped. Pasting a transcript
 * with an unexplained gap somewhere else would lose the one piece of context that says the
 * silence was deliberate.
 */
function toPlainText(lines: Line[], me: string): string {
  return lines
    .map((line) =>
      line.kind === "mute"
        ? `[${formatClock(line.span.start_ms)}] [mic muted until ${formatClock(line.span.end_ms)}]`
        : `[${formatClock(line.segment.start_ms)}] ${
            line.segment.channel === "mic" ? me : "Them"
          }: ${line.segment.text.trim()}`,
    )
    .join("\n");
}

function isActive(segment: Segment, activeMs: number | null): boolean {
  if (activeMs === null) return false;
  return activeMs >= segment.start_ms && activeMs < segment.end_ms;
}

/**
 * Merges segments and mute spans into one timeline ordered by start time.
 *
 * Mute markers sort by their start, so a muted stretch lands between the things said
 * before and after it rather than being appended at the end.
 */
function interleave(segments: Segment[], muteSpans: MuteSpan[]): Line[] {
  const lines: Line[] = [
    ...segments.map((segment) => ({ kind: "segment" as const, segment })),
    ...muteSpans.map((span) => ({ kind: "mute" as const, span })),
  ];

  lines.sort((a, b) => {
    const aStart = a.kind === "segment" ? a.segment.start_ms : a.span.start_ms;
    const bStart = b.kind === "segment" ? b.segment.start_ms : b.span.start_ms;
    return aStart - bStart;
  });

  return lines;
}
