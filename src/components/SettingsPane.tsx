import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import type { SystemStatus } from "../types";

interface Settings {
  has_api_key: boolean;
  data_dir: string;
  recordings_dir: string;
  models_dir: string;
  transcription_available: boolean;
}

interface Props {
  status: SystemStatus | null;
  onStatusChange: () => void;
}

/**
 * Settings.
 *
 * The API key is write-only from here: it is saved into the Keychain and never read back
 * into the interface. There is no field showing the current key because there is no code
 * path that could fill one in.
 */
export function SettingsPane({ status, onStatusChange }: Props) {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [draftKey, setDraftKey] = useState("");
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setSettings(await invoke<Settings>("settings"));
    } catch (problem) {
      setError(String((problem as { message?: string })?.message ?? problem));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const save = async () => {
    setError(null);
    setMessage(null);
    try {
      await invoke("save_api_key", { key: draftKey });
      // Clear immediately: the key should not linger in a React state tree or in the
      // DOM any longer than the moment it takes to hand it over.
      setDraftKey("");
      setMessage("Saved to your Keychain.");
      await load();
      onStatusChange();
    } catch (problem) {
      setError(String((problem as { message?: string })?.message ?? problem));
    }
  };

  const clear = async () => {
    setError(null);
    setMessage(null);
    try {
      await invoke("clear_api_key");
      setMessage("Removed. Summaries are off; everything else still works.");
      await load();
      onStatusChange();
    } catch (problem) {
      setError(String((problem as { message?: string })?.message ?? problem));
    }
  };

  return (
    <>
      <div className="detail-header">
        <h1 style={{ padding: "2px 0" }}>Settings</h1>
        <div className="detail-meta">
          <span>Everything stays on this machine.</span>
        </div>
        <div style={{ height: 14 }} />
      </div>

      <div className="detail-body" style={{ maxWidth: 700 }}>
        <section style={{ marginBottom: 28 }}>
          <h2>Summaries</h2>
          <p className="muted small" style={{ marginTop: 4 }}>
            Hearsay works fully without a key: it records, transcribes on this machine,
            searches, and plays back. A key adds AI titles and summaries, and is the only
            thing that ever sends text off this machine — when you ask for a summary, and
            not otherwise.
          </p>

          <div className="panel" style={{ marginTop: 12 }}>
            <div className="row" style={{ marginBottom: 10 }}>
              <strong>Anthropic API key</strong>
              <span className="spacer" />
              <span className="small muted">
                {settings?.has_api_key ? "Stored in your Keychain" : "Not set"}
              </span>
            </div>

            <div className="row">
              <input
                className="search-input"
                type="password"
                autoComplete="off"
                spellCheck={false}
                placeholder={settings?.has_api_key ? "Replace the stored key" : "sk-ant-…"}
                value={draftKey}
                onChange={(changed) => setDraftKey(changed.target.value)}
              />
              <button
                type="button"
                className="button primary"
                disabled={!draftKey.trim()}
                onClick={save}
              >
                Save
              </button>
              {settings?.has_api_key ? (
                <button type="button" className="button destructive" onClick={clear}>
                  Remove
                </button>
              ) : null}
            </div>

            {message ? (
              <p className="small muted" style={{ margin: "8px 0 0" }}>
                {message}
              </p>
            ) : null}
            {error ? (
              <p className="small" style={{ margin: "8px 0 0", color: "var(--danger)" }}>
                {error}
              </p>
            ) : null}

            <p className="small muted" style={{ margin: "10px 0 0" }}>
              Keys are stored in the macOS Keychain, never in a file or in the database.{" "}
              <button
                type="button"
                className="link"
                onClick={() => void openUrl("https://console.anthropic.com/settings/keys")}
              >
                Get a key
              </button>
            </p>
          </div>
        </section>

        <section style={{ marginBottom: 28 }}>
          <h2>Capture</h2>
          <div className="panel" style={{ marginTop: 12 }}>
            <CheckRow
              label="Audio helper"
              ok={status?.helper_available ?? false}
              okText="Built and ready"
              badText="Missing — run ./helper/build.sh"
            />
            <CheckRow
              label="System audio permission"
              ok={status?.audio_permission ?? false}
              okText="Granted"
              badText="Not granted — recordings would be silent"
            />
            <CheckRow
              label="Transcription"
              ok={settings?.transcription_available ?? false}
              okText="Installed"
              badText="Not set up — run ./python/setup_venv.sh"
            />
          </div>
        </section>

        <section>
          <h2>On disk</h2>
          <div className="panel" style={{ marginTop: 12 }}>
            <PathRow label="Recordings" path={settings?.recordings_dir} />
            <PathRow label="Database" path={settings?.data_dir} />
            <PathRow label="Speech models" path={settings?.models_dir} />
          </div>
        </section>
      </div>
    </>
  );
}

function CheckRow({
  label,
  ok,
  okText,
  badText,
}: {
  label: string;
  ok: boolean;
  okText: string;
  badText: string;
}) {
  return (
    <div className="row" style={{ padding: "5px 0" }}>
      <span style={{ minWidth: 190 }}>{label}</span>
      <span className="small" style={{ color: ok ? "var(--sapphire)" : "var(--danger)" }}>
        {ok ? okText : badText}
      </span>
    </div>
  );
}

function PathRow({ label, path }: { label: string; path?: string }) {
  return (
    <div className="row" style={{ padding: "5px 0", alignItems: "flex-start" }}>
      <span style={{ minWidth: 130 }}>{label}</span>
      <span className="small muted mono" style={{ wordBreak: "break-all" }}>
        {path ?? "—"}
      </span>
      {path ? (
        <>
          <span className="spacer" />
          <button
            type="button"
            className="button small"
            onClick={() => void openUrl(`file://${path}`)}
          >
            Open
          </button>
        </>
      ) : null}
    </div>
  );
}
