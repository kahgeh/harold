# Harold's TTS worker for macOS. Harold writes this file into its tts-models folder and starts
# it with the Python that has mlx_audio installed (`[voice_input] command`). It keeps one TTS
# model loaded on the Apple GPU and answers on the real stdout, one JSON line at a time:
#   {"ready": true, "load_ms": N, "device": "apple_gpu", "voices": [...]}   after loading
#   {"ready": false, "cause": "import_failed" | "model_missing" | "load_failed" | "voice_missing"}
#   {"id": N, "chunk": "<hex>", "rate": 24000}   16-bit little-endian mono PCM, hex coded
#   {"id": N, "done": true, "device": "apple_gpu"}
#   {"id": N, "error": "voice_missing" | "failed"}
# It reads one JSON request per line on stdin: {"id": N, "op": "speak", "text": "...", "voice": "..."}.
# It never downloads: Harold has fetched and checked the pinned model, and this program runs with
# HF_HUB_OFFLINE=1 and no Hugging Face token. It is given no tools.
import json
import sys
import time
import traceback
from pathlib import Path

answers = sys.stdout
sys.stdout = sys.stderr

try:
    import numpy
    import mlx_audio.tts.utils as tts_utils
except Exception:
    traceback.print_exc()
    answers.write(json.dumps({"ready": False, "cause": "import_failed"}) + "\n")
    answers.flush()
    sys.exit(0)


class LoadFailure(Exception):
    def __init__(self, cause):
        super().__init__(cause)
        self.cause = cause


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


def load(model, snapshot, commit):
    """Loads `model`. With a pinned `snapshot` folder, exactly that commit is loaded, from the
    folder when mlx_audio can read the model's name from it, else by name from the cache,
    whose refs/main must then name the pinned commit."""
    try:
        if not snapshot:
            return tts_utils.load_model(model)
        folder = Path(snapshot)
        if not folder.is_dir():
            raise LoadFailure("model_missing")
        parts = folder.parts
        # mlx_audio takes the model's name from the folder after `hub`.
        if "hub" in parts and parts.index("hub") == len(parts) - 4:
            return tts_utils.load_model(folder)
        refs = folder.parents[1] / "refs" / "main"
        if not refs.is_file() or refs.read_text().strip() != commit:
            raise LoadFailure("model_missing")
        return tts_utils.load_model(model)
    except LoadFailure:
        raise
    except Exception as error:
        traceback.print_exc()
        raise LoadFailure("model_missing" if not_in_cache(error) else "load_failed")


def voices_in(snapshot):
    """The voices the model's folder holds: the names of its `voices/*.safetensors` files."""
    if not snapshot:
        return []
    folder = Path(snapshot) / "voices"
    if not folder.is_dir():
        return []
    return sorted(path.stem for path in folder.glob("*.safetensors"))


# The first letter of a Kokoro voice name tells mlx_audio the language.
LANG_CODES = "abefhijpz"


def lang_code_of(voice, default):
    return voice[:1] if voice[:1] in LANG_CODES else default


def pcm_of(part):
    samples = numpy.asarray(part.audio).reshape(-1)
    return (numpy.clip(samples, -1.0, 1.0) * 32767.0).astype(numpy.int16).tobytes()


def speak(tts, request, default_lang_code):
    """Streams the audio of one line, one chunk for each part the model makes."""
    request_id = request["id"]
    try:
        parts = tts.generate(
            text=request["text"],
            voice=request["voice"],
            speed=1.0,
            lang_code=lang_code_of(request["voice"], default_lang_code),
        )
        for part in parts:
            answer({"id": request_id, "chunk": pcm_of(part).hex(), "rate": tts.sample_rate})
    except Exception:
        traceback.print_exc()
        answer({"id": request_id, "error": "failed"})
        return
    answer({"id": request_id, "done": True, "device": "apple_gpu"})


def main():
    started = time.monotonic()
    model = argument("--model")
    snapshot = argument("--snapshot", "")
    commit = argument("--commit", "")
    voice = argument("--voice")
    lang_code = argument("--lang-code", "a")
    try:
        tts = load(model, snapshot, commit)
        # One throwaway line, so a missing voice fails here and not on the first answer.
        try:
            for _ in tts.generate(
                text="Ready.", voice=voice, speed=1.0, lang_code=lang_code_of(voice, lang_code)
            ):
                pass
        except Exception:
            traceback.print_exc()
            raise LoadFailure("voice_missing")
    except LoadFailure as failure:
        answer({"ready": False, "cause": failure.cause})
        return
    except Exception:
        traceback.print_exc()
        answer({"ready": False, "cause": "load_failed"})
        return
    answer(
        {
            "ready": True,
            "load_ms": int((time.monotonic() - started) * 1000),
            "device": "apple_gpu",
            "voices": voices_in(snapshot),
        }
    )
    for line in sys.stdin:
        try:
            request = json.loads(line)
        except Exception:
            continue
        if request.get("op") == "speak":
            speak(tts, request, lang_code)
        else:
            answer({"id": request.get("id"), "error": "failed"})


if __name__ == "__main__":
    main()
