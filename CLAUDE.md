# Hearsay

Local-first macOS app that records meetings, transcribes them on-device, and stores them as
searchable events. Single user, single machine, no server, no accounts.

---

## 1. Non-negotiables

These are not preferences. Violating any of them is a bug, even if the app still runs.

- **Nothing leaves the machine** except two named, opt-in exceptions, both using the
  user's own credentials:
  1. **LLM calls about one recording's transcript** (Anthropic or Gemini), made only
     when the user asks for a summary (§8a) or sends a question about that recording
     (§8b). One channel, two triggers, both requiring an explicit press. Never on a
     timer, never in the background, never for a recording the user is not looking at.
  2. **Google Calendar reads** (§11), when the user connects a calendar — read-only,
     titles and times only, and nothing is ever uploaded.

  There is no third. Adding one is a spec change, not an implementation detail.

  Audio is never uploaded by either. What goes out is transcript text, which was
  produced on this machine.
- **No analytics, no telemetry, no crash reporting, no update checks.** Do not add a dependency that
  phones home. If a crate or npm package does background network I/O, it does not belong here.

  Hearsay *does* notice when it is older than the checkout, and says so. That is not an
  update check: the binary carries the commit it was built from and compares it against
  the local repository. No server is contacted and nothing is fetched. Any version of
  this that reaches the network is out.
- **One user.** No auth, no roles, no sharing, no multi-tenancy, no "workspace" concept.
- **The app works fully without an API key.** Recording, transcription, search, and playback must
  never be gated on the API. Missing key degrades to transcripts only — it never blocks.
- **`listen_only` never opens the microphone.** Not opened-and-muted. Not opened-and-discarded.
  Never opened. See §4.
- **No `unwrap()` outside tests.** `anyhow::Result` at boundaries, `thiserror` in the audio layer.

---

## 2. Stack

| Layer | Choice |
|---|---|
| Shell | Tauri v2 |
| Core | Rust |
| Frontend | React + TypeScript + Vite |
| Storage | SQLite via `rusqlite` (bundled feature) |
| Transcription | `faster-whisper` as a **Python sidecar** invoked from Rust |
| System audio | **Swift helper binary** using Core Audio process taps |
| Secrets | macOS Keychain via the `keyring` crate |
| Target | macOS 26.5.2, Apple Silicon |

Transcription is a Python sidecar, **not** Rust-native whisper bindings. That is a deliberate
choice; do not "simplify" it into `whisper-rs`.

### Layout

```
hearsay/
├── CLAUDE.md                 this file
├── Cargo.toml                workspace root
├── rust-toolchain.toml       pinned toolchain
├── crates/
│   ├── hearsay-audio/        AudioSource trait, WAV writer, ring buffer, mute, echo detection
│   └── hearsay-core/         db, transcription driver, summaries, secrets, calendar
├── src-tauri/                thin Tauri app: commands + state, no business logic
├── src/                      React frontend
├── helper/                   Swift audio helper (dumb pipe, see §3)
└── python/                   venv setup + faster-whisper sidecar
```

Everything the app writes at runtime lives in `~/Library/Application Support/hearsay/`.

**The asset protocol scope in `tauri.conf.json` must name that path literally**, as
`$HOME/Library/Application Support/hearsay/recordings/*`. It reads as though `$APPDATA`
belongs there, and it does not: Tauri expands `$APPDATA` to
`~/Library/Application Support/<identifier>`, so `$APPDATA/hearsay/recordings/*` points at
`.../com.hearsay.app/hearsay/recordings/*`, a directory nothing ever creates. Every
recording is then blocked from the webview, the `<audio>` element never loads, and the
Audio tab, click-to-seek, and the timestamps in an answer all silently do nothing — with
one `asset protocol not configured to allow the path` line in the log as the only sign.

---

## 3. System audio capture

Core Audio **process taps** (`AudioHardwareCreateProcessTap`). Available on macOS 14.4+; the target
is 26.5.2, so this is supported.

**Do not use** BlackHole, Loopback, Soundflower, or any virtual audio device. **Do not** ask the
user to change their sound output. **Do not** build a Multi-Output Device.

The user's audio routing stays untouched: volume keys keep working, AirPods connect and disconnect
freely, there is nothing to remember before a meeting. If some requirement appears to make that
impossible, **stop and say so** — do not fall back to a loopback device.

