//! Storage.
//!
//! Segments are the source of truth. Everything else — summaries, titles, the search
//! index — is derived and can be rebuilt from them without re-transcribing, which is why
//! they are stored as rows rather than as a blob of text hung off the event.
//!
//! Schema changes go in [`MIGRATIONS`] and are applied in order, tracked by SQLite's
//! own `user_version`. Migrations are never edited once shipped.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

/// Ordered schema migrations. Index + 1 is the resulting `user_version`.
const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    r#"
    CREATE TABLE events (
        id                INTEGER PRIMARY KEY,
        title             TEXT    NOT NULL,
        ai_title          TEXT,
        calendar_event_id TEXT,
        started_at        TEXT    NOT NULL,
        ended_at          TEXT,
        mode              TEXT    NOT NULL CHECK (mode IN ('listen_only', 'conversation')),
        audio_path        TEXT,
        summary_md        TEXT,
        model_used        TEXT,
        created_at        TEXT    NOT NULL
    );

    CREATE TABLE segments (
        id        INTEGER PRIMARY KEY,
        event_id  INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
        channel   TEXT    NOT NULL CHECK (channel IN ('mic', 'system')),
        start_ms  INTEGER NOT NULL,
        end_ms    INTEGER NOT NULL,
        text      TEXT    NOT NULL
    );

    CREATE TABLE mute_spans (
        id        INTEGER PRIMARY KEY,
        event_id  INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
        start_ms  INTEGER NOT NULL,
        end_ms    INTEGER NOT NULL
    );

    CREATE INDEX idx_events_started    ON events(started_at DESC);
    CREATE INDEX idx_segments_event    ON segments(event_id, start_ms);
    CREATE INDEX idx_mute_spans_event  ON mute_spans(event_id, start_ms);
    "#,
    // 2: full-text search over segment text.
    //
    // An external-content table: the index stores no copy of the text, only the terms,
    // and reads through to `segments` for anything it needs to display. Triggers keep it
    // in step, so nothing in the app has to remember to update the index.
    r#"
    CREATE VIRTUAL TABLE segments_fts USING fts5(
        text,
        content = 'segments',
        content_rowid = 'id',
        tokenize = 'unicode61 remove_diacritics 2'
    );

    CREATE TRIGGER segments_fts_insert AFTER INSERT ON segments BEGIN
        INSERT INTO segments_fts(rowid, text) VALUES (new.id, new.text);
    END;

    CREATE TRIGGER segments_fts_delete AFTER DELETE ON segments BEGIN
        INSERT INTO segments_fts(segments_fts, rowid, text)
            VALUES ('delete', old.id, old.text);
    END;

    CREATE TRIGGER segments_fts_update AFTER UPDATE ON segments BEGIN
        INSERT INTO segments_fts(segments_fts, rowid, text)
            VALUES ('delete', old.id, old.text);
        INSERT INTO segments_fts(rowid, text) VALUES (new.id, new.text);
    END;

    INSERT INTO segments_fts(rowid, text) SELECT id, text FROM segments;
    "#,
    // 3: small key/value store for preferences.
    //
    // Not the Keychain: these are not secrets, and a name sitting beside an API key
    // would blur what the Keychain is for.
    r#"
    CREATE TABLE preferences (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
    // 4: when transcription last finished for an event.
    //
    // Recovery needs to distinguish "never transcribed" from "transcribed and found
    // nothing to say". Counting segments cannot: a recording of an empty room has none
    // either way, and it would be re-transcribed on every launch forever.
    //
    // Existing events with segments are backfilled from `created_at`. The exact time is
    // not knowable after the fact and does not matter — only that the pass happened.
    r#"
    ALTER TABLE events ADD COLUMN transcribed_at TEXT;

    UPDATE events SET transcribed_at = created_at
     WHERE id IN (SELECT DISTINCT event_id FROM segments);
    "#,
    // 5: questions asked about a recording, and the answers.
    //
    // Persisted rather than held in the window, so coming back to a recording next week
    // shows what was already asked instead of an empty box. Deleted with the event, like
    // segments — a question about a recording is meaningless once the recording is gone.
    r#"
    CREATE TABLE chat_messages (
        id         INTEGER PRIMARY KEY,
        event_id   INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
        role       TEXT    NOT NULL CHECK (role IN ('user', 'assistant')),
        content    TEXT    NOT NULL,
        created_at TEXT    NOT NULL
    );

    CREATE INDEX idx_chat_messages_event ON chat_messages(event_id, id);
    "#,
    // 6: when the audio was deliberately deleted, keeping the transcript.
    //
    // A recording's WAV is by far the largest thing Hearsay writes — hundreds of
    // megabytes an hour — and once a transcript exists the audio is often no longer
    // wanted. Deleting it leaves the segments, the summary, the search index and the
    // questions all intact, because none of them are derived from the file.
    //
    // Its own column rather than an inference from `audio_path IS NULL`, for the same
    // reason `transcribed_at` exists: absence cannot say why. A recording whose file
    // never arrived and one whose audio was thrown away on purpose would otherwise look
    // identical, and the second needs to say so — the seek buttons in its transcript are
    // inert, and that has to read as a consequence rather than as a fault.
    r#"
    ALTER TABLE events ADD COLUMN audio_deleted_at TEXT;
    "#,
    // 7: stretches of a recording during which a channel was not being captured.
    //
    // A recording's mode can change while it runs, and the moment it does the transcript
    // stops being uniform: part of it has a microphone channel and part of it does not.
    // `events.mode` describes the file, which is one value, so it cannot say when.
    //
    // Not folded into `mute_spans`, for the reason `audio_deleted_at` is not folded into
    // `audio_path IS NULL`: the zeros look identical and the causes are not. A muted span
    // is a microphone that was open and deliberately silenced. `no_microphone` is a
    // microphone that was never open at all, which is a stronger statement — nothing the
    // room said could have reached disk. Reading the second as the first would have the
    // transcript claim the user chose to go quiet when in fact they were not being
    // recorded.
    //
    // `system_audio_gap` is the cost of opening a microphone partway through: the tap's
    // aggregate device has to be destroyed first, and for the sub-second that takes there
    // is no system audio. The file carries silence there so its timeline still matches
    // the clock, and this is what says the silence was a switch rather than a quiet room.
    r#"
    CREATE TABLE capture_spans (
        id        INTEGER PRIMARY KEY,
        event_id  INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
        kind      TEXT    NOT NULL CHECK (kind IN ('no_microphone', 'system_audio_gap')),
        start_ms  INTEGER NOT NULL,
        end_ms    INTEGER NOT NULL
    );

    CREATE INDEX idx_capture_spans_event ON capture_spans(event_id, start_ms);
    "#,
];

