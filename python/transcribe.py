#!/usr/bin/env python3
"""Hearsay's transcription sidecar.

Reads a recorded WAV, runs faster-whisper over it locally, and prints segments as JSON.

The contract mirrors the Swift audio helper deliberately, so the Rust side reads both
the same way:

    stdout   one JSON object, the final result, printed once at the end
    stderr   one JSON object per line, progress events, each with a "type"

Everything runs on this machine. The single exception is the first run for a given
model, which downloads weights from Hugging Face into --models-dir; that download emits
progress events and is never silent. After it completes, transcription needs no network.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from dataclasses import dataclass, asdict


def emit(event_type: str, **fields) -> None:
    """Writes one progress event to stderr."""
    payload = {"type": event_type, **fields}
    print(json.dumps(payload, sort_keys=True), file=sys.stderr, flush=True)


def fail(kind: str, message: str, code: int = 1) -> None:
    emit("error", kind=kind, message=message)
    sys.exit(code)


@dataclass
class Segment:
    """One transcribed span. `channel` is filled in by the caller.

    Times are milliseconds from the start of the recording, so they line up with
    mute spans and with click-to-seek in the player without any further conversion.
    """

    start_ms: int
    end_ms: int
    text: str
    channel: str


class JsonProgressBar:
    """Stands in for tqdm so model downloads report as JSON instead of terminal bars.

    huggingface_hub drives whatever it is given through the tqdm interface. Only the
    handful of methods it actually touches are implemented here; the rest would be dead
    code with no way to be exercised.
    """

    def __init__(self, *args, **kwargs):
        self.total = kwargs.get("total") or 0
        self.desc = kwargs.get("desc") or "model"
        self.disable = kwargs.get("disable", False)
        self.n = 0
        self._last_reported = -1
        # huggingface_hub's xet reporter reads this to compute a transfer rate.
        self.format_dict = {"rate": None, "n": 0, "total": self.total}

    def update(self, amount: int = 1) -> None:
        self.n += amount
        self.format_dict["n"] = self.n
        if not self.total:
            return
        percent = int(self.n * 100 / self.total)
        # One event per whole percent; a byte-level firehose helps nobody.
        if percent != self._last_reported:
            self._last_reported = percent
            emit(
                "download",
                file=self.desc,
                downloaded=self.n,
                total=self.total,
                percent=percent,
            )

    def close(self) -> None:
        pass

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False

    def __iter__(self):
        return iter(())

    # The remainder of the tqdm surface huggingface_hub touches. These are display
    # concerns with nothing to display, so they are deliberately inert rather than
    # raising — a cosmetic call must never be able to fail a download.
    def set_description(self, desc=None, refresh=True):
        self.desc = desc or self.desc

    def set_description_str(self, desc=None, refresh=True):
        self.desc = desc or self.desc

    def set_postfix(self, *args, **kwargs):
        pass

    def set_postfix_str(self, *args, **kwargs):
        pass

    def refresh(self, *args, **kwargs):
        pass

    def reset(self, total=None):
        self.n = 0
        if total is not None:
            self.total = total

    def unpause(self):
        pass

    def clear(self, *args, **kwargs):
        pass

    def display(self, *args, **kwargs):
        pass

    @staticmethod
    def write(message, *args, **kwargs):
        emit("download_note", message=str(message))


def ensure_weights(model_name: str, models_dir: str) -> str:
    """Downloads the model's weights if they are not already on disk.

    The download is done explicitly rather than left to faster-whisper so it can report
    real progress: this is the one slow, network-touching step in the whole app, and a
    silent multi-hundred-megabyte wait looks identical to a hang.

    Returns a local directory to load the model from.
    """
    from faster_whisper.utils import _MODELS
    from huggingface_hub import snapshot_download

    repo_id = _MODELS.get(model_name)
    if repo_id is None:
        # An explicit path or an unrecognised name: hand it to faster-whisper as-is.
        return model_name

    target = os.path.join(models_dir, repo_id.replace("/", "--"))
    marker = os.path.join(target, "model.bin")
    if os.path.isfile(marker):
        emit("model_cached", model=model_name, path=target)
        return target

    emit("download_start", model=model_name, repo=repo_id)
    try:
        snapshot_download(
            repo_id=repo_id,
            local_dir=target,
            tqdm_class=JsonProgressBar,
            allow_patterns=["*.bin", "*.json", "*.txt", "*.model"],
        )
    except Exception as error:  # noqa: BLE001
        fail(
            "download_failed",
            f"could not download {repo_id}: {error}. "
            "Transcription needs the model weights once; after that it works offline.",
            code=5,
        )
    emit("download_done", model=model_name, path=target)
    return target


def load_model(model_name: str, models_dir: str):
    """Loads the model, downloading it on first use.

    Kept separate from transcription so the download — the slow, network-touching,
    user-visible part — has its own clearly reported phase.
    """
    from faster_whisper import WhisperModel

    os.makedirs(models_dir, exist_ok=True)
    model_path = ensure_weights(model_name, models_dir)

    emit("model_loading", model=model_name, models_dir=models_dir)
    try:
        model = WhisperModel(
            model_path,
            device="cpu",
            # Apple Silicon runs CTranslate2 on the CPU; int8 is several times faster
            # than float32 here and the accuracy difference on speech is not audible in
            # the transcript.
            compute_type="int8",
            download_root=models_dir,
            num_workers=1,
        )
    except Exception as error:  # noqa: BLE001 - surfaced to the user verbatim
        fail(
            "model_load_failed",
            f"could not load {model_name}: {error}",
            code=3,
        )
    emit("model_ready", model=model_name)
    return model


def read_channel(path: str, channel: str):
    """Reads one channel of the recording as float32 at 16 kHz.

    Whisper wants 16 kHz mono. Resampling here rather than in Rust keeps the WAV on disk
    at full rate for playback while the model gets exactly what it expects.
    """
    import numpy as np
    import soundfile as sf

    try:
        data, sample_rate = sf.read(path, dtype="float32", always_2d=True)
    except Exception as error:  # noqa: BLE001
        fail("unreadable_audio", f"could not read {path}: {error}", code=4)

    channels = data.shape[1]
    if channel == "mono":
        # A stereo file asked for as mono is averaged; a mono file passes through.
        samples = data.mean(axis=1) if channels > 1 else data[:, 0]
    elif channel == "left":
        samples = data[:, 0]
    elif channel == "right":
        if channels < 2:
            fail(
                "missing_channel",
                f"{path} has {channels} channel(s); there is no right channel",
                code=4,
            )
        samples = data[:, 1]
    else:
        fail("bad_channel", f"unknown channel {channel!r}", code=2)

    target_rate = 16_000
    if sample_rate != target_rate:
        # Linear resampling. Whisper's own front end is far less sensitive than the
        # difference between this and a windowed-sinc filter would suggest, and it keeps
        # the dependency list to numpy.
        duration = samples.shape[0] / sample_rate
        target_length = int(round(duration * target_rate))
        if target_length <= 0:
            return np.zeros(0, dtype="float32"), 0.0
        source_positions = np.linspace(0, samples.shape[0] - 1, target_length)
        samples = np.interp(
            source_positions, np.arange(samples.shape[0]), samples
        ).astype("float32")

    duration_seconds = samples.shape[0] / target_rate
    return samples, duration_seconds


def transcribe_channel(model, samples, duration_seconds: float, channel: str, language):
    """Runs the model and returns segments, reporting progress as it goes."""
    emit("transcribe_start", channel=channel, duration_seconds=round(duration_seconds, 3))

    segments_iter, info = model.transcribe(
        samples,
        language=language,
        beam_size=5,
        # Cuts silent stretches before they reach the model. Without it, whisper is
        # prone to inventing text over silence.
        vad_filter=True,
        vad_parameters={"min_silence_duration_ms": 500},
        condition_on_previous_text=False,
    )

    results: list[Segment] = []
    last_percent = -1
    for segment in segments_iter:
        text = segment.text.strip()
        if text:
            results.append(
                Segment(
                    start_ms=int(round(segment.start * 1000)),
                    end_ms=int(round(segment.end * 1000)),
                    text=text,
                    channel=channel,
                )
            )
        if duration_seconds > 0:
            percent = int(min(segment.end / duration_seconds, 1.0) * 100)
            if percent != last_percent:
                last_percent = percent
                emit("progress", channel=channel, percent=percent)

    emit("transcribe_done", channel=channel, segments=len(results))
    return results, info


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="transcribe.py",
        description="Transcribe a Hearsay recording locally with faster-whisper.",
    )
    parser.add_argument("--audio", required=True, help="path to the recorded WAV")
    parser.add_argument(
        "--models-dir",
        required=True,
        help="where model weights live (and are downloaded to on first use)",
    )
    parser.add_argument(
        "--model",
        default="distil-large-v3",
        help="faster-whisper model name (default: distil-large-v3)",
    )
    parser.add_argument(
        "--channel",
        default="mono",
        choices=["mono", "left", "right"],
        help="which channel of the recording to transcribe (default: mono)",
    )
    parser.add_argument(
        "--language",
        default=None,
        help="force a language code; omit to detect automatically",
    )
    return parser


def main() -> int:
    args = build_parser().parse_args()

    if not os.path.isfile(args.audio):
        fail("missing_audio", f"no such file: {args.audio}", code=4)

    model = load_model(args.model, args.models_dir)
    samples, duration_seconds = read_channel(args.audio, args.channel)

    if duration_seconds <= 0:
        emit("empty_audio", channel=args.channel)
        json.dump(
            {
                "segments": [],
                "language": None,
                "duration_ms": 0,
                "model": args.model,
            },
            sys.stdout,
        )
        sys.stdout.write("\n")
        return 0

    segments, info = transcribe_channel(
        model, samples, duration_seconds, args.channel, args.language
    )

    json.dump(
        {
            "segments": [asdict(segment) for segment in segments],
            "language": getattr(info, "language", None),
            "duration_ms": int(round(duration_seconds * 1000)),
            "model": args.model,
        },
        sys.stdout,
    )
    sys.stdout.write("\n")
    sys.stdout.flush()
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        emit("cancelled", reason="interrupted")
        sys.exit(130)
