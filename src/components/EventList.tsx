interface Props {
  selectedId: number | null;
  onSelect: (id: number) => void;
  refreshToken: number;
}

/**
 * Pane two: every recording, grouped by day.
 *
 * The list is the app's whole navigation model. There is no folder tree and no archive —
 * scrolling back is the only "elsewhere" there is.
 */
export function EventList(_props: Props) {
  return (
    <section className="event-list">
      <div className="list-header">
        <input
          className="search-input"
          type="search"
          placeholder="Search transcripts"
          aria-label="Search transcripts"
          disabled
        />
      </div>
      <div className="empty">
        <div>No recordings yet</div>
        <div className="small">Recordings you make will appear here, newest first.</div>
      </div>
    </section>
  );
}
