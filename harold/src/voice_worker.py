# Harold's voice worker. Harold writes this file into the call's folder and starts it with
# the Python that has mlx_audio installed. It keeps Whisper loaded for the length of one call
# and answers one JSON line per request on the real stdout. It only turns speech to text: the
# lines of a call are made by the TTS server (tts_worker_mlx.py).
# It never downloads: Harold has fetched and checked the pinned model, and this program
# runs with HF_HUB_OFFLINE=1.
import json
import sys
import time
import traceback
from pathlib import Path

answers = sys.stdout
sys.stdout = sys.stderr

try:
    import mlx_audio.stt.utils as stt_utils
except Exception:
    traceback.print_exc()
    answers.write(json.dumps({"ready": False, "cause": "import_failed"}) + "\n")
    answers.flush()
    sys.exit(0)


class LoadFailure(Exception):
    def __init__(self, cause, model=None):
        super().__init__(cause)
        self.cause = cause
        self.model = model


def argument(name, default=None):
    if name in sys.argv:
        return sys.argv[sys.argv.index(name) + 1]
    if default is None:
        raise KeyError(name)
    return default


def answer(value):
    answers.write(json.dumps(value) + "\n")
    answers.flush()


def not_in_cache(error):
    names = {cls.__name__ for cls in type(error).__mro__}
    return isinstance(error, FileNotFoundError) or "LocalEntryNotFoundError" in names


def load(utils, model, snapshot, commit):
    """Loads `model`. With a pinned `snapshot` folder, exactly that commit is loaded, from
    the folder when mlx_audio can read the model's name from it, else by name from the
    cache, whose refs/main must then name the pinned commit. Without one, by name from the
    cache. Returns the model and how it was loaded."""
    try:
        if not snapshot:
            return utils.load_model(model), "name"
        folder = Path(snapshot)
        if not folder.is_dir():
            raise LoadFailure("model_missing", model)
        parts = folder.parts
        # mlx_audio takes the model's name from the folder after `hub`.
        if "hub" in parts and parts.index("hub") == len(parts) - 4:
            return utils.load_model(folder), "folder"
        refs = folder.parents[1] / "refs" / "main"
        if not refs.is_file() or refs.read_text().strip() != commit:
            raise LoadFailure("model_missing", model)
        return utils.load_model(model), "name"
    except LoadFailure:
        raise
    except Exception as error:
        traceback.print_exc()
        cause = "model_missing" if not_in_cache(error) else "load_failed"
        raise LoadFailure(cause, model)


def transcribe(stt, language, wav):
    # Told the language, Whisper does not guess it from a clip of two or three seconds.
    result = stt.generate(wav, language=language)
    segments = getattr(result, "segments", None) or []
    probs = [float(s["no_speech_prob"]) for s in segments if "no_speech_prob" in s]
    return {
        "ok": True,
        "text": result.text.strip(),
        "no_speech_prob": min(probs) if probs else 1.0,
        "language": language,
    }


def main():
    started = time.monotonic()
    stt_model = argument("--stt-model")
    try:
        stt, stt_how = load(
            stt_utils, stt_model, argument("--stt-snapshot", ""), argument("--stt-commit", "")
        )
        language = argument("--language")
    except LoadFailure as failure:
        # Harold discards stderr; the traceback is there for someone who runs this by hand.
        reply = {"ready": False, "cause": failure.cause}
        if failure.model:
            reply["model"] = failure.model
        answer(reply)
        return
    except Exception:
        traceback.print_exc()
        answer({"ready": False, "cause": "load_failed"})
        return
    answer(
        {
            "ready": True,
            "load_ms": int((time.monotonic() - started) * 1000),
            "stt_load": stt_how,
        }
    )
    for line in sys.stdin:
        try:
            request = json.loads(line)
            if request["op"] == "transcribe":
                reply = transcribe(stt, language, request["wav"])
            else:
                reply = {"ok": False}
        except Exception:
            reply = {"ok": False}
        answer(reply)


main()
