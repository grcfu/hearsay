import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { dayKey, formatDayHeading, formatDuration, formatMode, formatTime } from "../format";
import type { HearsayEvent, SearchHit } from "../types";

interface Props {
  selectedId: number | null;
  onSelect: (id: number, seekMs?: number) => void;
  refreshToken: number;
}

/**
 * Pane two: every recording, grouped by day.
 *
 * The list is the app's whole navigation model — there is no folder tree and no archive,
 * so scrolling back is the only "elsewhere" there is. Typing in the search box replaces
 * the list with matching transcript lines, and clearing it puts the list back.
 */
export function EventList({ selectedId, onSelect, refreshToken }: Props) {
  const [events, setEvents] = useState<HearsayEvent[]>([]);
  const [query, setQuery] = useState("");
  const [hits, setHits] = useState<SearchHit[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setEvents(await invoke<HearsayEvent[]>("list_events"));
      setError(null);
    } catch (problem) {
      setError(String((problem as { message?: string })?.message ?? problem));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load, refreshToken]);

  // Debounced so every keystroke does not run a query, but short enough that results
  // feel like they arrive as you type.
  useEffect(() => {
    const trimmed = query.trim();
    if (!trimmed) {
      setHits(null);
      return;
    }
    const timer = window.setTimeout(async () => {
      try {
        setHits(await invoke<SearchHit[]>("search_transcripts", { query: trimmed }));
      } catch (problem) {
        console.error("search failed", problem);
      }
    }, 140);
    return () => window.clearTimeout(timer);
  }, [query]);

  const groups = useMemo(() => groupByDay(events), [events]);

  return (
    <section className="event-list">
      <div className="list-header">
        <input
          className="search-input"
          type="search"
          value={query}
          placeholder="Search transcripts"
          aria-label="Search transcripts"
          onChange={(changed) => setQuery(changed.target.value)}
        />
      </div>

      {error ? (
        <div className="empty">
          <div>Could not load recordings</div>
          <div className="small">{error}</div>
        </div>
      ) : hits !== null ? (
        <SearchResults hits={hits} onSelect={onSelect} />
      ) : groups.length === 0 ? (
        <div className="empty">
          <div>No recordings yet</div>
          <div className="small">Recordings you make will appear here, newest first.</div>
        </div>
      ) : (
        groups.map((group) => (
          <div className="day-group" key={group.key}>
            <div className="day-heading">{formatDayHeading(group.events[0]!.started_at)}</div>
            {group.events.map((event) => (
              <EventCard
                key={event.id}
                event={event}
                selected={event.id === selectedId}
                onSelect={() => onSelect(event.id)}
              />
            ))}
          </div>
        ))
      )}
    </section>
  );
}

function EventCard({
  event,
  selected,
  onSelect,
}: {
  event: HearsayEvent;
  selected: boolean;
  onSelect: () => void;
}) {
  const title = event.title.trim() || event.ai_title?.trim() || "Untitled recording";
  const duration = event.ended_at
    ? new Date(event.ended_at).getTime() - new Date(event.started_at).getTime()
    : null;

  return (
    <button
      type="button"
      className={`event-card${selected ? " selected" : ""}`}
      onClick={onSelect}
      aria-current={selected}
    >
      <span className="event-card-title">{title}</span>
      <span className="event-card-meta">
        <span>{formatTime(event.started_at)}</span>
        <span aria-hidden>·</span>
        <span>{formatDuration(duration)}</span>
        <span aria-hidden>·</span>
        <span>{formatMode(event.mode)}</span>
      </span>
    </button>
  );
}

function SearchResults({
  hits,
  onSelect,
}: {
  hits: SearchHit[];
  onSelect: (id: number, seekMs?: number) => void;
}) {
  if (hits.length === 0) {
    return (
      <div className="empty">
        <div>Nothing found</div>
        <div className="small">No transcript contains those words.</div>
      </div>
    );
  }

  return (
    <div className="day-group">
      <div className="day-heading">
        {hits.length} {hits.length === 1 ? "match" : "matches"}
      </div>
      {hits.map((hit) => (
        <button
          type="button"
          key={hit.segment_id}
          className="event-card"
          // Clicking a result opens the recording at the moment those words were said.
          onClick={() => onSelect(hit.event_id, hit.start_ms)}
        >
          <span className="event-card-title">{hit.event_title}</span>
          <span className="small" style={{ display: "block", marginTop: 2 }}>
            {hit.snippet}
          </span>
          <span className="event-card-meta">
            <span>{hit.channel === "mic" ? "You" : "Them"}</span>
            <span aria-hidden>·</span>
            <span>{formatTime(hit.started_at)}</span>
          </span>
        </button>
      ))}
    </div>
  );
}

interface DayGroup {
  key: string;
  events: HearsayEvent[];
}

/** Buckets events into calendar days, preserving the newest-first order within each. */
function groupByDay(events: HearsayEvent[]): DayGroup[] {
  const groups: DayGroup[] = [];
  for (const event of events) {
    const key = dayKey(event.started_at);
    const existing = groups.find((group) => group.key === key);
    if (existing) existing.events.push(event);
    else groups.push({ key, events: [event] });
  }
  return groups;
}
