import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import type { Settings, SystemStatus } from "../types";

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
  const [draftName, setDraftName] = useState("");
  // Set once from the loaded settings and then left alone, so typing is not fought by a
  // reload landing mid-edit.
  const [, setNameLoaded] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const loaded = await invoke<Settings>("settings");
      setSettings(loaded);
      setNameLoaded((already) => {
        if (!already) setDraftName(loaded.speaker_name);
        return true;
      });
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
      await invoke(
        settings?.provider === "gemini" ? "save_gemini_key" : "save_api_key",
        { key: draftKey },
      );
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
      await invoke(settings?.provider === "gemini" ? "clear_gemini_key" : "clear_api_key");
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
              <strong>Call me</strong>
              <span className="spacer" />
              <input
                className="search-input"
                style={{ maxWidth: 220 }}
                type="text"
                autoComplete="off"
                spellCheck={false}
                placeholder="You"
                value={draftName}
                onChange={(changed) => setDraftName(changed.target.value)}
                onBlur={async () => {
                  if (draftName === (settings?.speaker_name ?? "")) return;
                  try {
                    await invoke("set_speaker_name", { name: draftName });
                    await load();
                  } catch (problem) {
                    setError(String((problem as { message?: string })?.message ?? problem));
                  }
                }}
              />
            </div>
            <p className="muted small" style={{ marginTop: 0, marginBottom: 14 }}>
              Summaries and action items will use this name instead of &ldquo;you&rdquo;.
              Leave it empty to keep &ldquo;you&rdquo;.
            </p>

            <div className="row" style={{ marginBottom: 10 }}>
              <strong>Summaries are written by</strong>
              <span className="spacer" />
              <div className="mode-toggle" style={{ background: "var(--shellstone)" }}>
                {(["anthropic", "gemini"] as const).map((name) => (
                  <button
                    key={name}
                    type="button"
                    className={`mode-option${settings?.provider === name ? " selected" : ""}`}
                    style={{ color: settings?.provider === name ? undefined : "var(--royal)" }}
                    onClick={async () => {
                      await invoke("set_summary_provider", { provider: name });
                      await load();
                    }}
                  >
                    {name === "anthropic" ? "Claude" : "Gemini"}
                  </button>
                ))}
              </div>
            </div>

            <div className="row">
              <input
                className="search-input"
                type="password"
                autoComplete="off"
                spellCheck={false}
                placeholder={
                  settings?.provider === "gemini"
                    ? settings?.has_gemini_key
                      ? "Replace the stored Gemini key"
                      : "AIza…"
                    : settings?.has_api_key
                      ? "Replace the stored Claude key"
                      : "sk-ant-…"
                }
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
              {(settings?.provider === "gemini" ? settings?.has_gemini_key : settings?.has_api_key) ? (
                <button type="button" className="button destructive" onClick={clear}>
                  Remove
                </button>
              ) : null}
            </div>

            <p className="small muted" style={{ margin: "8px 0 0" }}>
              {settings?.provider === "gemini"
                ? settings?.has_gemini_key
                  ? "Gemini key stored in your Keychain."
                  : "No Gemini key yet."
                : settings?.has_api_key
                  ? "Claude key stored in your Keychain."
                  : "No Claude key yet."}
            </p>

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
                onClick={() =>
                  void openUrl(
                    settings?.provider === "gemini"
                      ? "https://aistudio.google.com/apikey"
                      : "https://console.anthropic.com/settings/keys",
                  )
                }
              >
                Get a {settings?.provider === "gemini" ? "Gemini" : "Claude"} key
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

        <section style={{ marginBottom: 28 }}>
          <h2>Calendar</h2>
          <p className="muted small" style={{ marginTop: 4 }}>
            Optional. Connecting Google Calendar lets Hearsay name recordings after the
            meeting they belong to and offer to start recording when one begins. It is
            read-only and reads titles and times only — nothing is ever uploaded, and no
            recording, transcript or summary is sent to Google.
          </p>
          <CalendarSection />
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

/**
 * Connecting the calendar.
 *
 * Hearsay ships no OAuth client of its own — embedding one would route every user's
 * calendar access through credentials they do not control. The user creates a Desktop
 * client in Google Cloud Console and pastes it here; it is stored in the Keychain.
 */