### The helper contract

`helper/` builds a Swift binary that:

1. Creates a `CATapDescription`.
2. Wraps it in an aggregate device.
3. Writes **raw interleaved float32 PCM to stdout**.
4. Writes the negotiated format (sample rate, channel count) to **stderr** as one line of JSON.

Rust spawns it and reads the pipe.

**Keep the helper dumb.** No buffering, no file writing, no policy, no reconnection logic, no
resampling. All of that lives in Rust, behind `trait AudioSource`.

**Prefer process-scoped taps over system-wide**, so a single meeting app can be captured while music
playing alongside stays out of the transcript.

### Failure mode to design against

The characteristic failure of an audio tap is **silence**: it runs, reports success, and captures
nothing. The helper must **never silently emit zeros**. If it cannot obtain a real tap — TCC
permission denied, no such process, aggregate device creation failed — it exits non-zero with a
diagnostic on stderr. A run that produces only zero samples is a failure to be reported, not a
quiet success.

On `SIGTERM` the helper stops the device, destroys the tap and aggregate device, flushes stdout,
and exits 0.

---

## 4. Recording modes

Two modes. **`listen_only` is the default on every launch. Last-used mode is not persisted.**

### `listen_only` (default)

Opens the system audio tap **only**. The microphone is never opened — not opened-and-muted, never
instantiated at all.

This is relied upon: in this mode it must be *physically impossible* for room audio to reach disk.
The user attends info sessions where they never speak and may be talking to someone next to them.
Any code path that could construct a mic input in this mode is a defect.

### `conversation`

Opens both and writes **one stereo WAV**:

- **left channel = mic**
- **right channel = system audio**

**Never mix to mono.** The channel split gives speaker attribution for free (left is the user, right
is everyone else) and guarantees sample alignment with no drift.

### Switching mode mid-recording

Both directions, on a deliberate press, from the same toggle. The info session that turns
into a conversation is a real thing that happens, and the alternative was stopping and
starting a second recording of the same meeting.

Nothing about §4's guarantee is weakened. **The microphone still only ever opens because
somebody pressed Conversation** — never on a timer, never on a calendar event, never
because a previous session was in that mode. A switch *down* closes the input device
outright rather than muting it, which is a stronger promise than §5 can make: mute keeps
the device open and writes zeros, this releases it.

**Going up costs a sub-second gap in the system channel, and there is no version of it
that does not.** §3's helper wraps its tap in an aggregate device, and while one exists,
opening an input device takes over four minutes against under 200 ms before it — which is
why `Recording::start` opens the microphone first. There is no way to add one later without
taking the aggregate device down. So the tap is stopped, the microphone opened, and the tap
started again.

- **The gap is padded into the file as true silence and written down as a span.** Unpadded,
  everything captured after the switch would land where the gap belongs and every later
  timestamp would sit early by its length.
- **The file is restrided, not replaced.** A WAV states its channel count before any audio
  exists, so `wav::promote_to_stereo` rewrites the frames already written with the
  recording moved to the right channel and the left silent. One file, one `audio_path`;
  export, seek, and re-transcription keep working. It goes to a temporary beside the
  original and is renamed over it, so an interruption leaves a playable recording. Refused
  past the 32-bit size fields — around six hours at 48 kHz — rather than truncated.
- **Nothing may be left buffered across the boundary.** The mixer is drained before it
  widens; a backlog on either side at that moment would offset the two channels for the
  rest of the recording and take the speaker attribution with it.
- **Neither device failing loses the recording.** No microphone: the tap goes back and the
  session stays in listen-only, which is what the user still believes it is. No tap: the
  session carries on with the microphone and says so, loudly, while the meeting is still
  happening.
- **The file decides how a recording is transcribed**, not the mode the session ended in. A
  recording that gained a microphone and then closed it again ends in listen-only with a
  stereo file, and reading that as one mono channel would take the silent left channel for
  the whole meeting. `events.mode` is corrected the moment the switch happens, and recovery
  takes it from the file — which cannot be wrong about its own shape.
- **The scrub keeps working after the microphone closes.** Up to a minute of microphone
  audio is still held back at that point, and that is exactly the audio someone closing
  their microphone is most likely to want gone. So it is gated on the file having a mic
  channel, and the commit delay deliberately stays where it is rather than dropping to zero
  and flushing all of it to disk.

