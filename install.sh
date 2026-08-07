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
# Permissions and rebuilds. macOS identifies an app by its "designated requirement". For
# an ad-hoc signed app that requirement is a content hash, which changes on every build —
# so every rebuild looks like a brand new app and the permission has to be granted again.
#
# Signing with a real certificate changes the requirement to bundle ID + certificate,
# which does not change when the code does. This script finds a code-signing identity in
# your keychain and uses it, so grants survive rebuilds. An "Apple Development" identity
# is enough and is free with any Apple ID — no paid Developer Program needed.
#
# With no identity available it falls back to ad-hoc, and then --reset-tcc is the way to
# clear a stale grant after each rebuild.
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

# Prefer a real signing identity: it is what makes permission grants survive rebuilds.
IDENTITY="${HEARSAY_SIGNING_IDENTITY:-}"
if [ -z "$IDENTITY" ]; then
  IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
    | awk -F'"' '/"/ {print $2; exit}')"
fi

if [ -n "$IDENTITY" ]; then
  echo "==> Signing with: $IDENTITY"
  # Nested code first, then the bundle — the order codesign requires.
  codesign --force --sign "$IDENTITY" --options runtime \
    "$BUILT/Contents/Resources/hearsay-audio-helper" >/dev/null 2>&1 || true
  codesign --force --deep --sign "$IDENTITY" \
    --entitlements "$REPO/src-tauri/entitlements.plist" --options runtime \
    "$BUILT" 2>&1 | tail -1
else
  echo "==> No code-signing identity found; leaving the ad-hoc signature in place."
  echo "    Permission grants will not survive rebuilds. Create a free identity in"
  echo "    Xcode (Settings → Accounts → Manage Certificates → + → Apple Development)."
fi

echo "==> Installing to $INSTALLED"
rm -rf "$INSTALLED"
# ditto, not cp: cp -R mangles bundle metadata and breaks the code signature.
ditto "$BUILT" "$INSTALLED"

codesign --verify --strict --deep "$INSTALLED" \
  && echo "    signature valid" \
  || echo "    warning: signature did not verify"

# The line that decides whether the permission survives the next rebuild. A requirement
# mentioning a certificate is stable; one mentioning cdhash is not.
REQUIREMENT="$(codesign -d -r- "$INSTALLED" 2>&1 | grep 'designated' || true)"
case "$REQUIREMENT" in
  *cdhash*) echo "    note: identified by content hash — the permission will need"
            echo "          re-granting after the next rebuild." ;;
  *certificate*) echo "    identified by certificate — permission grants persist across rebuilds." ;;
esac

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