/// A recording session and everything known about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    /// The user-facing title. Defaults to a timestamp and can be edited by hand.
    pub title: String,
    /// A title proposed by the model. Kept apart from `title` so regenerating a summary
    /// can never overwrite something the user typed.
    pub ai_title: Option<String>,
    pub calendar_event_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub mode: String,
    pub audio_path: Option<String>,
    pub summary_md: Option<String>,
    pub model_used: Option<String>,
    pub created_at: DateTime<Utc>,
    /// When a transcription pass last completed. `None` means one has never finished —
    /// which is what recovery looks for, and is not the same as having no segments.
    pub transcribed_at: Option<DateTime<Utc>>,
    /// When the audio was deleted on purpose, the transcript being kept. `None` covers
    /// both "the audio is still there" and "there never was any" — `audio_path` tells
    /// those two apart, and this tells them apart from a deliberate deletion.
    pub audio_deleted_at: Option<DateTime<Utc>>,
}

impl Event {
    /// What to show in the list: the user's title, else the model's, else a fallback.
    pub fn display_title(&self) -> &str {
        if !self.title.trim().is_empty() {
            return &self.title;
        }
        match &self.ai_title {
            Some(title) if !title.trim().is_empty() => title,
            _ => "Untitled recording",
        }
    }

    pub fn duration_ms(&self) -> Option<i64> {
        self.ended_at
            .map(|ended| (ended - self.started_at).num_milliseconds().max(0))
    }

    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            title: row.get("title")?,
            ai_title: row.get("ai_title")?,
            calendar_event_id: row.get("calendar_event_id")?,
            started_at: row.get("started_at")?,
            ended_at: row.get("ended_at")?,
            mode: row.get("mode")?,
            audio_path: row.get("audio_path")?,
            summary_md: row.get("summary_md")?,
            model_used: row.get("model_used")?,
            created_at: row.get("created_at")?,
            transcribed_at: row.get("transcribed_at")?,
            audio_deleted_at: row.get("audio_deleted_at")?,
        })
    }

    /// Whether the audio was deleted on purpose while the transcript was kept.
    pub fn audio_was_deleted(&self) -> bool {
        self.audio_deleted_at.is_some()
    }

    /// Whether there is a file to play, seek, re-transcribe, or export.
    ///
    /// Only says a path is recorded, not that it is still on disk — a caller that needs
    /// the file itself has to stat it.
    pub fn has_audio(&self) -> bool {
        self.audio_path.is_some()
    }
}

/// One transcribed span. `channel` is `mic` or `system`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub id: i64,
    pub event_id: i64,
    pub channel: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

impl Segment {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            event_id: row.get("event_id")?,
            channel: row.get("channel")?,
            start_ms: row.get("start_ms")?,
            end_ms: row.get("end_ms")?,
            text: row.get("text")?,
        })
    }
}

/// A stretch during which the microphone was writing zeros.
///
/// Persisted so the transcript can say so out loud. A silent gap with no marker would be
/// indistinguishable from nobody talking.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MuteSpan {
    pub id: i64,
    pub event_id: i64,
    pub start_ms: i64,
    pub end_ms: i64,
}

impl MuteSpan {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            event_id: row.get("event_id")?,
            start_ms: row.get("start_ms")?,
            end_ms: row.get("end_ms")?,
        })
    }
}

/// What a [`CaptureSpan`] is a stretch of.
pub const NO_MICROPHONE: &str = "no_microphone";
/// See [`NO_MICROPHONE`].
pub const SYSTEM_AUDIO_GAP: &str = "system_audio_gap";

/// A stretch during which one of a recording's channels was not being captured.
///
/// Written only when the recording's mode changed while it ran. A recording that stayed
/// in one mode says which in `events.mode`, and marking the whole of a listen-only
/// transcript as having no microphone would tell a reader nothing they cannot already see.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureSpan {
    pub id: i64,
    pub event_id: i64,
    /// [`NO_MICROPHONE`] or [`SYSTEM_AUDIO_GAP`].
    pub kind: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

impl CaptureSpan {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            event_id: row.get("event_id")?,
            kind: row.get("kind")?,
            start_ms: row.get("start_ms")?,
            end_ms: row.get("end_ms")?,
        })
    }
}

/// One question asked about a recording, or one answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: i64,
    pub event_id: i64,
    /// `user` or `assistant`.
    pub role: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

impl ChatMessage {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            event_id: row.get("event_id")?,
            role: row.get("role")?,
            content: row.get("content")?,
            created_at: row.get("created_at")?,
        })
    }
}

