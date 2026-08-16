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
  /** Whether there is audio to jump to. False once the audio has been deleted. */
  canSeek: boolean;
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
export function Transcript({
  segments,
  muteSpans,
  onSeek,
  activeMs,
  speakerName,
  canSeek,
}: Props) {
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
            // Without audio these are still the transcript, just not a way into it. A
            // button that looks live and does nothing when pressed is the failure mode
            // this app avoids everywhere else, so the line becomes plain text — readable
            // and selectable, but making no offer it cannot keep.
            <Line
              key={`segment-${line.segment.id}`}
              as={canSeek ? "button" : "div"}
              className={`transcript-line${isActive(line.segment, activeMs) ? " active" : ""}${
                canSeek ? "" : " static"
              }`}
              onClick={canSeek ? () => onSeek(line.segment.start_ms) : undefined}
            >
              <span className="transcript-time mono">{formatClock(line.segment.start_ms)}</span>
              <span className={`speaker speaker-${line.segment.channel}`}>
                {line.segment.channel === "mic" ? me : "Them"}
              </span>
              <span className="transcript-text">{line.segment.text}</span>
            </Line>
          ),
        )}
      </div>
    </>
  );
}

/**
 * One transcript line, as a button when it can seek and as plain text when it cannot.
 *
 * Two elements rather than a disabled button: a disabled button is dimmed and its text
 * cannot be selected, and this is a transcript — reading and copying from it are the
 * point, and neither should get worse because the audio is gone.
 */
function Line({
  as,
  className,
  onClick,
  children,
}: {
  as: "button" | "div";
  className: string;
  onClick?: () => void;
  children: React.ReactNode;
}) {
  if (as === "div") {
    return <div className={className}>{children}</div>;
  }
  return (
    <button type="button" className={className} onClick={onClick}>
      {children}
    </button>
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
