#!/usr/bin/env bash
# Creates the transcription sidecar's virtualenv at python/.venv.
#
# Safe to re-run: it reuses an existing venv and just re-syncs the pinned requirements.
# Nothing here is installed system-wide, and nothing outside python/.venv is touched.
set -euo pipefail

PYTHON_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VENV="$PYTHON_DIR/.venv"
PYTHON_BIN="${HEARSAY_PYTHON:-python3}"

if ! command -v "$PYTHON_BIN" >/dev/null 2>&1; then
  echo "error: $PYTHON_BIN not found. Install Python 3.10 or newer, or set HEARSAY_PYTHON." >&2
  exit 1
fi

VERSION="$("$PYTHON_BIN" -c 'import sys; print("%d.%d" % sys.version_info[:2])')"
echo "Using $PYTHON_BIN (Python $VERSION)"

case "$VERSION" in
  3.1[0-9]|3.[2-9][0-9]) ;;
  *)
    echo "error: Python 3.10 or newer is required, found $VERSION." >&2
    exit 1
    ;;
esac

if [ ! -d "$VENV" ]; then
  echo "Creating virtualenv at $VENV"
  "$PYTHON_BIN" -m venv "$VENV"
else
  echo "Reusing existing virtualenv at $VENV"
fi

# --no-input so this can never sit waiting on a prompt inside an app-triggered setup.
"$VENV/bin/python" -m pip install --no-input --quiet --upgrade pip
echo "Installing pinned requirements (this takes a few minutes the first time)"
"$VENV/bin/python" -m pip install --no-input --requirement "$PYTHON_DIR/requirements.txt"

echo
"$VENV/bin/python" - <<'PY'
import faster_whisper, ctranslate2, soundfile, numpy
print(f"faster-whisper {faster_whisper.__version__}")
print(f"ctranslate2    {ctranslate2.__version__}")
print(f"soundfile      {soundfile.__version__}")
print(f"numpy          {numpy.__version__}")
PY

echo
echo "Ready. Model weights download on first transcription into"
echo "  ~/Library/Application Support/hearsay/models/"
