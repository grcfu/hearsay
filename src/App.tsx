import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Sidebar } from "./components/Sidebar";
import { EventList } from "./components/EventList";
import { Detail } from "./components/Detail";
import { SetupBanner } from "./components/SetupBanner";
import type { Mode, SystemStatus } from "./types";

/**
 * The whole application: three panes, side by side, always visible.
 *
 * There is no router and no navigation stack. Selecting an event changes the detail
 * pane and nothing else, which is why there is never a back button to look for.
 */
export function App() {
  const [status, setStatus] = useState<SystemStatus | null>(null);
  const [selectedId, setSelectedId] = useState<number | null>(null);
  const [refreshToken, setRefreshToken] = useState(0);

  // listen_only on every launch, deliberately not persisted. Whatever mode was used
  // last time must never silently arm the microphone this time.
  const [mode, setMode] = useState<Mode>("listen_only");

  const refresh = useCallback(() => setRefreshToken((token) => token + 1), []);

  const loadStatus = useCallback(async () => {
    try {
      setStatus(await invoke<SystemStatus>("system_status"));
    } catch (error) {
      console.error("could not read system status", error);
    }
  }, []);

  useEffect(() => {
    void loadStatus();
  }, [loadStatus]);

  return (
    <div className="app">
      <Sidebar
        mode={mode}
        onModeChange={setMode}
        status={status}
        onRecorded={(eventId) => {
          setSelectedId(eventId);
          refresh();
        }}
        onStatusChange={loadStatus}
      />

      <EventList
        selectedId={selectedId}
        onSelect={setSelectedId}
        refreshToken={refreshToken}
      />

      <div className="detail">
        <SetupBanner status={status} onRecheck={loadStatus} />
        <Detail eventId={selectedId} onChanged={refresh} />
      </div>
    </div>
  );
}