function CalendarSection() {
  const [connected, setConnected] = useState(false);
  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const status = await invoke<{ connected: boolean }>("calendar_status");
      setConnected(status.connected);
    } catch {
      setConnected(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const connect = async () => {
    setBusy(true);
    setFailure(null);
    setNote("A browser window has opened. Finish signing in to Google there.");
    try {
      await invoke("connect_calendar", { clientId, clientSecret });
      setClientId("");
      setClientSecret("");
      setNote("Connected. Hearsay can read your calendar; it cannot change it.");
      await load();
    } catch (problem) {
      setNote(null);
      setFailure(String((problem as { message?: string })?.message ?? problem));
    } finally {
      setBusy(false);
    }
  };

  const disconnect = async () => {
    try {
      await invoke("disconnect_calendar");
      setNote("Disconnected. Revoke Hearsay in your Google account to remove it there too.");
      await load();
    } catch (problem) {
      setFailure(String((problem as { message?: string })?.message ?? problem));
    }
  };

  return (
    <div className="panel" style={{ marginTop: 12 }}>
      <div className="row" style={{ marginBottom: 10 }}>
        <strong>Google Calendar</strong>
        <span className="spacer" />
        <span className="small muted">{connected ? "Connected, read-only" : "Not connected"}</span>
      </div>

      {connected ? (
        <button type="button" className="button destructive" onClick={disconnect}>
          Disconnect
        </button>
      ) : (
        <>
          <p className="small muted" style={{ margin: "0 0 4px" }}>
            Google needs you to create your own credentials. Hearsay ships none of its
            own, so your calendar access runs through a client you control and can revoke
            at any time. Takes about five minutes, once.
          </p>

          <ol className="steps">
            <li>
              <span>
                <strong>Create a project.</strong> Sign in with the account whose calendar
                you want to read, then use the project dropdown at the top left → New
                project. Name it anything.
                <button
                  type="button"
                  className="link step-note"
                  onClick={() => void openUrl("https://console.cloud.google.com/projectcreate")}
                >
                  Open project creation →
                </button>
              </span>
            </li>
            <li>
              <span>
                <strong>Enable the Google Calendar API</strong> and click Enable.
                <button
                  type="button"
                  className="link step-note"
                  onClick={() =>
                    void openUrl(
                      "https://console.cloud.google.com/apis/library/calendar-json.googleapis.com",
                    )
                  }
                >
                  Open the Calendar API page →
                </button>
              </span>
            </li>
            <li>
              <span>
                <strong>Set up the consent screen.</strong> Choose <em>External</em>, fill
                in a name and your email, and add your own address under Test users. You
                can skip the scopes step — Hearsay asks for what it needs at sign-in.
                <button
                  type="button"
                  className="link step-note"
                  onClick={() => void openUrl("https://console.cloud.google.com/auth/overview")}
                >
                  Open the consent screen →
                </button>
              </span>
            </li>
            <li>
              <span>
                <strong>Publish the app</strong> on that same screen.
                <span className="step-note">
                  This matters: while the status is “Testing”, Google expires the
                  connection after 7 days and you would have to reconnect every week.
                  Publishing stops that. Google will warn that the app is unverified when
                  you sign in — click Advanced, then “Go to Hearsay (unsafe)”. The warning
                  is about strangers trusting your app; you are the only person who will
                  ever use it.
                </span>
              </span>
            </li>
            <li>
              <span>
                <strong>Create the credential.</strong> Credentials → Create credentials →
                OAuth client ID → application type <strong>Desktop app</strong>. Copy the
                client ID and client secret into the fields below.
                <button
                  type="button"
                  className="link step-note"
                  onClick={() => void openUrl("https://console.cloud.google.com/apis/credentials")}
                >
                  Open Credentials →
                </button>
              </span>
            </li>
          </ol>

          <p className="callout">
            You do <strong>not</strong> need to configure a redirect URI. Desktop clients
            accept <code>127.0.0.1</code> on any port, which is what Hearsay uses — it
            opens a local server on a random free port to catch the sign-in, so nothing
            passes through a URL bar. Both values go straight into your Keychain.
          </p>
          <div className="row" style={{ marginBottom: 8 }}>
            <input
              className="search-input"
              placeholder="Client ID"
              autoComplete="off"
              value={clientId}
              onChange={(changed) => setClientId(changed.target.value)}
            />
          </div>
          <div className="row">
            <input
              className="search-input"
              type="password"
              placeholder="Client secret"
              autoComplete="off"
              value={clientSecret}
              onChange={(changed) => setClientSecret(changed.target.value)}
            />
            <button
              type="button"
              className="button primary"
              disabled={busy || !clientId.trim() || !clientSecret.trim()}
              onClick={connect}
            >
              {busy ? "Waiting…" : "Connect"}
            </button>
          </div>
        </>
      )}

      {note ? (
        <p className="small muted" style={{ margin: "8px 0 0" }}>
          {note}
        </p>
      ) : null}
      {failure ? (
        <p className="small" style={{ margin: "8px 0 0", color: "var(--danger)" }}>
          {failure}
        </p>
      ) : null}
    </div>
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
