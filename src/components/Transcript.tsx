import { formatClock } from "../format";
import type { MuteSpan, Segment } from "../types";

interface Props {
  segments: Segment[];
  muteSpans: MuteSpan[];
  onSeek: (ms: number) => void;
  activeMs: number | null;
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
export function Transcript({ segments, muteSpans, onSeek, activeMs }: Props) {
  const lines = interleave(segments, muteSpans);

  if (lines.length === 0) {
    return (
      <p className="muted">
        No transcript yet. It appears here once transcription finishes.
      </p>
    );
  }

  return (
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
              {line.segment.channel === "mic" ? "You" : "Them"}
            </span>
            <span className="transcript-text">{line.segment.text}</span>
          </button>
        ),
      )}
    </div>
  );
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
