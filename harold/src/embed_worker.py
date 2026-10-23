"""Harold's embedding worker: keeps one embedding model loaded and answers one JSON line
per request on stdin with one JSON line on stdout.

Started as: python -I embed_worker.py --model PATH [--sha256 HEX]

First line out, once the model is loaded:
  {"ready": true, "load_ms": 240, "dimensions": 768, "runtime": "llama.cpp"}
or, when it cannot load:
  {"ready": false, "cause": "model_missing" | "model_changed" | "import_failed" | "load_failed"}

Request:  {"id": 1, "texts": ["...", "..."]}
Answer:   {"id": 1, "embeddings": [[...], [...]]}  or  {"id": 1, "error": "failed"}

Each embedding has length 1. The worker never writes a text, a path or an error message
anywhere: stderr is not read, and the answers carry no text.
"""
import argparse
import hashlib
import json
import math
import sys
import time


def say(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()


def sha256_of(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True)
    parser.add_argument("--sha256")
    args = parser.parse_args()
    started = time.time()
    try:
        with open(args.model, "rb"):
            pass
    except OSError:
        say({"ready": False, "cause": "model_missing"})
        return
    if args.sha256 and sha256_of(args.model) != args.sha256.lower():
        say({"ready": False, "cause": "model_changed"})
        return
    try:
        from llama_cpp import Llama
    except Exception:
        say({"ready": False, "cause": "import_failed"})
        return
    try:
        model = Llama(
            model_path=args.model,
            embedding=True,
            n_ctx=2048,
            n_batch=2048,
            n_ubatch=2048,
            n_gpu_layers=-1,
            verbose=False,
        )
        dimensions = len(model.embed("warm up"))
    except Exception:
        say({"ready": False, "cause": "load_failed"})
        return
    say(
        {
            "ready": True,
            "load_ms": int((time.time() - started) * 1000),
            "dimensions": dimensions,
            "runtime": "llama.cpp",
        }
    )
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        request_id = None
        try:
            request = json.loads(line)
            request_id = request.get("id")
            embeddings = []
            for text in request["texts"]:
                vector = [float(x) for x in model.embed(text)]
                norm = math.sqrt(sum(x * x for x in vector)) or 1.0
                embeddings.append([x / norm for x in vector])
            say({"id": request_id, "embeddings": embeddings})
        except Exception:
            say({"id": request_id, "error": "failed"})


main()
