interface Props {
  eventId: number | null;
  onChanged: () => void;
}

/**
 * Pane three: one recording, with its summary, transcript, and audio as peer tabs.
 */
export function Detail({ eventId }: Props) {
  if (eventId === null) {
    return (
      <div className="empty">
        <div>Nothing selected</div>
        <div className="small">Pick a recording to see its summary and transcript.</div>
      </div>
    );
  }

  return (
    <div className="empty">
      <div>Loading…</div>
    </div>
  );
}
