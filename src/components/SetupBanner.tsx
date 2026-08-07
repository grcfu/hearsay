import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  isPermissionGranted,
  requestPermission as requestNotifications,
  sendNotification,
} from "@tauri-apps/plugin-notification";
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
interface UpdateStatus {
  behind: boolean;
  commits: number;
  built: string;
  available: string;
  repo: string;
}

export function SetupBanner({ status, onRecheck }: Props) {
  const [requesting, setRequesting] = useState(false);
  const [update, setUpdate] = useState<UpdateStatus | null>(null);
  const [dismissedUpdate, setDismissedUpdate] = useState(false);

  // Checked once on launch, against the checkout on this machine — no server, no
  // version endpoint, nothing fetched. See commands/version.rs.
  useEffect(() => {
    void (async () => {
      try {
        const found = await invoke<UpdateStatus>("update_status");
        if (!found.behind) return;
        setUpdate(found);
        // Notify too: a new build usually matters most right after a `git pull`, when
        // the window is not the thing being looked at.
        let allowed = await isPermissionGranted();
        if (!allowed) allowed = (await requestNotifications()) === "granted";
        if (allowed) {
          sendNotification({
            title: "A newer build of Hearsay is ready",
            body: `${found.commits} change${found.commits === 1 ? "" : "s"} since this one. Run ./install.sh in ${found.repo} to update.`,
          });
        }
      } catch {
        // Not in a checkout, or no git. Nothing to say.
      }
    })();
  }, []);

  if (update && !dismissedUpdate) {
    return (
      <Banner>
        <div>
          <strong>
            A newer build is available — {update.commits} change
            {update.commits === 1 ? "" : "s"} since the one you are running.
          </strong>
          <div className="small muted" style={{ marginTop: 4 }}>
            Running <code>{update.built}</code>, checkout is at{" "}
            <code>{update.available}</code>. To update, run this in a terminal:
            <div className="mono" style={{ marginTop: 6 }}>
              cd {update.repo} &amp;&amp; ./install.sh
            </div>
            Your recordings are not touched, and the permission carries over.
          </div>
        </div>
        <button
          type="button"
          className="button"
          onClick={() => setDismissedUpdate(true)}
        >
          Later
        </button>
      </Banner>
    );
  }

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
