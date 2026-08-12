/** Shapes shared with the Rust side. These mirror the `Serialize` structs exactly. */

export type Mode = "listen_only" | "conversation";

export type Channel = "mic" | "system";

/** Which pane three is showing. Not a route — there is no navigation stack. */
export type View = "recordings" | "settings";

export interface SystemStatus {
  helper_available: boolean;
  helper_path: string | null;
  audio_permission: boolean;
  transcription_available: boolean;
  problem: string | null;
}

/** What Settings shows and the detail pane needs to know about the summary provider. */
export interface Settings {
  has_api_key: boolean;
  has_gemini_key: boolean;
  provider: string;
  speaker_name: string;
  data_dir: string;
  recordings_dir: string;
  models_dir: string;
  transcription_available: boolean;
}

/** Whether the provider currently selected has a key — mirrors `has_summary_key` in Rust. */
export function hasSummaryKey(settings: Settings | null): boolean {
  if (!settings) return false;
  return settings.provider === "gemini" ? settings.has_gemini_key : settings.has_api_key;
}

export interface AudibleApp {
  key: string;
  name: string;
  bundle_id: string | null;
  pids: number[];
  is_playing: boolean;
}

export interface HearsayEvent {
  id: number;
  title: string;
  ai_title: string | null;
  calendar_event_id: string | null;
  started_at: string;
  ended_at: string | null;
  mode: Mode;
  audio_path: string | null;
  summary_md: string | null;
  model_used: string | null;
  created_at: string;
  /** When a transcription pass last finished. Null means one never has — which is not the
   *  same as the recording having nothing to say. */
  transcribed_at: string | null;
}

export interface Segment {
  id: number;
  event_id: number;
  channel: Channel;
  start_ms: number;
  end_ms: number;
  text: string;
}

export interface MuteSpan {
  id: number;
  event_id: number;
  start_ms: number;
  end_ms: number;
}

/** Where a saved copy landed, and how big it turned out. */
export interface ExportedAudio {
  path: string;
  bytes: number;
}

export interface SearchHit {
  segment_id: number;
  event_id: number;
  event_title: string;
  started_at: string;
  channel: Channel;
  start_ms: number;
  end_ms: number;
  text: string;
  snippet: string;
}
