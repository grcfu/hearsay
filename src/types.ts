/** Shapes shared with the Rust side. These mirror the `Serialize` structs exactly. */

export type Mode = "listen_only" | "conversation";

export type Channel = "mic" | "system";

export interface SystemStatus {
  helper_available: boolean;
  helper_path: string | null;
  audio_permission: boolean;
  transcription_available: boolean;
  problem: string | null;
}

export interface AudibleApp {
  key: string;
  name: string;
  bundle_id: string | null;
  pids: number[];
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
