# Hearsay

Local-first macOS app that records meetings, transcribes them on-device, and stores them as
searchable events.

Single user. Single machine. No server, no accounts, no telemetry. The only thing that ever
leaves the machine is an explicit summary request to the Anthropic API, using your own key.

## What it does

- **Records system audio** using Core Audio process taps — no virtual audio device, no change to
  your sound output. Volume keys keep working, AirPods come and go, nothing to set up before a
  meeting.
- **Two modes.** `listen_only` (the default, every launch) captures system audio only and never
  opens the microphone. `conversation` captures both into one stereo WAV — left channel is you,
  right channel is everyone else.
- **Transcribes on-device** with `faster-whisper`. Per-channel, so you get speaker attribution for
  free.
- **Mute and retroactive scrub.** ⌘⇧M zeros the mic channel going forward. ⌘⇧X wipes the last
  60 seconds of mic audio you already spoke — for the side conversation you only realized was
  sensitive after it started.
- **Search everything.** Segments are stored as rows with FTS5 full-text search and click-to-seek.
- **Optional AI summaries.** With an API key in your Keychain you get a title, a markdown summary,
  and action items. Without one, everything else still works.

## Requirements

- macOS 14.4 or later (process taps). Developed against 26.5.2.
- Apple Silicon.
- Rust, Node, Python 3.

## Getting started

```sh
./python/setup_venv.sh     # faster-whisper + deps
./helper/build.sh          # Swift audio helper
npm install
npm run tauri dev
```

The first launch will ask for **Screen & System Audio Recording** permission (needed for process
taps) and, in `conversation` mode only, **Microphone** permission.

## Where things live

Recordings, the SQLite database, and downloaded model weights all live in
`~/Library/Application Support/hearsay/`. Nothing is written anywhere else.

## Development

See [CLAUDE.md](./CLAUDE.md) for the full specification: architecture, the helper contract, the
data model, and the design system.

## License

Private project. All rights reserved.