Compressing the file instead of restriding it, or writing a second file alongside, were
both considered. A second file breaks `audio_path`, `wav::extract`, seek, and the one-pass
transcription model for the sake of avoiding a rewrite that takes seconds.

**A recording that switched carries its own markers** — `no_microphone` and
`system_audio_gap` in `capture_spans` (§8). A recording that stayed in one mode gets none:
`events.mode` already says which, and marking the whole of a listen-only transcript as
having no microphone tells the reader nothing they cannot see.

---

## 5. Mute

Only meaningful in `conversation` mode.

- The mute toggle **writes zeros into the left channel** while system audio keeps recording.
- **Do not stop or reopen the input device.** Zeros only. The device stays running.
- Global hotkey **⌘⇧M** via Tauri's global shortcut plugin. Must work while the app is unfocused.
- The menu bar item reflects mute state.
- Every mute span is persisted to `mute_spans` and rendered in the transcript as
  `[mic muted — MM:SS to MM:SS]`.

**Never silently omit a muted stretch of time.** A gap in the transcript with no marker is a bug.

---

## 6. Retroactive scrub

Mic audio passes through a **60-second ring buffer** before being committed to the WAV.
**⌘⇧X zeroes the buffer**, erasing a side conversation the user only realized was sensitive after
it started.

This is a primary feature, not a nicety. The mute button only helps someone who remembers to press
it beforehand; the scrub is what covers the case where they didn't.

Consequence: the mic channel is written to disk on a 60-second delay relative to the system channel.
The writer compensates so the two channels stay sample-aligned in the final file.

---

## 7. Echo

On speakers, the other party's voice leaks through the air into the mic, appearing faintly on the
left channel and breaking speaker attribution. Handle it **two cheap ways only**:

1. **Detect and warn.** At record start and once a minute after, cross-correlate the two channels
   and look for a peak at **20–150 ms positive lag**. If found, show a non-blocking banner
   suggesting headphones.
2. **Dedupe at the text layer.** After transcribing both channels, drop any mic-channel segment
   closely matching a system-channel segment within 3 seconds. Normalized similarity,
   threshold ≈ **0.82**.

**Do not implement** acoustic echo cancellation, adaptive filters, NLMS, or double-talk detection.

---

## 8. Data model

**Segments are the source of truth.** Summaries are derived and must be regenerable from stored
segments without re-transcribing. Store segments as **rows, not a blob**, so full-text search and
click-to-seek work.

```sql
events(id, title, ai_title, calendar_event_id, started_at, ended_at,
       mode, audio_path, summary_md, model_used, created_at, transcribed_at,
       audio_deleted_at)

segments(id, event_id, channel, start_ms, end_ms, text)   -- channel is 'mic' or 'system'

mute_spans(id, event_id, start_ms, end_ms)

capture_spans(id, event_id, kind, start_ms, end_ms)       -- kind is 'no_microphone' or
                                                          -- 'system_audio_gap'

chat_messages(id, event_id, role, content, created_at)    -- role is 'user' or 'assistant'
```

FTS5 over `segments.text`.

`transcribed_at` records that a pass *finished*, not that it found anything. Segment count
cannot stand in for it: a recording of a silent room has none either way, and treating
that as "never transcribed" would re-transcribe it on every launch forever.

`capture_spans` is its own table rather than a `kind` on `mute_spans`, for the reason
`audio_deleted_at` is not an inference from `audio_path IS NULL`. The zeros look
identical and the causes are not. A muted span is a microphone that was open and
deliberately silenced; `no_microphone` is one that was never open, so nothing said in the
room *could* have reached disk. Reading the second as the first has the transcript claim
the user chose to go quiet when in fact they were not being recorded — an assertion about
somebody's behaviour, made from a gap. Written only when the mode changed (§4).

### Interrupted recordings

`ended_at IS NULL` on a stored event means the app went away mid-recording — the machine
slept, the process was killed, the power went. The audio is still there, because the WAV
header is rewritten as the recording runs, so **a startup pass repairs and adopts these**:
`wav::repair` restores the header, `ended_at` is set from the length of the audio rather
than the clock, and anything with no finished transcription pass is queued for one.

