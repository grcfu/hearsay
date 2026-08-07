# Hearsay

Local-first macOS app that records meetings, transcribes them on-device, and stores them
as searchable events. Single user, single machine, no server, no accounts, no telemetry.

## What leaves your machine

Nothing, with two exceptions. Both are off until you turn them on, and both use your own
credentials:

1. **AI summaries** — only when you press the button, using your Anthropic API key.
2. **Google Calendar** — only if you connect it. Read-only, titles and times only.
   Nothing is ever uploaded to Google.

Recordings, transcripts, and audio never go anywhere.

## What it does

- **Records system audio** using Core Audio process taps. No virtual audio device, no
  change to your sound output. Volume keys keep working, AirPods come and go.
- **Two modes.** `listen_only` (the default, every launch) records system audio only and
  never opens the microphone. `conversation` records both into one stereo WAV — left is
  you, right is everyone else.
- **Transcribes on-device** with `faster-whisper`, per channel, so you get speaker
  attribution for free.
- **Mute (⌘⇧M)** writes silence into your channel while the other side keeps recording.
- **Retroactive scrub (⌘⇧X)** erases the last 60 seconds of your microphone *before* it
  reaches disk — for the side conversation you only realised was sensitive after it
  started. Both hotkeys work while Hearsay is in the background.
- **Search everything** with full-text search and click-to-seek.
- **Optional AI summaries and calendar matching.** Without them, everything else works.

## Requirements

- macOS 14.4 or later (process taps). Built against 26.5.2.
- Apple Silicon.
- Rust, Node, and Python 3.10+ to build.

## Install

```sh
git clone https://github.com/grcfu/hearsay.git && cd hearsay
./python/setup_venv.sh     # faster-whisper — takes a few minutes
./helper/build.sh          # Swift audio helper
npm install
npm run tauri build        # produces the app
```

Then drag `target/release/bundle/macos/Hearsay.app` to `/Applications`.

To run from source instead while developing, use `npm run tauri dev`.

### On first launch

macOS will ask for **Screen & System Audio Recording**. Grant it, or recordings run
normally and capture nothing but silence — Hearsay detects this and refuses to start
rather than writing a silent file. **Microphone** is requested separately, and only the
first time you use conversation mode.

Permissions are tied to code identity, so the installed app needs its own grant even if
you already granted one to your terminal while building.

### Keep the checkout

**Do not delete the cloned repo after building.** The app finds the Python transcription
environment at `python/.venv` inside it. Move or delete the checkout and recording still
works, but transcription silently stops. The venv is not bundled into the app because
Python virtualenvs are not relocatable.

## Where your data lives

Everything is in `~/Library/Application Support/hearsay/` — the SQLite database, the
`recordings/` folder, and downloaded speech models. Nothing is written anywhere else.

> **Back it up.** There is no cloud copy, because that is the point. If the disk fails,
> every recording is gone. Point Time Machine at an external drive, or:
>
> ```sh
> rsync -a --delete ~/Library/Application\ Support/hearsay/ /Volumes/YourDrive/hearsay/
> ```
>
> Avoid iCloud or Dropbox for this — it would send your meetings to a third party.

## Current limits

- **Apple Silicon only.** No Intel build.
- **Not notarized.** The app is ad-hoc signed, so on a Mac other than the one that built
  it, Gatekeeper blocks it until you right-click → Open. Proper signing needs an Apple
  Developer account.
- **Transcription needs the checkout**, as above. There is no self-contained installer.

## Development

`cargo test` runs the full suite. Tests that need real audio and real permissions are
marked `#[ignore]`; run them with audio playing:

```sh
cargo test -p hearsay-audio --test live_capture -- --ignored --nocapture
```

See [CLAUDE.md](./CLAUDE.md) for the specification: architecture, the audio helper
contract, the data model, and the design system.

## License

Private project. All rights reserved.