/// A segment about to be written. Has no id yet.
#[derive(Debug, Clone)]
pub struct NewSegment {
    pub channel: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

/// The database. Cheap to clone a reference to; all access is serialised internally.
pub struct Database {
    connection: Mutex<Connection>,
}

impl Database {
    /// Opens (or creates) the database at `path` and brings the schema up to date.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("could not open database at {}", path.display()))?;
        Self::configure(&connection)?;
        migrate(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// An in-memory database, for tests.
    pub fn open_in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory()?;
        Self::configure(&connection)?;
        migrate(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn configure(connection: &Connection) -> Result<()> {
        // Cascading deletes only happen if foreign keys are actually enforced, and
        // SQLite leaves them off by default.
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )?;
        Ok(())
    }

    /// Runs `body` with the connection held. Keeps the mutex handling in one place.
    pub fn with_connection<T>(&self, body: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("the database lock was poisoned by an earlier panic"))?;
        body(&connection)
    }

    // -- events ------------------------------------------------------------------

    /// Records the start of a session and returns its id.
    pub fn create_event(
        &self,
        title: &str,
        mode: &str,
        started_at: DateTime<Utc>,
        audio_path: Option<&str>,
        calendar_event_id: Option<&str>,
    ) -> Result<i64> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO events
                     (title, mode, started_at, audio_path, calendar_event_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![title, mode, started_at, audio_path, calendar_event_id, Utc::now()],
            )?;
            Ok(connection.last_insert_rowid())
        })
    }

    /// Marks a session finished.
    pub fn finish_event(&self, event_id: i64, ended_at: DateTime<Utc>) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE events SET ended_at = ?1 WHERE id = ?2",
                params![ended_at, event_id],
            )?;
            Ok(())
        })
    }

    /// Records that a transcription pass finished, whatever it found.
    ///
    /// Set even when the pass produced no segments. A recording of a silent room has
    /// nothing to store, and without this it would look untranscribed forever.
    pub fn mark_transcribed(&self, event_id: i64, at: DateTime<Utc>) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE events SET transcribed_at = ?1 WHERE id = ?2",
                params![at, event_id],
            )?;
            Ok(())
        })
    }

    /// Events that were never marked finished, oldest first.
    ///
    /// Only one recording runs at a time and `ended_at` is written when it stops, so at
    /// startup every one of these is a session that was interrupted — by sleep, a crash,
    /// or a force-quit. Meaningful only before recording begins; during a session the
    /// live one is in here too.
    pub fn unfinished_events(&self) -> Result<Vec<Event>> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT * FROM events WHERE ended_at IS NULL ORDER BY started_at ASC",
            )?;
            let events = statement
                .query_map([], Event::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(events)
        })
    }

    /// Events with audio that no transcription pass has ever finished, oldest first.
    pub fn untranscribed_events(&self) -> Result<Vec<Event>> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT * FROM events
                  WHERE audio_path IS NOT NULL AND transcribed_at IS NULL
                  ORDER BY started_at ASC",
            )?;
            let events = statement
                .query_map([], Event::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(events)
        })
    }

    pub fn event(&self, event_id: i64) -> Result<Option<Event>> {
        self.with_connection(|connection| {
            let event = connection
                .query_row(
                    "SELECT * FROM events WHERE id = ?1",
                    params![event_id],
                    Event::from_row,
                )
                .optional()?;
            Ok(event)
        })
    }

    /// Every event, newest first.
    pub fn events(&self) -> Result<Vec<Event>> {
        self.with_connection(|connection| {
            let mut statement =
                connection.prepare("SELECT * FROM events ORDER BY started_at DESC")?;
            let events = statement
                .query_map([], Event::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(events)
        })
    }

    pub fn rename_event(&self, event_id: i64, title: &str) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE events SET title = ?1 WHERE id = ?2",
                params![title, event_id],
            )?;
            Ok(())
        })
    }

    /// Stores a generated summary and the title the model suggested.
    ///
    /// `title` is written to `ai_title`, never to `title`, so regenerating cannot
    /// clobber a name the user chose.
    pub fn set_summary(
        &self,
        event_id: i64,
        summary_md: &str,
        ai_title: Option<&str>,
        model_used: &str,
    ) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE events
                    SET summary_md = ?1, ai_title = COALESCE(?2, ai_title), model_used = ?3
                  WHERE id = ?4",
                params![summary_md, ai_title, model_used, event_id],
            )?;
            Ok(())
        })
    }

    pub fn set_audio_path(&self, event_id: i64, audio_path: &str) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE events SET audio_path = ?1 WHERE id = ?2",
                params![audio_path, event_id],
            )?;
            Ok(())
        })
    }

    /// Records that a recording's audio was deleted while its transcript was kept.
    ///
    /// Clears `audio_path` in the same statement. Leaving the path behind would point
    /// every reader at a file that is not there: the webview would build an `<audio>`
    /// source that never loads, and [`Database::untranscribed_events`] — which keys on
    /// `audio_path IS NOT NULL` — would offer the recording up for transcription on
    /// every launch from now on.
    ///
    /// Deleting the file itself is the caller's job, as with [`Database::delete_event`].
    /// The database does not remove a user's recording behind their back.
    pub fn mark_audio_deleted(&self, event_id: i64) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE events SET audio_path = NULL, audio_deleted_at = ?1 WHERE id = ?2",
                params![Utc::now(), event_id],
            )?;
            Ok(())
        })
    }

    pub fn link_calendar_event(&self, event_id: i64, calendar_event_id: &str) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE events SET calendar_event_id = ?1 WHERE id = ?2",
                params![calendar_event_id, event_id],
            )?;
            Ok(())
        })
    }

    /// Deletes an event and everything hanging off it. The audio file is the caller's
    /// responsibility — the database will not silently remove a user's recording.
    pub fn delete_event(&self, event_id: i64) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute("DELETE FROM events WHERE id = ?1", params![event_id])?;
            Ok(())
        })
    }

    // -- segments ----------------------------------------------------------------

    /// Replaces an event's segments in one transaction.
    ///
    /// Re-transcribing must not be able to leave a half-old, half-new transcript behind,
    /// so the delete and the insert either both happen or neither does.
    pub fn replace_segments(&self, event_id: i64, segments: &[NewSegment]) -> Result<()> {
        self.with_connection(|connection| {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute("DELETE FROM segments WHERE event_id = ?1", params![event_id])?;
            {
                let mut statement = transaction.prepare(
                    "INSERT INTO segments (event_id, channel, start_ms, end_ms, text)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                for segment in segments {
                    statement.execute(params![
                        event_id,
                        segment.channel,
                        segment.start_ms,
                        segment.end_ms,
                        segment.text
                    ])?;
                }
            }
            transaction.commit()?;
            Ok(())
        })
    }

    /// An event's segments in playback order.
    pub fn segments(&self, event_id: i64) -> Result<Vec<Segment>> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT * FROM segments WHERE event_id = ?1 ORDER BY start_ms, end_ms",
            )?;
            let segments = statement
                .query_map(params![event_id], Segment::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(segments)
        })
    }

    // -- mute spans --------------------------------------------------------------

    // -- questions about a recording -----------------------------------------------

    /// Every question and answer for an event, oldest first.
    pub fn chat_messages(&self, event_id: i64) -> Result<Vec<ChatMessage>> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT * FROM chat_messages WHERE event_id = ?1 ORDER BY id ASC",
            )?;
            let messages = statement
                .query_map(params![event_id], ChatMessage::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(messages)
        })
    }

    /// Appends one message and returns its id.
    pub fn add_chat_message(&self, event_id: i64, role: &str, content: &str) -> Result<i64> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO chat_messages (event_id, role, content, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![event_id, role, content, Utc::now()],
            )?;
            Ok(connection.last_insert_rowid())
        })
    }

    /// Forgets the whole conversation about an event. The transcript is untouched.
    pub fn clear_chat(&self, event_id: i64) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "DELETE FROM chat_messages WHERE event_id = ?1",
                params![event_id],
            )?;
            Ok(())
        })
    }

    /// Removes a single message. Used to take back a question whose answer never arrived,
    /// so a failed attempt does not sit in the history forever.
    pub fn delete_chat_message(&self, message_id: i64) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "DELETE FROM chat_messages WHERE id = ?1",
                params![message_id],
            )?;
            Ok(())
        })
    }

    pub fn replace_mute_spans(&self, event_id: i64, spans: &[(i64, i64)]) -> Result<()> {
        self.with_connection(|connection| {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute(
                "DELETE FROM mute_spans WHERE event_id = ?1",
                params![event_id],
            )?;
            {
                let mut statement = transaction.prepare(
                    "INSERT INTO mute_spans (event_id, start_ms, end_ms) VALUES (?1, ?2, ?3)",
                )?;
                for (start_ms, end_ms) in spans {
                    statement.execute(params![event_id, start_ms, end_ms])?;
                }
            }
            transaction.commit()?;
            Ok(())
        })
    }

    /// Replaces the stretches during which a channel was not being captured.
    ///
    /// Written in one transaction with the mute spans' shape, so a recording whose mode
    /// changed twice cannot end up with half its markers.
    pub fn replace_capture_spans(
        &self,
        event_id: i64,
        spans: &[(&str, i64, i64)],
    ) -> Result<()> {
        self.with_connection(|connection| {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute(
                "DELETE FROM capture_spans WHERE event_id = ?1",
                params![event_id],
            )?;
            {
                let mut statement = transaction.prepare(
                    "INSERT INTO capture_spans (event_id, kind, start_ms, end_ms)
                     VALUES (?1, ?2, ?3, ?4)",
                )?;
                for (kind, start_ms, end_ms) in spans {
                    statement.execute(params![event_id, kind, start_ms, end_ms])?;
                }
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn capture_spans(&self, event_id: i64) -> Result<Vec<CaptureSpan>> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare("SELECT * FROM capture_spans WHERE event_id = ?1 ORDER BY start_ms")?;
            let spans = statement
                .query_map(params![event_id], CaptureSpan::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(spans)
        })
    }

    /// Records what the recording's file turned out to be, which a mode switch changes.
    ///
    /// Set from the finished file rather than from what was asked for at the start. It
    /// decides how many channels transcription reads, so a stereo file recorded as
    /// `listen_only` would be read as one mono channel — the silent left one, and none of
    /// the meeting.
    pub fn set_mode(&self, event_id: i64, mode: &str) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE events SET mode = ?1 WHERE id = ?2",
                params![mode, event_id],
            )?;
            Ok(())
        })
    }

    // -- preferences -------------------------------------------------------------

    /// Reads a preference. `None` when it has never been set.
    pub fn preference(&self, key: &str) -> Result<Option<String>> {
        self.with_connection(|connection| {
            let value = connection
                .query_row(
                    "SELECT value FROM preferences WHERE key = ?1",
                    params![key],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            Ok(value)
        })
    }

    /// Sets a preference, or clears it when given an empty string.
    pub fn set_preference(&self, key: &str, value: &str) -> Result<()> {
        self.with_connection(|connection| {
            if value.trim().is_empty() {
                connection.execute("DELETE FROM preferences WHERE key = ?1", params![key])?;
            } else {
                connection.execute(
                    "INSERT INTO preferences(key, value) VALUES(?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![key, value.trim()],
                )?;
            }
            Ok(())
        })
    }

    // -- search ------------------------------------------------------------------

    /// Full-text search across every transcript.
    ///
    /// Results carry enough context to render a row and jump straight to the moment:
    /// the event, the timestamp, and a snippet with the matched terms marked.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let Some(match_expression) = to_fts_query(query) else {
            return Ok(Vec::new());
        };

        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT s.id            AS segment_id,
                        s.event_id      AS event_id,
                        s.channel       AS channel,
                        s.start_ms      AS start_ms,
                        s.end_ms        AS end_ms,
                        s.text          AS text,
                        e.title         AS event_title,
                        e.ai_title      AS event_ai_title,
                        e.started_at    AS started_at,
                        snippet(segments_fts, 0, '[', ']', '…', 12) AS snippet
                   FROM segments_fts
                   JOIN segments s ON s.id = segments_fts.rowid
                   JOIN events   e ON e.id = s.event_id
                  WHERE segments_fts MATCH ?1
                  ORDER BY rank
                  LIMIT ?2",
            )?;

            let hits = statement
                .query_map(params![match_expression, limit as i64], |row| {
                    let title: String = row.get("event_title")?;
                    let ai_title: Option<String> = row.get("event_ai_title")?;
                    Ok(SearchHit {
                        segment_id: row.get("segment_id")?,
                        event_id: row.get("event_id")?,
                        event_title: if title.trim().is_empty() {
                            ai_title.unwrap_or_else(|| "Untitled recording".to_string())
                        } else {
                            title
                        },
                        started_at: row.get("started_at")?,
                        channel: row.get("channel")?,
                        start_ms: row.get("start_ms")?,
                        end_ms: row.get("end_ms")?,
                        text: row.get("text")?,
                        snippet: row.get("snippet")?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(hits)
        })
    }

    /// Rebuilds the search index from the segments table.
    ///
    /// Nothing in normal operation needs this — the triggers keep the index current. It
    /// exists so a suspect index can be fixed without re-transcribing anything.
    pub fn rebuild_search_index(&self) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO segments_fts(segments_fts) VALUES ('rebuild')",
                [],
            )?;
            Ok(())
        })
    }

    pub fn mute_spans(&self, event_id: i64) -> Result<Vec<MuteSpan>> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare("SELECT * FROM mute_spans WHERE event_id = ?1 ORDER BY start_ms")?;
            let spans = statement
                .query_map(params![event_id], MuteSpan::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(spans)
        })
    }
}