That pass runs **synchronously in `setup`, before Tauri's event loop starts.** That
ordering is load-bearing: it is the only reason every unfinished event can safely be
treated as abandoned. On a background thread it could finalise a session the user had just
started.

### One transcription at a time

Each pass runs a `faster-whisper` process that takes every core it can get. Two of them
alongside a live recording starve the writer thread, and audio the mixer drops is gone
**with no marker in the transcript** — strictly worse than a transcript arriving later. So
passes queue, oldest first, and a queued recording says so in its detail pane.

Dropped audio is reported for the same reason a muted span is written down: it is a
stretch of missing speech with nothing in the file to show it was ever captured. A live
banner while it is happening, a warning at stop. Small amounts are normal — the mixer
trims clock drift between two devices — so the alarm is a threshold, not any drop at all.

---

## 8a. Summaries

The prompt lives in `crates/hearsay-core/src/summary.rs` as one editable string. Its rules:

- **Bullets under headings, not paragraphs.** Two to four `##` sections, named after what
  *this* conversation was about — not a fixed template. A recruiting session gets
  "The role" and "How to apply"; a coffee chat gets "About Dana" and "Advice she gave".
- **A closing `Worth remembering` section** for the details that are easy to lose: names,
  roles, deadlines, numbers, and the personal things worth recalling next time. Omitted
  when the transcript has none.
- **Action items are a separate schema field**, never a heading the model writes. Owner is
  an enum — `you` / `them` / `unassigned`. Never a guessed name.
- **Nothing invented.** Undecided stays undecided; garbled stays out. A `[mic muted]` span
  is passed to the model and explicitly not to be speculated about, and so are the two
  spans a mode switch leaves: `[no microphone]`, which the model is told not to read as
  the recorder staying quiet, and `[system audio not captured]`.

The recorder is addressed **by name**, set in Settings and stored in `preferences`
(a preference, not a secret — it is written into every prompt, so it does not belong in
the Keychain). Unset falls back to "You". The name is used in the transcript labels the
model reads and in rendered action items.

Structure is enforced by a JSON schema on both providers, so summaries are never parsed
out of prose.

---

## 8b. Asking about a recording

The **Ask** tab answers questions about one recording. It exists for a specific reason:
without it, finding one detail in an hour of transcript means pasting the whole transcript
into somebody else's chat window — which sends the same text to a service the user did not
choose, under an account they may not control. This sends it to the provider and key
already configured, and only when a question is sent.

The prompt lives in `crates/hearsay-core/src/chat.rs` as one editable string, like the
summary prompt. Its rules:

- **Answers come from the transcript and nothing else.** When it does not contain the
  answer, say so and stop — no reasoning towards a likely answer, no general knowledge
  offered instead. The user is asking what was said, not what is usually true.
- **Timestamps are cited as `[MM:SS]`** and rendered as buttons that seek the audio, so an
  answer can be checked rather than trusted.
- **A `[mic muted]` span is never speculated about.** If the answer might lie inside one,
  the answer is that it might lie inside one. The same holds for the two spans a mode
  switch leaves (§4): inside a `[no microphone]` stretch the recorder may have been
  speaking and none of it was kept.
- **Garbled transcription is reported, not silently corrected.**

Free text, not a schema: an answer is prose, and there is no structure to enforce.

Conversations are stored in `chat_messages` and deleted with the event. A question whose
answer never arrives is **withdrawn** rather than left in place — kept, it would be
replayed as history on every later question, sending the model a turn it never answered.

---

## 8c. Saving the audio

The Audio tab has **Save a copy**, which writes the recording to a path the user chose in
a save sheet. Local only — this is a file copy, not a share, and no network is involved.

- The extension decides the format: **`.m4a`** (AAC at 96 kbps, roughly 43 MB an hour,
  the default the sheet prefills) or **`.wav`** (the original bytes, copied verbatim).
- **A span can be saved instead of the whole recording** — the reason to keep audio is
  usually one part of it. `wav::extract` cuts on a frame boundary and copies the sample
  bytes across without decoding them, so a span of a `conversation` recording keeps its
  two channels aligned. For an `.m4a` the cut happens first and the compression second,
  because `afconvert` converts whole files and cannot trim; the intermediate WAV goes in
  Hearsay's own directory, never `/tmp`, and is removed even when the export fails.
