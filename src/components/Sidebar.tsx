import type { Mode, SystemStatus } from "../types";

interface Props {
  mode: Mode;
  onModeChange: (mode: Mode) => void;
  status: SystemStatus | null;
  onRecorded: (eventId: number) => void;
  onStatusChange: () => void;
}

/**
 * Pane one. Royal, always visible, and the only place recording is controlled from.
 */
export function Sidebar({ mode, onModeChange }: Props) {
  return (
    <aside className="sidebar">
      <div className="sidebar-title">Hearsay</div>

      <div className="sidebar-section">
        <div className="sidebar-label">Mode</div>
        <ModeToggle mode={mode} onChange={onModeChange} disabled={false} />
        <p className="small" style={{ opacity: 0.6, padding: "2px 6px 0", margin: 0 }}>
          {mode === "listen_only"
            ? "System audio only. The microphone is never opened."
            : "Microphone on the left channel, system audio on the right."}
        </p>
      </div>
    </aside>
  );
}

interface ToggleProps {
  mode: Mode;
  onChange: (mode: Mode) => void;
  disabled: boolean;
}

/**
 * Choosing between the two modes.
 *
 * Locked while recording: switching from listen-only to conversation mid-session would
 * mean opening the microphone partway through a recording the user started believing it
 * could not hear the room.
 */
export function ModeToggle({ mode, onChange, disabled }: ToggleProps) {
  return (
    <div className="mode-toggle" role="radiogroup" aria-label="Recording mode">
      <button
        type="button"
        role="radio"
        aria-checked={mode === "listen_only"}
        disabled={disabled}
        className={`mode-option${mode === "listen_only" ? " selected" : ""}`}
        onClick={() => onChange("listen_only")}
      >
        Listen only
      </button>
      <button
        type="button"
        role="radio"
        aria-checked={mode === "conversation"}
        disabled={disabled}
        className={`mode-option${mode === "conversation" ? " selected" : ""}`}
        onClick={() => onChange("conversation")}
      >
        Conversation
      </button>
    </div>
  );
}
