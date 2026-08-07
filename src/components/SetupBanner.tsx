import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { SystemStatus } from "../types";

interface Props {
  status: SystemStatus | null;
  onRecheck: () => void;
}

/**
 * Says what is missing, and what to do about it.
 *
 * Each prerequisite fails independently and has its own fix, so they are reported one at
 * a time rather than collapsed into a single "setup incomplete". The permission case
 * gets the most words because its failure mode is a recording that runs happily and
 * captures nothing at all.
 */
export function SetupBanner({ status, onRecheck }: Props) {
  const [requesting, setRequesting] = useState(false);

  if (!status) return null;

  const requestPermission = async () => {
    setRequesting(true);
    try {
      await invoke<boolean>("request_audio_permission");
    } catch (error) {
      console.error("permission request failed", error);
    } finally {
      setRequesting(false);
      onRecheck();
    }
  };

  if (!status.helper_available) {
    return (
      <Banner problem>
        <div>
          <strong>The audio helper is missing.</strong> Build it with{" "}
          <code className="mono">./helper/build.sh</code>, then re-check.
          {status.problem ? (
            <div className="small muted" style={{ marginTop: 4 }}>
              {status.problem}
            </div>
          ) : null}
        </div>
        <button type="button" className="button" onClick={onRecheck}>
          Re-check
        </button>
      </Banner>
    );
  }

  if (!status.audio_permission) {
    return (
      <Banner problem>
        <div>
          <strong>macOS reports that system audio recording is not permitted.</strong>{" "}
          You can still record — if it captures only silence, this is why.
          <div className="small muted" style={{ marginTop: 4 }}>
            Press Ask macOS. If no prompt appears, the permission is switched on for an
            older build: macOS ties it to a signature that changes every time the app is
            rebuilt. Run <code>./install.sh --reset-tcc</code> from the repo to clear it,
            or remove Hearsay from System Settings → Privacy &amp; Security → Screen
            &amp; System Audio Recording and add it back with +.
          </div>
        </div>
        <button
          type="button"
          className="button"
          onClick={requestPermission}
          disabled={requesting}
        >
          {requesting ? "Asking…" : "Ask macOS"}
        </button>
      </Banner>
    );
  }

  if (!status.transcription_available) {
    return (
      <Banner>
        <div>
          <strong>Transcription is not set up yet.</strong> Recording and playback work;
          recordings stay untranscribed until you run{" "}
          <code className="mono">./python/setup_venv.sh</code>.
        </div>
        <button type="button" className="button" onClick={onRecheck}>
          Re-check
        </button>
      </Banner>
    );
  }

  return null;
}

function Banner({
  children,
  problem = false,
}: {
  children: React.ReactNode;
  problem?: boolean;
}) {
  return (
    <div style={{ padding: "16px 24px 0" }}>
      <div className={`banner${problem ? " problem" : ""}`}>{children}</div>
    </div>
  );
}
