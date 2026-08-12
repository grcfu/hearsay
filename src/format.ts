/** Formatting helpers. Sentence case everywhere, per the design rules. */

/** `MM:SS`, or `H:MM:SS` past an hour. Used for transcript timestamps and elapsed time. */
export function formatClock(ms: number): string {
  const totalSeconds = Math.max(0, Math.floor(ms / 1000));
  const seconds = totalSeconds % 60;
  const minutes = Math.floor(totalSeconds / 60) % 60;
  const hours = Math.floor(totalSeconds / 3600);
  const pad = (value: number) => value.toString().padStart(2, "0");
  return hours > 0
    ? `${hours}:${pad(minutes)}:${pad(seconds)}`
    : `${pad(minutes)}:${pad(seconds)}`;
}

/**
 * The inverse of {@link formatClock}, for a field someone types into.
 *
 * Accepts `H:MM:SS`, `MM:SS`, and a bare number of seconds, since all three are things a
 * person reasonably types into a box showing `04:20`. Returns null for anything else rather
 * than a guess — a silently misread selection would export the wrong part of the recording.
 */
export function parseClock(text: string): number | null {
  const trimmed = text.trim();
  if (trimmed === "") return null;

  const parts = trimmed.split(":");
  if (parts.length > 3) return null;

  let total = 0;
  for (const part of parts) {
    if (!/^\d+$/.test(part.trim())) return null;
    total = total * 60 + Number(part);
  }
  return total * 1000;
}

/** A short human duration, like "42 min" or "1 h 8 min". */
export function formatDuration(ms: number | null): string {
  if (ms === null || ms <= 0) return "—";
  const minutes = Math.round(ms / 60000);
  if (minutes < 1) return "under a minute";
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.floor(minutes / 60);
  const remainder = minutes % 60;
  return remainder === 0 ? `${hours} h` : `${hours} h ${remainder} min`;
}

/** Time of day in the user's locale, e.g. "2:15 PM". */
export function formatTime(iso: string): string {
  return new Date(iso).toLocaleTimeString(undefined, {
    hour: "numeric",
    minute: "2-digit",
  });
}

/**
 * A day heading. "Today" and "Yesterday" get names because those are the two the user
 * scans for; everything older gets a date.
 */
export function formatDayHeading(iso: string): string {
  const date = new Date(iso);
  const startOfDay = (value: Date) =>
    new Date(value.getFullYear(), value.getMonth(), value.getDate()).getTime();

  const days = Math.round((startOfDay(new Date()) - startOfDay(date)) / 86_400_000);
  if (days === 0) return "Today";
  if (days === 1) return "Yesterday";
  if (days < 7) return date.toLocaleDateString(undefined, { weekday: "long" });

  const sameYear = date.getFullYear() === new Date().getFullYear();
  return date.toLocaleDateString(undefined, {
    weekday: "short",
    month: "short",
    day: "numeric",
    year: sameYear ? undefined : "numeric",
  });
}

/** The key events are grouped by in the list: one bucket per calendar day, local time. */
export function dayKey(iso: string): string {
  const date = new Date(iso);
  return `${date.getFullYear()}-${date.getMonth()}-${date.getDate()}`;
}

export function formatMode(mode: string): string {
  return mode === "conversation" ? "Conversation" : "Listen only";
}
