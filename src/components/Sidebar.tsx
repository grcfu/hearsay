import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { formatClock } from "../format";
import type { AudibleApp, HearsayEvent, Mode, SystemStatus } from "../types";

interface LiveStatus {
  recording: boolean;
  event_id: number | null;
  mode: Mode | null;
  elapsed_ms: number;
  frames_written: number;
  peak: number;
  has_audio: boolean;
  silent_while_audio_playing: boolean;
}

interface Props {
  mode: Mode;
  onModeChange: (mode: Mode) => void;
  status: SystemStatus | null;
  onRecorded: (eventId: number) => void;
  onStatusChange: () => void;
}

/**
 * Pane one. Royal, always visible, and the only place recording is controlled from.
 *
 * Gold appears here and only here: on the start button, the live dot, and the mode badge
 * while a session runs. That is what makes a glance at this pane a reliable answer to
 * "is it recording right now?".
 */
export function Sidebar({ mode, onModeChange, status, onRecorded }: Props) {
  const [live, setLive] = useState<LiveStatus | null>(null);
  const [apps, setApps] = useState<AudibleApp[]>([]);
  const [selectedApp, setSelectedApp] = useState<string>("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const pollRef = useRef<number | null>(null);

  const recording = live?.recording ?? false;

  const poll = useCallback(async () => {
    try {
      setLive(await invoke<LiveStatus>("recording_status"));
    } catch (problem) {
      console.error("could not read recording status", problem);
    }
  }, []);

  // Poll only while recording. There is nothing to watch when idle, and a background
  // timer that never stops is a battery cost for no information.
  useEffect(() => {
    void poll();
    if (!recording) {
      if (pollRef.current !== null) {
        window.clearInterval(pollRef.current);
        pollRef.current = null;
      }
      return;
    }
    pollRef.current = window.setInterval(() => void poll(), 500);
    return () => {
      if (pollRef.current !== null) window.clearInterval(pollRef.current);
      pollRef.current = null;
    };
  }, [recording, poll]);

  const refreshApps = useCallback(async () => {
    try {
      setApps(await invoke<AudibleApp[]>("list_audible_apps"));
    } catch (problem) {
      console.error("could not list audible apps", problem);
    }
  }, []);

  useEffect(() => {
    void refreshApps();
    const timer = window.setInterval(() => void refreshApps(), 4000);
    return () => window.clearInterval(timer);
  }, [refreshApps]);

  const start = async () => {
    setBusy(true);
    setError(null);
    try {
      const chosen = apps.find((app) => app.key === selectedApp);
      const event = await invoke<HearsayEvent>("start_recording", {
        request: {
          mode,
          pids: chosen ? chosen.pids : [],
          source_name: chosen ? chosen.name : null,
        },
      });
      onRecorded(event.id);
      await poll();
    } catch (problem) {
      setError(String((problem as { message?: string })?.message ?? problem));
    } finally {
      setBusy(false);
    }
  };

  const stop = async () => {
    setBusy(true);
    setError(null);
    try {
      const event = await invoke<HearsayEvent>("stop_recording");
      onRecorded(event.id);
      await poll();
    } catch (problem) {
      setError(String((problem as { message?: string })?.message ?? problem));
    } finally {
      setBusy(false);
    }
  };

  const blocked = !status?.helper_available || !status?.audio_permission;

  return (
    <aside className="sidebar">
      <div className="sidebar-title">Hearsay</div>

      <div className="sidebar-section">
        {recording ? (
          <>
            <button type="button" className="record-button stop" onClick={stop} disabled={busy}>
              Stop recording
            </button>
            <div className="live-indicator">
              <span className="recording-dot" aria-hidden />
              <span className="live-elapsed">{formatClock(live?.elapsed_ms ?? 0)}</span>
              <span className="spacer" />
              <LevelMeter peak={live?.peak ?? 0} />
            </div>
            {live && !live.has_audio ? (
              <p className="small" style={{ opacity: 0.8, margin: "2px 6px 0" }}>
                No audio captured yet. If this stays empty, the recording will be silent.
              </p>
            ) : null}
          </>
        ) : (
          <button
            type="button"
            className="record-button"
            onClick={start}
            disabled={busy || blocked}
            title={blocked ? "Finish setup before recording" : undefined}
          >
            Start recording
          </button>
        )}
        {error ? (
          <p className="small" style={{ margin: "4px 6px 0", opacity: 0.9 }}>
            {error}
          </p>
        ) : null}
      </div>

      <div className="sidebar-section">
        <div className="sidebar-label">Mode</div>
        <ModeToggle mode={mode} onChange={onModeChange} disabled={recording} />
        <p className="small" style={{ opacity: 0.6, padding: "2px 6px 0", margin: 0 }}>
          {mode === "listen_only"
            ? "System audio only. The microphone is never opened."
            : "Microphone on the left channel, system audio on the right."}
        </p>
      </div>

      <div className="sidebar-section">
        <div className="sidebar-label">Record from</div>
        <select
          className="app-select"
          value={selectedApp}
          disabled={recording}
          onChange={(changed) => setSelectedApp(changed.target.value)}
        >
          <option value="">Everything the machine plays</option>
          {apps.map((app) => (
            <option key={app.key} value={app.key}>
              {app.name}
            </option>
          ))}
        </select>
        <p className="small" style={{ opacity: 0.6, padding: "2px 6px 0", margin: 0 }}>
          {selectedApp
            ? "Only this app. Music playing alongside stays out."
            : "Everything, including music playing alongside."}
        </p>
      </div>
    </aside>
  );
}

/** A level meter, drawn in plain white — a meter is not a recording signal. */
function LevelMeter({ peak }: { peak: number }) {
  const bars = 5;
  const lit = Math.min(bars, Math.round(Math.sqrt(Math.max(0, peak)) * bars));
  return (
    <span className="meter" aria-label={`Level ${Math.round(peak * 100)} percent`}>
      {Array.from({ length: bars }, (_, index) => (
        <span key={index} className={`meter-bar${index < lit ? " lit" : ""}`} />
      ))}
    </span>
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