/// One search result, with enough context to render it and seek to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub segment_id: i64,
    pub event_id: i64,
    pub event_title: String,
    pub started_at: DateTime<Utc>,
    pub channel: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    /// The matched text with `[` and `]` around the hit terms.
    pub snippet: String,
}

/// Turns what the user typed into an FTS5 MATCH expression.
///
/// FTS5's query language has its own syntax, and a transcript search box is exactly
/// where someone types an apostrophe, a colon, or a stray quote. Rather than let that
/// become a SQL error in the user's face, every word is extracted and quoted as a
/// literal term. The last word gets a prefix marker so results narrow as you type.
///
/// Returns `None` when there is nothing searchable, so callers can show an empty result
/// instead of running a query that matches everything.
fn to_fts_query(input: &str) -> Option<String> {
    let words: Vec<String> = input
        .split(|character: char| !character.is_alphanumeric() && character != '\'')
        .filter(|word| !word.is_empty())
        .map(|word| word.replace('"', ""))
        .filter(|word| !word.is_empty())
        .collect();

    if words.is_empty() {
        return None;
    }

    let last = words.len() - 1;
    let terms: Vec<String> = words
        .iter()
        .enumerate()
        .map(|(index, word)| {
            if index == last {
                format!("\"{word}\"*")
            } else {
                format!("\"{word}\"")
            }
        })
        .collect();

    Some(terms.join(" "))
}