- **A span is named in the file**, as `<title> <date> 3m12s to 8m45s.m4a`. A clip that
  does not say which part of the meeting it is cannot be placed again later. No colons:
  the Finder renders one as a slash.
- **A selection that holds no audio is reported, not written.** Empty and reversed spans
  are refused rather than clamped, and a start past the end of the recording is an error —
  an exported file that opens and plays nothing is the same silent failure as a dead tap.
- The AAC pass is `/usr/bin/afconvert`, which ships with macOS. **Do not bundle an
  encoder** and do not depend on a Homebrew `ffmpeg` or `lame` being present.
- **`.mp3` is refused with an explanation**, not silently satisfied. macOS decodes MP3 and
  cannot encode it, and writing AAC under an `.mp3` extension would be a file that lies
  about itself.
- **The channel split survives the copy.** §4's "never mix to mono" holds here too: a
  `conversation` export stays stereo, so the exported file keeps its speaker attribution.
- A recording that is still running cannot be exported — its header understates its length
  until the next sync, so the copy would be short by an unpredictable amount with nothing
  to mark what was missing.
- Hearsay never picks the destination itself. There is no "Exports" folder, no default
  drop into `~/Downloads`; the audio leaves the app only where the user pointed it.

---

## 8d. Deleting the audio, keeping the transcript

Audio is the only thing Hearsay writes that grows without bound: 16-bit PCM at the rate
the devices negotiated, which is about **700 MB an hour in `conversation` and half that
in `listen_only`** — a recording switched mid-session (§4) is restrided, so it weighs the
`conversation` rate for its whole length, not just from the switch. Everything else is text and rounds to nothing beside it. The Audio tab
has **Delete the audio**, which removes the file and keeps everything written from it.

This works because §8 already holds: **segments are the source of truth**. The transcript,
the summary, the FTS index and the stored questions are all rows, none of them read from
the file, so all of them survive it. **Ask keeps working** — it reads `segments`, never the
WAV.

- **Always a deliberate, per-recording press.** No timer, no age policy, no background
  sweep, no "delete audio older than 30 days". Hearsay does not destroy a user's data on a
  schedule, and this is the one feature where it would be most tempting.
- **`audio_deleted_at` is its own column**, not an inference from `audio_path IS NULL`.
  Absence cannot say why. A recording whose file never arrived and one whose audio was
  thrown away on purpose must not look identical, because the consequences of the second
  have to read as a decision rather than as a fault. Same reasoning as `transcribed_at`.
- **`audio_path` is cleared in the same statement.** A dangling path would have the webview
  build an `<audio>` source that never loads, and `untranscribed_events` — which keys on
  `audio_path IS NOT NULL` — would offer the recording for transcription on every launch
  forever, against a file that is deliberately gone.
- **Three refusals, all because the audio cannot be rebuilt from anything else.** A live
  recording's file is still being written; a recording with a transcription pass in flight
  is being read right now and would fail partway; and **a recording that has never been
  transcribed is refused outright**, because deleting its audio leaves an empty row rather
  than a saving. Passes in flight are tracked by id — the ticket queue orders them but does
  not say whose they are.
- **The file goes before the row is marked.** Marking first and then failing to remove it
  would orphan the audio: still on disk, still taking the space, with nothing left pointing
  at it to try again.
- **Seek stops being offered, rather than silently doing nothing.** Transcript lines and
  the `[MM:SS]` citations in an answer become plain text — not disabled buttons, which dim
  their text and cannot be selected, and this is a transcript. A control that looks live
  and does nothing when pressed is the same class of failure as a dead tap.
- **Re-transcribing is gone for good**, and both it and Save a copy say *deleted* rather
  than *missing*. The transcript on disk is now the only one this recording will ever have.
- **The list shows what each recording weighs**, because choosing what to delete is a
  comparison and nobody makes one by opening every recording in turn.

Compressing instead of deleting was considered and is not available. The transcription
sidecar reads audio through `soundfile`/libsndfile, which does not decode AAC, so
converting recordings to the `.m4a` that §8c exports would break re-transcription in
exactly the way deleting does — while only recovering some of the space.

---

## 9. Secrets

Summary API keys live in the **macOS Keychain** via the `keyring` crate. Either
**Anthropic** or **Google Gemini** — the user picks, since it is their key and their
account the text is sent to. Only the summary call differs between them; everything else
in the app is identical either way, and both remain optional.

