#!/usr/bin/env bash
# Builds the Hearsay audio helper into bin/hearsay-audio-helper.
#
# The Info.plist is embedded directly into the binary's __TEXT,__info_plist section.
# A bare command-line tool has no bundle, and without an embedded plist macOS has no
# usage-description string to show in the system-audio permission prompt.
#
# The binary is ad-hoc signed. TCC keys its permission grants on code identity; an
# unsigned binary gets a new identity on every rebuild, which means re-approving the
# permission after every build.
set -euo pipefail

HELPER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HELPER_DIR/.." && pwd)"
OUT_DIR="$REPO_ROOT/bin"
OUT="$OUT_DIR/hearsay-audio-helper"

CONFIG="${1:-release}"
case "$CONFIG" in
  release) SWIFT_FLAGS=(-O) ;;
  debug)   SWIFT_FLAGS=(-Onone -g) ;;
  *) echo "usage: build.sh [release|debug]" >&2; exit 64 ;;
esac

mkdir -p "$OUT_DIR"

swiftc \
  "${SWIFT_FLAGS[@]}" \
  -target arm64-apple-macos14.4 \
  -framework CoreAudio \
  -framework AudioToolbox \
  -framework AppKit \
  -Xlinker -sectcreate \
  -Xlinker __TEXT \
  -Xlinker __info_plist \
  -Xlinker "$HELPER_DIR/Info.plist" \
  -o "$OUT" \
  "$HELPER_DIR"/Sources/*.swift

codesign --force --sign - --identifier com.hearsay.audio-helper "$OUT"

echo "built $OUT" >&2
