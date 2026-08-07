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

### “Apple could not verify Hearsay is free of malware”

Expected, and safe to bypass. The app is ad-hoc signed rather than notarized, because
notarizing needs a paid Apple Developer account. macOS shows this for any app it did not
get from the App Store or a registered developer — it is a statement about *provenance*,
not about the code.

To open it the first time:

**Right-click** (or Control-click) `Hearsay.app` → **Open** → **Open** in the dialog.

Double-clicking will not offer the bypass; the right-click menu is what unlocks it. You
only do this once — macOS remembers. On recent macOS you may instead need System Settings
→ Privacy & Security, scroll to the bottom, and click **Open Anyway** next to the message
about Hearsay.

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

## Connecting a calendar (optional)

Google requires you to create your own OAuth credentials. Hearsay embeds none, so your
calendar access runs through a client you control and can revoke at
[myaccount.google.com/permissions](https://myaccount.google.com/permissions).

Settings → Calendar walks you through it in the app. In short:

1. Create a project in [Google Cloud Console](https://console.cloud.google.com/projectcreate).
2. Enable the **Google Calendar API**.
3. Configure the consent screen: **External**, add your own address as a test user.
4. **Publish the app.** ⚠️ While the status is "Testing", Google expires refresh tokens
   after **7 days** and you would have to reconnect weekly. Publishing stops that. Google
   then warns the app is unverified at sign-in — click **Advanced → Go to Hearsay
   (unsafe)**. That warning is about other people trusting your app; you are the only
   person who will use it.
5. Credentials → Create credentials → OAuth client ID → **Desktop app**. Paste the client
   ID and secret into Hearsay.

No redirect URI to configure — Desktop clients accept `127.0.0.1` on any port, and
Hearsay opens a local server on a random free port to catch the sign-in. Credentials and
tokens go into the Keychain, never into the database or a file.

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
