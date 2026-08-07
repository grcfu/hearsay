#!/usr/bin/env bash
# Build Hearsay and install it over the copy in /Applications.
#
#   ./install.sh              build and install
#   ./install.sh --reset-tcc  also clear the stored permission, so macOS asks again
#
# There is deliberately no auto-updater. An updater checks a server on launch, and
# "no update checks" is one of this project's non-negotiables — a recorder that phones
# home on every start is not a local-first recorder. This script does the same job
# without the network: it rebuilds from the source already on your disk.
#
# A note on why permissions sometimes need resetting. The app is ad-hoc signed, and
# macOS binds permission grants to a signature hash that changes on every rebuild. The
# entry in System Settings can therefore look switched on while pointing at a build that
# no longer exists — the toggle is on, but not for the binary you just installed. That is
# what --reset-tcc clears. A paid Apple Developer certificate is the only way to make a
# grant survive rebuilds; until then, this is the workaround.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_NAME="Hearsay.app"
BUILT="$REPO/target/release/bundle/macos/$APP_NAME"
INSTALLED="/Applications/$APP_NAME"
BUNDLE_ID="com.hearsay.app"

RESET_TCC=false
[ "${1:-}" = "--reset-tcc" ] && RESET_TCC=true

cd "$REPO"

echo "==> Building the audio helper"
./helper/build.sh >/dev/null

echo "==> Building the app (a couple of minutes)"
npm run tauri build 2>&1 | tail -3

[ -d "$BUILT" ] || { echo "error: the build produced no $APP_NAME" >&2; exit 1; }

# Quit the running copy, or the replacement lands under a process still using it.
if pgrep -f "$INSTALLED/Contents/MacOS" >/dev/null 2>&1; then
  echo "==> Quitting the running copy"
  osascript -e 'quit app "Hearsay"' 2>/dev/null || pkill -f "$INSTALLED/Contents/MacOS" || true
  sleep 2
fi

echo "==> Installing to $INSTALLED"
rm -rf "$INSTALLED"
# ditto, not cp: cp -R mangles bundle metadata and breaks the code signature.
ditto "$BUILT" "$INSTALLED"

codesign --verify --strict --deep "$INSTALLED" \
  && echo "    signature valid" \
  || echo "    warning: signature did not verify"

if $RESET_TCC; then
  echo "==> Clearing the stored permission for $BUNDLE_ID"
  tccutil reset ScreenCapture "$BUNDLE_ID" >/dev/null 2>&1 || true
  tccutil reset Microphone "$BUNDLE_ID" >/dev/null 2>&1 || true
  echo "    macOS will ask again next time you record."
  echo "    If it does not, remove Hearsay from System Settings → Privacy & Security"
  echo "    → Screen & System Audio Recording, then add it back with the + button."
fi

echo
echo "Done. Launching."
open "$INSTALLED"
