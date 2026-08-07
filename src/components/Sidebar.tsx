import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import { formatClock } from "../format";
import type { AudibleApp, HearsayEvent, Mode, SystemStatus, View } from "../types";

interface LiveStatus {
  recording: boolean;
  event_id: number | null;
  mode: Mode | null;
  elapsed_ms: number;
  frames_written: number;
  peak: number;
  has_audio: boolean;
  silent_while_audio_playing: boolean;
  muted: boolean;
  echo: { lag_ms: number; correlation: number } | null;
}

interface Props {
  mode: Mode;
  onModeChange: (mode: Mode) => void;
  status: SystemStatus | null;
  onRecorded: (eventId: number) => void;
  onStatusChange: () => void;
  view: View;
  onViewChange: (view: View) => void;
}

/**
 * Pane one. Royal, always visible, and the only place recording is controlled from.
 *
 * Gold appears here and only here: on the start button, the live dot, and the mode badge
 * while a session runs. That is what makes a glance at this pane a reliable answer to
 * "is it recording right now?".
 */
export function Sidebar({ mode, onModeChange, status, onRecorded, view, onViewChange }: Props) {
  const [live, setLive] = useState<LiveStatus | null>(null);
  const [apps, setApps] = useState<AudibleApp[]>([]);
  const [selectedApp, setSelectedApp] = useState<string>("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [mute, setMute] = useState<{ muted: boolean; applicable: boolean }>({
    muted: false,
    applicable: false,
  });
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

  // The mute hotkey works while Hearsay is in the background, so mute state arrives as
  // an event rather than only from clicks in this window.
  useEffect(() => {
    const unlisten = listen<{ muted: boolean; applicable: boolean }>("mute", (message) =>
      setMute(message.payload),
    );
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, []);

  useEffect(() => {
    if (!recording) {
      setMute({ muted: false, applicable: false });
      return;
    }
    void invoke<{ muted: boolean; applicable: boolean }>("mute_state")
      .then(setMute)
      .catch(() => undefined);
  }, [recording]);

  const [scrubbed, setScrubbed] = useState<string | null>(null);
  const [meeting, setMeeting] = useState<{ id: string; title: string } | null>(null);
  const [dismissedSilence, setDismissedSilence] = useState(false);

  // The calendar offers; it never starts anything. A recorder that arms itself is one
  // the user cannot trust to be off.
  useEffect(() => {
    const unlisten = listen<{ id: string; title: string }>("calendar-arm", (message) =>
      setMeeting(message.payload),
    );
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, []);

  useEffect(() => {
    if (recording) setMeeting(null);
    else setDismissedSilence(false);
  }, [recording]);

  // A minute of recording with no audio at all. Not proof — a genuinely quiet room looks
  // the same — but worth interrupting for, because the alternative is discovering it
  // after the meeting. Dismissible, and never stops the recording on its own.
  const silentTooLong =
    recording && !dismissedSilence && (live?.elapsed_ms ?? 0) > 60_000 && !live?.has_audio;

  // The window is usually behind the meeting app, so an in-window banner would go
  // unseen. Ask the system to surface it.
  useEffect(() => {
    if (!silentTooLong) return;
    void (async () => {
      try {
        let allowed = await isPermissionGranted();
        if (!allowed) allowed = (await requestPermission()) === "granted";
        if (allowed) {
          sendNotification({
            title: "Hearsay has not heard anything",
            body: "A minute into this recording, no audio has been captured. Open Hearsay to stop it, or ignore this if the room is just quiet.",
          });
        }
      } catch {
        // No notification permission is survivable — the in-window alert still shows.
      }
    })();
  }, [silentTooLong]);

  // The scrub hotkey works while Hearsay is in the background, so confirmation has to
  // arrive by event. Silently erasing audio with no acknowledgement would leave the user
  // unsure whether it worked — exactly the wrong feeling for this feature.
  useEffect(() => {
    const unlisten = listen<{ erased_ms: number; window_seconds: number }>(
      "scrub",
      (message) => {
        const seconds = Math.round(message.payload.erased_ms / 1000);
        setScrubbed(
          seconds > 0
            ? `Erased the last ${seconds} second${seconds === 1 ? "" : "s"} of your microphone.`
            : "Nothing to erase yet.",
        );
        window.setTimeout(() => setScrubbed(null), 5000);
      },
    );
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, []);

  const scrub = async () => {
    try {
      await invoke("scrub_microphone");
    } catch (problem) {
      setError(String((problem as { message?: string })?.message ?? problem));
    }
  };

  const toggleMute = async () => {
    try {
      setMute(await invoke<{ muted: boolean; applicable: boolean }>("toggle_mute"));
    } catch (problem) {
      setError(String((problem as { message?: string })?.message ?? problem));
    }
  };

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


  // The title bar is transparent, so this bar doubles as the window's drag handle and
  // holds the space the traffic lights sit in.
  return (
    <header className="topbar" data-tauri-drag-region>
      <div className="brand" data-tauri-drag-region>
        Hearsay
      </div>

      <div className="bar-group">
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
            {mute.applicable ? (
              <button
                type="button"
                className={`button mute-button${mute.muted ? " engaged" : ""}`}
                onClick={toggleMute}
              >
                {mute.muted ? "Unmute microphone" : "Mute microphone"}
                <span className="shortcut-hint">⌘⇧M</span>
              </button>
            ) : null}
            {mute.applicable ? (
              <button type="button" className="button mute-button" onClick={scrub}>
                Erase last 60 seconds
                <span className="shortcut-hint">⌘⇧X</span>
              </button>
            ) : null}
            {scrubbed ? (
              <p className="small" style={{ opacity: 0.9, margin: "2px 6px 0" }}>
                {scrubbed}
              </p>
            ) : null}
            {mute.muted ? (
              <p className="small" style={{ opacity: 0.8, margin: "2px 6px 0" }}>
                Your microphone is writing silence. The other side is still being
                recorded, and the muted stretch is marked in the transcript.
              </p>
            ) : null}
            {silentTooLong ? (
              <div className="bar-alert">
                <span>Nothing heard for a minute.</span>
                <button type="button" className="button small" onClick={stop}>
                  End recording
                </button>
                <button
                  type="button"
                  className="button small"
                  onClick={() => setDismissedSilence(true)}
                >
                  Keep going
                </button>
              </div>
            ) : null}
            {live?.echo ? (
              <p className="small" style={{ opacity: 0.85, margin: "2px 6px 0" }}>
                The other side is coming back through your microphone. Headphones would
                keep the two voices apart in the transcript. Recording continues either
                way.
              </p>
            ) : null}
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
        {meeting && !recording ? (
          <div className="live-indicator" style={{ flexDirection: "column", alignItems: "stretch", gap: 6 }}>
            <span className="small">“{meeting.title}” is starting.</span>
            <div className="row">
              <button
                type="button"
                className="button"
                style={{ flex: 1 }}
                onClick={() => {
                  setMeeting(null);
                  void start();
                }}
              >
                Record it
              </button>
              <button type="button" className="button" onClick={() => setMeeting(null)}>
                Not now
              </button>
            </div>
          </div>
        ) : null}
        {error ? (
          <p className="small" style={{ margin: "4px 6px 0", opacity: 0.9 }}>
            {error}
          </p>
        ) : null}
      </div>

      <div className="bar-group">
        <ModeToggle mode={mode} onChange={onModeChange} disabled={recording} />
      </div>

      <div className="bar-group">
        <select
          className="app-select"
          value={selectedApp}
          disabled={recording}
          onChange={(changed) => setSelectedApp(changed.target.value)}
        >
          <option value="">Everything the machine plays</option>
          {apps.map((app) => (
            <option key={app.key} value={app.key}>
              {app.is_playing ? `${app.name} — playing now` : app.name}
            </option>
          ))}
        </select>
      </div>

      {/* The mode and source descriptions used to sit under each control. In a bar there
          is no room, and they said the same thing every time — one line that reflects the
          current choice carries the same information in a fraction of the space. */}
      <p className="bar-hint">
        {mode === "listen_only"
          ? "System audio only — the microphone is never opened."
          : "Microphone on the left, system audio on the right."}
        {selectedApp ? " Only the chosen app." : " Everything the machine plays."}
      </p>

      <nav className="topbar-nav" aria-label="Views">
        <button
          type="button"
          className={`icon-nav${view === "recordings" ? " active" : ""}`}
          onClick={() => onViewChange("recordings")}
          title="Recordings"
          aria-label="Recordings"
        >
          <RecordingsIcon />
        </button>
        <button
          type="button"
          className={`icon-nav${view === "settings" ? " active" : ""}`}
          onClick={() => onViewChange("settings")}
          title="Settings"
          aria-label="Settings"
        >
          <SettingsIcon />
        </button>
      </nav>
    </header>
  );
}

/* Icons are inline SVG: no icon font to load, nothing fetched, and they inherit the
   current colour so the active state needs no second asset. */

function RecordingsIcon() {
  return (
    <svg viewBox="0 0 16 16" width="15" height="15" fill="none" aria-hidden>
      <rect x="2" y="3" width="12" height="10" rx="2" stroke="currentColor" strokeWidth="1.3" />
      <path d="M5 6.5h6M5 9.5h4" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" />
    </svg>
  );
}

function SettingsIcon() {
  // A real cog: a ring with lobed teeth. The earlier version was a circle with straight
  // radiating lines, which reads as a sun rather than a gear.
  return (
    <svg viewBox="0 0 24 24" width="16" height="16" fill="none" aria-hidden>
      <circle cx="12" cy="12" r="3" stroke="currentColor" strokeWidth="1.8" />
      <path
        d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"
        stroke="currentColor"
        strokeWidth="1.8"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
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