Never a `.env`. Never in SQLite. Never in a log line. Never in an error message.

---

## 10. Design

### Palette — use these exact values, introduce no others

| Token | Hex | Use |
|---|---|---|
| royal | `#112250` | sidebar, headings, primary text |
| sapphire | `#3C507D` | secondary text, active nav, selected borders |
| quicksand | `#E0C58F` | accent — **recording state only** |
| swan | `#F5F0E9` | list canvas, inline panels |
| shellstone | `#D9CBC2` | hairlines |
| white | `#FFFFFF` | cards, detail pane |
| danger | `#9B4A3F` | destructive confirmations only |

### Quicksand is a safety signal

Quicksand means **recording**. Start button, live indicator, mode badge. **Nothing else in the app
is ever gold** — not save buttons, not links, not highlights, not hover states.

A glance must answer "is audio being captured right now?" If gold appears anywhere else, that
glance becomes a guess.

### Layout

A control bar across the top, then two panes. No nesting, no back button.

1. **Top bar** (royal) — record button, mode toggle, source picker, nav icons. Doubles as
   the window's drag region, since the title bar is transparent.
2. **Event list** (swan canvas) — grouped by day, cards on white
3. **Detail** (white) — title, date, peer tabs *Summary / Transcript / Audio*

The controls were a left column originally. Four short controls in a full-height strip
left most of it empty, and took width away from the two panes that hold the content.

### Typography

Two macOS system faces, so nothing is ever fetched from a font CDN — a webfont would have
a privacy-first recorder phoning home to render its own chrome.

| Role | Face |
|---|---|
| Display — titles, headings, wordmark, empty states | **New York** (`ui-serif`) |
| Interface — everything else | **Avenir Next** |
| Timestamps, durations, hotkey hints | **SF Mono**, tabular numerals |

Anything that *names a thing* is set in the serif; the interface around it is the sans.
Section labels are small letterspaced caps, so the panes read as an index.

Sentence case everywhere. **Two weights only: 400 and 500** — the serif carries emphasis
through shape and size, never through a heavier cut. Light mode first.

Spacing comes from one scale (`--s1`…`--s7`, 3–30px) and is deliberately tight: this is a
working tool showing a lot of text at once, where airiness reads as emptiness.

---

## 11. Calendar

Optional and off until connected. Google OAuth via an **installed-application loopback**:
a local HTTP server on a random port receives the redirect, so no credential passes
through a URL bar. PKCE (S256) throughout.

The user supplies their own OAuth client — Hearsay embeds none, because an embedded
client would route every user's calendar access through credentials they do not control.
Client details and tokens live in the **Keychain**, never in SQLite or a file.

- Scope is `calendar.readonly`. Hearsay cannot create, change, or delete anything.
- Only event titles and times are read. Attendees, descriptions, and attachments are not
  requested and not stored.
- A recording is matched to an event by **greatest time overlap**, requiring at least a
  quarter of the recording's duration — so joining late still matches, and clipping the
  end of the previous meeting does not.
- A calendar title replaces the default `Recording, <time>` name only. A title the user
  typed is never overwritten.
- When a meeting starts, the menu bar **offers** to record. It never starts on its own.
  Offered once per meeting, never repeated.

**Summaries are never written back to the calendar.** This was considered and rejected: a
calendar event's description is visible to every guest on the invite, and Google offers
no per-person private field. Writing a summary there would push notes about a 1:1, an
interview, or a vendor call to everyone present, irreversibly. The detail pane has a
**Copy summary** button instead, which leaves the judgement of who should read it with
the person who was in the room. Adding write access would also mean upgrading the OAuth
scope beyond `calendar.readonly`, which §1 rules out.

---

## 12. Ground rules

- **Every commit builds.** If one doesn't, fix it before moving on.
- **The app runs and is usable from commit 16 onward.** No stubbed screens. No `TODO` in a code path
  the user will hit.
- `.gitignore` covers the venv, model weights, recordings, and the SQLite file. None of those are
  ever committed.
- If a macOS permission prompt appears, **stop and say exactly what to click.** Do not work around
  it, do not script around it.
- If something in the spec is wrong or won't work, **say so and stop** rather than silently
  substituting a different approach.
