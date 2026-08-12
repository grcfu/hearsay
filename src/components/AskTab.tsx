import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ask } from "@tauri-apps/plugin-dialog";
import { hasSummaryKey } from "../types";
import type { Settings } from "../types";

interface ChatMessage {
  id: number;
  event_id: number;
  role: "user" | "assistant";
  content: string;
  created_at: string;
}

interface Props {
  eventId: number;
  segmentCount: number;
  /** Null while still loading — which is not the same as having no key configured. */
  settings: Settings | null;
  /** Jump the player to a timestamp the answer cited. */
  onSeek: (ms: number) => void;
}

/**
 * Asking questions about one recording.
 *
 * The point is not a chat window for its own sake — it is that finding one detail in an
 * hour of transcript should not mean pasting the whole thing into somebody else's AI. That
 * would send the same text to a service the user did not choose, under an account they may
 * not control. Here it goes to the provider and key already configured in Settings, and
 * only when a question is actually sent.
 *
 * The conversation is stored, so coming back to a recording shows what was already asked.
 */
export function AskTab({ eventId, segmentCount, settings, onSeek }: Props) {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [question, setQuestion] = useState("");
  const [asking, setAsking] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const endRef = useRef<HTMLDivElement | null>(null);

  const load = useCallback(async () => {
    try {
      setMessages(await invoke<ChatMessage[]>("chat_history", { eventId }));
    } catch (problem) {
      setError(String((problem as { message?: string })?.message ?? problem));
    }
  }, [eventId]);

  useEffect(() => {
    void load();
    setQuestion("");
    setError(null);
    setAsking(false);
  }, [load]);

  // The answer arrives on a worker thread, so this listens rather than awaiting.
  useEffect(() => {
    const unlisten = listen<{
      event_id: number;
      stage: string;
      message?: string;
      question?: string;
    }>("chat", (message) => {
      if (message.payload.event_id !== eventId) return;
      if (message.payload.stage === "answered") {
        setAsking(false);
        void load();
      } else if (message.payload.stage === "failed") {
        setAsking(false);
        setError(message.payload.message ?? "That question could not be answered.");
        // The question was withdrawn so it is not replayed as history. Put it back in the
        // box rather than making it be retyped.
        if (message.payload.question) setQuestion(message.payload.question);
        void load();
      }
    });
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, [eventId, load]);

  // Follow the conversation down as it grows, but only once there is something to follow.
  useEffect(() => {
    if (messages.length > 0) endRef.current?.scrollIntoView({ block: "nearest" });
  }, [messages.length, asking]);

  const send = async () => {
    const trimmed = question.trim();
    if (!trimmed || asking) return;
    setAsking(true);
    setError(null);
    // Cleared optimistically so the box is ready for the next question; restored from the
    // failure event if the answer never comes.
    setQuestion("");
    // Shown immediately. The real row is already stored, and reloading would work, but
    // waiting a round trip to see your own question makes the app feel like it hung.
    setMessages((current) => [
      ...current,
      {
        id: -1,
        event_id: eventId,
        role: "user",
        content: trimmed,
        created_at: new Date().toISOString(),
      },
    ]);
    try {
      await invoke("ask_question", { eventId, question: trimmed });
    } catch (problem) {
      setAsking(false);
      setError(String((problem as { message?: string })?.message ?? problem));
      setQuestion(trimmed);
      void load();
    }
  };

  const clear = async () => {
    const confirmed = await ask(
      "Forget everything asked about this recording? The transcript, summary and audio are untouched.",
      { title: "Clear questions", kind: "warning", okLabel: "Clear", cancelLabel: "Keep" },
    );
    if (!confirmed) return;
    try {
      await invoke("clear_chat", { eventId });
      await load();
    } catch (problem) {
      setError(String((problem as { message?: string })?.message ?? problem));
    }
  };

  if (segmentCount === 0) {
    return (
      <div className="panel">
        <p style={{ marginTop: 0 }}>Nothing to ask about yet.</p>
        <p className="small muted">
          Questions are answered from the transcript, so this needs a transcript first.
        </p>
      </div>
    );
  }

  // Still reading Settings. Saying "no API key" here would be a guess presented as a
  // fact, and it is the guess that tells the user the feature is unavailable.
  if (!settings) {
    return <p className="muted">Loading…</p>;
  }

  if (!hasSummaryKey(settings)) {
    return (
      <div className="panel">
        <p style={{ marginTop: 0 }}>
          No {settings.provider === "gemini" ? "Gemini" : "Anthropic"} key set.
        </p>
        <p className="small muted">
          Add one in Settings to ask questions about a recording. Recording, transcription
          and search all work without it.
        </p>
      </div>
    );
  }

  return (
    <div className="ask">
      {messages.length === 0 && !asking ? (
        <div className="panel" style={{ marginBottom: 14 }}>
          <p style={{ marginTop: 0 }}>Ask about this recording.</p>
          <p className="small muted">
            Answered from the transcript only, with timestamps you can click. This is the
            one other thing that leaves your machine — the transcript of this recording
            goes to {settings.provider === "gemini" ? "Gemini" : "Anthropic"} with your
            key, and only when you send a question.
          </p>
        </div>
      ) : null}

      <div className="ask-thread">
        {messages.map((message, index) => (
          <div
            key={message.id === -1 ? `pending-${index}` : message.id}
            className={`ask-turn ask-${message.role}`}
          >
            <div className="ask-role">{message.role === "user" ? "You asked" : "Answer"}</div>
            <div className="ask-content">
              {message.role === "assistant"
                ? withTimestamps(message.content, onSeek)
                : message.content}
            </div>
          </div>
        ))}
        {asking ? (
          <div className="ask-turn ask-assistant">
            <div className="ask-role">Answer</div>
            <div className="ask-content muted">Reading the transcript…</div>
          </div>
        ) : null}
        <div ref={endRef} />
      </div>

      {error ? (
        <div className="banner problem" style={{ margin: "12px 0" }}>
          {error}
        </div>
      ) : null}

      <div className="ask-compose">
        <textarea
          className="ask-input"
          value={question}
          rows={2}
          placeholder="What did they say about the deadline?"
          onChange={(changed) => setQuestion(changed.target.value)}
          onKeyDown={(pressed) => {
            // Enter sends, shift-enter makes a new line. A question is usually one line,
            // so making the common case need a modifier would be backwards.
            if (pressed.key === "Enter" && !pressed.shiftKey) {
              pressed.preventDefault();
              void send();
            }
          }}
        />
        <div className="row">
          <button
            type="button"
            className="button primary"
            onClick={() => void send()}
            disabled={asking || !question.trim()}
          >
            {asking ? "Asking…" : "Ask"}
          </button>
          {messages.length > 0 ? (
            <button type="button" className="button" onClick={clear} disabled={asking}>
              Clear
            </button>
          ) : null}
          <span className="small muted">Enter to send, shift-enter for a new line.</span>
        </div>
      </div>
    </div>
  );
}

/**
 * Turns `[MM:SS]` in an answer into buttons that seek the audio.
 *
 * The model is asked to cite timestamps precisely so an answer can be checked against the
 * recording rather than taken on trust. Making them clickable is what turns that from a
 * promise into something one keystroke away.
 *
 * Everything else is rendered as a text node, so nothing in an answer can inject markup.
 */
function withTimestamps(text: string, onSeek: (ms: number) => void) {
  const pattern = /\[(\d{1,2}):([0-5]\d)\]/g;
  const parts: React.ReactNode[] = [];
  let last = 0;
  let match: RegExpExecArray | null;
  let key = 0;

  while ((match = pattern.exec(text)) !== null) {
    if (match.index > last) parts.push(text.slice(last, match.index));
    const ms = (Number(match[1]) * 60 + Number(match[2])) * 1000;
    parts.push(
      <button
        type="button"
        key={`at-${key++}`}
        className="ask-timestamp mono"
        onClick={() => onSeek(ms)}
        title="Play from here"
      >
        {match[0]}
      </button>,
    );
    last = match.index + match[0].length;
  }
  if (last < text.length) parts.push(text.slice(last));
  return parts;
}
