# Harold's voice worker. Harold writes this file into the call's folder and starts it with
# the Python that has mlx_audio installed. It keeps Whisper and Kokoro loaded for the
# length of one call and answers one JSON line per request on the real stdout.
import json
import sys

answers = sys.stdout
sys.stdout = sys.stderr

import numpy
import soundfile
import mlx_audio.stt.utils as stt_utils
import mlx_audio.tts.utils as tts_utils


def argument(name):
    return sys.argv[sys.argv.index(name) + 1]


def answer(value):
    answers.write(json.dumps(value) + "\n")
    answers.flush()


def transcribe(stt, wav):
    result = stt.generate(wav)
    segments = getattr(result, "segments", None) or []
    probs = [float(s["no_speech_prob"]) for s in segments if "no_speech_prob" in s]
    return {
        "ok": True,
        "text": result.text.strip(),
        "no_speech_prob": min(probs) if probs else 1.0,
    }


def say(tts, voice, lang_code, text, wav):
    chunks = [
        numpy.asarray(part.audio).reshape(-1)
        for part in tts.generate(text=text, voice=voice, speed=1.0, lang_code=lang_code)
    ]
    soundfile.write(wav, numpy.concatenate(chunks), tts.sample_rate)
    return {"ok": True}


def main():
    try:
        stt = stt_utils.load_model(argument("--stt-model"))
        tts = tts_utils.load_model(argument("--tts-model"))
        voice = argument("--voice")
        lang_code = argument("--lang-code")
    except Exception:
        answer({"ready": False})
        return
    answer({"ready": True})
    for line in sys.stdin:
        try:
            request = json.loads(line)
            if request["op"] == "transcribe":
                reply = transcribe(stt, request["wav"])
            elif request["op"] == "say":
                reply = say(tts, voice, lang_code, request["text"], request["wav"])
            else:
                reply = {"ok": False}
        except Exception:
            reply = {"ok": False}
        answer(reply)


main()