/// Applies any migrations the database has not seen yet.
fn migrate(connection: &Connection) -> Result<()> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let current = version.max(0) as usize;

    if current > MIGRATIONS.len() {
        anyhow::bail!(
            "this database is at schema version {current}, but this build only knows about \
             {}. It was written by a newer version of Hearsay.",
            MIGRATIONS.len()
        );
    }

    for (index, migration) in MIGRATIONS.iter().enumerate().skip(current) {
        let next = index + 1;
        tracing::info!("applying schema migration {next}");
        connection
            .execute_batch(&format!("BEGIN; {migration} PRAGMA user_version = {next}; COMMIT;"))
            .with_context(|| format!("migration {next} failed"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> (Database, i64) {
        let db = Database::open_in_memory().expect("in-memory database opens");
        let id = db
            .create_event("Design review", "conversation", Utc::now(), None, None)
            .expect("event is created");
        (db, id)
    }

    #[test]
    fn migrations_are_idempotent() {
        let db = Database::open_in_memory().expect("database opens");
        db.with_connection(|connection| {
            migrate(connection)?;
            migrate(connection)?;
            let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            assert_eq!(version as usize, MIGRATIONS.len());
            Ok(())
        })
        .expect("re-running migrations is safe");
    }

    #[test]
    fn an_event_round_trips() {
        let (db, id) = seeded();
        let event = db.event(id).expect("query works").expect("event exists");
        assert_eq!(event.title, "Design review");
        assert_eq!(event.mode, "conversation");
        assert!(event.ended_at.is_none());

        let ended = Utc::now();
        db.finish_event(id, ended).expect("finish works");
        let event = db.event(id).expect("query works").expect("event exists");
        assert!(event.ended_at.is_some());
        assert!(event.duration_ms().unwrap_or(-1) >= 0);
    }

    // ---- recovering interrupted recordings ----

    #[test]
    fn an_unfinished_event_is_listed_until_it_is_finished() {
        let (db, id) = seeded();
        let unfinished = db.unfinished_events().expect("query works");
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].id, id);

        db.finish_event(id, Utc::now()).expect("finish works");
        assert!(db.unfinished_events().expect("query works").is_empty());
    }

    #[test]
    fn an_event_with_audio_is_untranscribed_until_a_pass_is_recorded() {
        let (db, id) = seeded();
        // No audio path yet, so there is nothing to transcribe.
        assert!(db.untranscribed_events().expect("query works").is_empty());

        db.set_audio_path(id, "/tmp/whatever.wav").expect("path is set");
        let pending = db.untranscribed_events().expect("query works");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id);

        db.mark_transcribed(id, Utc::now()).expect("mark works");
        assert!(db.untranscribed_events().expect("query works").is_empty());
    }

    /// The reason `transcribed_at` exists at all. A recording of a silent room produces no
    /// segments, and if "no segments" meant "never transcribed" it would be re-transcribed
    /// on every single launch, forever.
    #[test]
    fn a_transcribed_recording_with_no_speech_is_not_pending_again() {
        let (db, id) = seeded();
        db.set_audio_path(id, "/tmp/silence.wav").expect("path is set");
        db.replace_segments(id, &[]).expect("an empty pass stores nothing");
        db.mark_transcribed(id, Utc::now()).expect("mark works");

        assert!(db.segments(id).expect("segments load").is_empty());
        assert!(
            db.untranscribed_events().expect("query works").is_empty(),
            "a recording with nothing to say must not queue for transcription forever"
        );
    }

    // ---- deleting the audio while keeping the transcript ----

    #[test]
    fn deleting_the_audio_keeps_the_transcript_and_the_summary() {
        let (db, id) = seeded();
        db.set_audio_path(id, "/tmp/big.wav").expect("path is set");
        db.replace_segments(
            id,
            &[NewSegment {
                channel: "system".into(),
                start_ms: 0,
                end_ms: 1_000,
                text: "the deadline is the fourth".into(),
            }],
        )
        .expect("segments store");
        db.mark_transcribed(id, Utc::now()).expect("mark works");
        db.set_summary(id, "## Worth remembering", None, "test-model")
            .expect("summary stores");

        db.mark_audio_deleted(id).expect("the audio is marked gone");

        let event = db.event(id).expect("query works").expect("event exists");
        assert!(event.audio_was_deleted());
        assert!(!event.has_audio(), "the dangling path must be cleared");
        assert_eq!(
            db.segments(id).expect("segments load").len(),
            1,
            "segments are the source of truth and outlive the audio"
        );
        assert!(event.summary_md.is_some());
        assert_eq!(
            db.search("deadline", 10).expect("search works").len(),
            1,
            "the search index reads through to segments, not to the file"
        );
    }

    /// The reason `mark_audio_deleted` clears `audio_path` rather than only stamping the
    /// date. Left in place, the path would offer the recording for transcription on every
    /// launch — against a file that is deliberately not there.
    #[test]
    fn a_recording_whose_audio_was_deleted_never_queues_for_transcription() {
        let (db, id) = seeded();
        db.set_audio_path(id, "/tmp/big.wav").expect("path is set");
        assert_eq!(db.untranscribed_events().expect("query works").len(), 1);

        db.mark_audio_deleted(id).expect("the audio is marked gone");
        assert!(
            db.untranscribed_events().expect("query works").is_empty(),
            "deleted audio must not be queued for a pass that can only fail"
        );
    }

    /// Deletion is not the same as never having had a file, and the difference has to
    /// survive a round trip — it is what the Audio tab reads to explain itself.
    #[test]
    fn a_recording_that_never_had_audio_is_not_reported_as_deleted() {
        let (db, id) = seeded();
        let event = db.event(id).expect("query works").expect("event exists");
        assert!(!event.has_audio());
        assert!(
            !event.audio_was_deleted(),
            "no audio is not the same as audio thrown away"
        );
    }

    /// Recordings made before `transcribed_at` existed already have transcripts, and must
    /// not all be re-transcribed the first time the new build starts.
    #[test]
    fn the_migration_backfills_events_that_already_have_segments() {
        // A real database left at the schema version before `transcribed_at` existed.
        let connection = Connection::open_in_memory().expect("connection opens");
        Database::configure(&connection).expect("pragmas apply");
        for (index, migration) in MIGRATIONS.iter().enumerate().take(3) {
            let next = index + 1;
            connection
                .execute_batch(&format!(
                    "BEGIN; {migration} PRAGMA user_version = {next}; COMMIT;"
                ))
                .expect("an earlier migration applies");
        }

        let now = Utc::now();
        for (title, path) in [("Old, transcribed", "/tmp/a.wav"), ("Never transcribed", "/tmp/b.wav")] {
            connection
                .execute(
                    "INSERT INTO events (title, mode, started_at, audio_path, created_at)
                     VALUES (?1, 'listen_only', ?2, ?3, ?2)",
                    params![title, now, path],
                )
                .expect("seed event inserts");
        }
        let transcribed = 1i64;
        let never = 2i64;
        connection
            .execute(
                "INSERT INTO segments (event_id, channel, start_ms, end_ms, text)
                 VALUES (?1, 'system', 0, 10, 'something was said')",
                params![transcribed],
            )
            .expect("seed segment inserts");

        migrate(&connection).expect("the new migration applies");

        let db = Database {
            connection: Mutex::new(connection),
        };
        let pending: Vec<i64> = db
            .untranscribed_events()
            .expect("query works")
            .into_iter()
            .map(|event| event.id)
            .collect();
        assert_eq!(
            pending,
            vec![never],
            "only the recording that never had a transcript should be pending"
        );
        assert!(db
            .event(transcribed)
            .expect("query works")
            .expect("event exists")
            .transcribed_at
            .is_some());
    }

    // ---- questions about a recording ----

    #[test]
    fn a_conversation_round_trips_in_the_order_it_happened() {
        let (db, id) = seeded();
        db.add_chat_message(id, "user", "what did they say about pay?")
            .expect("question stores");
        db.add_chat_message(id, "assistant", "Nothing — it never came up.")
            .expect("answer stores");
        db.add_chat_message(id, "user", "who else was there?")
            .expect("second question stores");

        let messages = db.chat_messages(id).expect("history loads");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "what did they say about pay?");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[2].content, "who else was there?");
    }

    #[test]
    fn clearing_a_conversation_leaves_the_transcript_alone() {
        let (db, id) = seeded();
        db.replace_segments(
            id,
            &[NewSegment {
                channel: "system".into(),
                start_ms: 0,
                end_ms: 10,
                text: "the transcript".into(),
            }],
        )
        .expect("segments insert");
        db.add_chat_message(id, "user", "anything?").expect("question stores");

        db.clear_chat(id).expect("clear works");

        assert!(db.chat_messages(id).expect("history loads").is_empty());
        assert_eq!(
            db.segments(id).expect("segments load").len(),
            1,
            "clearing questions must not touch the transcript"
        );
    }

    /// A failed question is withdrawn so it is never replayed as history to the model.
    #[test]
    fn a_single_message_can_be_withdrawn() {
        let (db, id) = seeded();
        let first = db
            .add_chat_message(id, "user", "kept")
            .expect("question stores");
        let second = db
            .add_chat_message(id, "user", "withdrawn")
            .expect("question stores");

        db.delete_chat_message(second).expect("withdraw works");

        let remaining = db.chat_messages(id).expect("history loads");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, first);
    }

    /// A question about a recording is meaningless once the recording is gone.
    #[test]
    fn deleting_a_recording_deletes_what_was_asked_about_it() {
        let (db, id) = seeded();
        db.add_chat_message(id, "user", "anything?").expect("question stores");

        db.delete_event(id).expect("delete works");

        assert!(db.chat_messages(id).expect("history loads").is_empty());
    }

    #[test]
    fn segments_are_stored_as_rows_in_playback_order() {
        let (db, id) = seeded();
        db.replace_segments(
            id,
            &[
                NewSegment {
                    channel: "system".into(),
                    start_ms: 2000,
                    end_ms: 3000,
                    text: "second".into(),
                },
                NewSegment {
                    channel: "mic".into(),
                    start_ms: 0,
                    end_ms: 1000,
                    text: "first".into(),
                },
            ],
        )
        .expect("segments insert");

        let segments = db.segments(id).expect("segments load");
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].text, "first");
        assert_eq!(segments[0].channel, "mic");
        assert_eq!(segments[1].text, "second");
    }

    #[test]
    fn re_transcribing_replaces_rather_than_appends() {
        let (db, id) = seeded();
        let one = [NewSegment {
            channel: "system".into(),
            start_ms: 0,
            end_ms: 500,
            text: "old".into(),
        }];
        db.replace_segments(id, &one).expect("first insert");
        let two = [NewSegment {
            channel: "system".into(),
            start_ms: 0,
            end_ms: 500,
            text: "new".into(),
        }];
        db.replace_segments(id, &two).expect("second insert");

        let segments = db.segments(id).expect("segments load");
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "new");
    }

    #[test]
    fn an_invalid_channel_is_rejected_by_the_schema() {
        let (db, id) = seeded();
        let result = db.replace_segments(
            id,
            &[NewSegment {
                channel: "speaker".into(),
                start_ms: 0,
                end_ms: 1,
                text: "nope".into(),
            }],
        );
        assert!(result.is_err(), "channel check constraint should reject this");
    }

    #[test]
    fn deleting_an_event_takes_its_segments_and_mute_spans_with_it() {
        let (db, id) = seeded();
        db.replace_segments(
            id,
            &[NewSegment {
                channel: "mic".into(),
                start_ms: 0,
                end_ms: 100,
                text: "hello".into(),
            }],
        )
        .expect("segments insert");
        db.replace_mute_spans(id, &[(100, 200)])
            .expect("mute spans insert");

        db.delete_event(id).expect("delete works");
        assert!(db.segments(id).expect("query works").is_empty());
        assert!(db.mute_spans(id).expect("query works").is_empty());
    }

    #[test]
    fn a_generated_title_never_overwrites_one_the_user_typed() {
        let (db, id) = seeded();
        db.set_summary(id, "## Summary", Some("Q3 planning sync"), "claude-opus-5")
            .expect("summary saves");

        let event = db.event(id).expect("query works").expect("event exists");
        assert_eq!(event.title, "Design review");
        assert_eq!(event.ai_title.as_deref(), Some("Q3 planning sync"));
        assert_eq!(event.display_title(), "Design review");
    }

    fn with_transcript() -> (Database, i64) {
        let (db, id) = seeded();
        db.replace_segments(
            id,
            &[
                NewSegment {
                    channel: "system".into(),
                    start_ms: 0,
                    end_ms: 4_000,
                    text: "We need someone to own the migration timeline".into(),
                },
                NewSegment {
                    channel: "mic".into(),
                    start_ms: 4_000,
                    end_ms: 7_000,
                    text: "I'll take the migration and follow up on Friday".into(),
                },
                NewSegment {
                    channel: "system".into(),
                    start_ms: 7_000,
                    end_ms: 9_000,
                    text: "Great, let's move on to hiring".into(),
                },
            ],
        )
        .expect("segments insert");
        (db, id)
    }

    #[test]
    fn search_finds_segments_by_word() {
        let (db, id) = with_transcript();
        let hits = db.search("migration", 20).expect("search runs");
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|hit| hit.event_id == id));
        assert!(hits.iter().any(|hit| hit.channel == "mic"));
        assert!(hits.iter().any(|hit| hit.channel == "system"));
    }

    #[test]
    fn search_results_carry_a_seek_point_and_a_snippet() {
        let (db, _) = with_transcript();
        let hits = db.search("hiring", 20).expect("search runs");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].start_ms, 7_000);
        assert!(
            hits[0].snippet.contains('['),
            "expected the match to be marked: {:?}",
            hits[0].snippet
        );
    }

    #[test]
    fn search_narrows_as_you_type() {
        let (db, _) = with_transcript();
        assert!(!db.search("migr", 20).expect("search runs").is_empty());
    }

    /// The search box is where stray punctuation lands. None of it may reach FTS5 as
    /// syntax and come back as an error.
    #[test]
    fn punctuation_in_a_query_is_never_a_syntax_error() {
        let (db, _) = with_transcript();
        for query in [
            "\"unbalanced",
            "migration:",
            "NEAR(",
            "a AND OR b",
            "friday's",
            "*",
            "^",
        ] {
            let result = db.search(query, 20);
            assert!(result.is_ok(), "query {query:?} failed: {result:?}");
        }
    }

    #[test]
    fn an_empty_query_matches_nothing_rather_than_everything() {
        let (db, _) = with_transcript();
        assert!(db.search("", 20).expect("search runs").is_empty());
        assert!(db.search("   ", 20).expect("search runs").is_empty());
        assert!(db.search("!!!", 20).expect("search runs").is_empty());
    }

    #[test]
    fn deleted_segments_leave_the_search_index() {
        let (db, id) = with_transcript();
        assert!(!db.search("hiring", 20).expect("search runs").is_empty());

        db.replace_segments(id, &[]).expect("segments cleared");
        assert!(
            db.search("hiring", 20).expect("search runs").is_empty(),
            "the index still returns text that is no longer stored"
        );
    }

    #[test]
    fn deleting_an_event_removes_its_text_from_search() {
        let (db, id) = with_transcript();
        db.delete_event(id).expect("delete works");
        assert!(db.search("migration", 20).expect("search runs").is_empty());
    }

    #[test]
    fn rebuilding_the_index_preserves_results() {
        let (db, _) = with_transcript();
        db.rebuild_search_index().expect("rebuild works");
        assert_eq!(db.search("migration", 20).expect("search runs").len(), 2);
    }

    #[test]
    fn preferences_round_trip_and_clear() {
        let (db, _) = seeded();
        assert_eq!(db.preference("speaker_name").expect("read"), None);

        db.set_preference("speaker_name", "  Grace  ").expect("write");
        assert_eq!(
            db.preference("speaker_name").expect("read").as_deref(),
            Some("Grace"),
            "the stored value should be trimmed"
        );

        db.set_preference("speaker_name", "").expect("clear");
        assert_eq!(db.preference("speaker_name").expect("read"), None);
    }

    #[test]
    fn capture_spans_round_trip() {
        let (db, id) = seeded();
        db.replace_capture_spans(
            id,
            &[
                (NO_MICROPHONE, 0, 720_000),
                (SYSTEM_AUDIO_GAP, 720_000, 720_600),
            ],
        )
        .expect("capture spans insert");

        let spans = db.capture_spans(id).expect("spans load");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].kind, NO_MICROPHONE);
        assert_eq!(spans[0].end_ms, 720_000);
        assert_eq!(spans[1].kind, SYSTEM_AUDIO_GAP);
    }

    /// Two switches in one recording produce two `no_microphone` stretches, and both have
    /// to survive: a transcript that marks the first and drops the second reads as though
    /// the microphone stayed open to the end.
    #[test]
    fn every_stretch_without_a_microphone_survives() {
        let (db, id) = seeded();
        db.replace_capture_spans(
            id,
            &[(NO_MICROPHONE, 0, 60_000), (NO_MICROPHONE, 300_000, 900_000)],
        )
        .expect("capture spans insert");

        let spans = db.capture_spans(id).expect("spans load");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].start_ms, 0);
        assert_eq!(spans[1].start_ms, 300_000);
    }

    #[test]
    fn a_kind_the_schema_does_not_know_is_refused() {
        let (db, id) = seeded();
        assert!(
            db.replace_capture_spans(id, &[("dozed_off", 0, 10)]).is_err(),
            "the schema must not accept a span nothing can render"
        );
    }

    #[test]
    fn deleting_an_event_takes_its_capture_spans_with_it() {
        let (db, id) = seeded();
        db.replace_capture_spans(id, &[(NO_MICROPHONE, 0, 500)])
            .expect("capture spans insert");

        db.delete_event(id).expect("delete works");
        assert!(db.capture_spans(id).expect("query works").is_empty());
    }

    /// The mode is written from the finished file, because a recording that gained a
    /// microphone partway through is stereo whatever it was started as.
    #[test]
    fn the_mode_can_be_corrected_after_a_switch() {
        let db = Database::open_in_memory().expect("in-memory database opens");
        let id = db
            .create_event("Info session", "listen_only", Utc::now(), None, None)
            .expect("event is created");
        assert_eq!(db.event(id).expect("read").expect("exists").mode, "listen_only");

        db.set_mode(id, "conversation").expect("mode updates");
        assert_eq!(
            db.event(id).expect("read").expect("exists").mode,
            "conversation"
        );
    }

    #[test]
    fn mute_spans_round_trip() {
        let (db, id) = seeded();
        db.replace_mute_spans(id, &[(1_000, 4_500), (9_000, 12_000)])
            .expect("spans insert");
        let spans = db.mute_spans(id).expect("spans load");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].start_ms, 1_000);
        assert_eq!(spans[1].end_ms, 12_000);
    }
}
