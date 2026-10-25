# Harold's TTS worker for Linux. Harold writes this file into its tts-models folder and starts it
# with `[tts] python`, the Python that has kokoro-onnx and onnxruntime-gpu installed. It keeps
# Kokoro loaded and runs it on the NVIDIA GPU (CUDAExecutionProvider) when one works, else on
# the CPU (CPUExecutionProvider). It answers on the real stdout, one JSON line at a time:
#   {"ready": true, "load_ms": N, "device": "gpu" | "cpu", "gpu_reason": null | "<word>",
#    "voices": [...]}                                               after loading
#   {"ready": false, "cause": "import_failed" | "model_missing" | "load_failed" |
#    "voice_missing" | "voices_unsafe"}
#   {"id": N, "chunk": "<hex>", "rate": 24000}   16-bit little-endian mono PCM, hex coded
#   {"id": N, "done": true, "device": "gpu" | "cpu", "gpu_reason": null | "<word>"}
#   {"id": N, "error": "voice_missing" | "failed"}
# gpu_reason is why the work runs on the CPU: no_gpu, no_driver, cuda_load_failed or
# gpu_out_of_memory. It reads one JSON request per line on stdin:
#   {"id": N, "op": "speak", "text": "...", "voice": "..."}.
# It never downloads, and it loads the voices file with NumPy's pickled objects turned off. It
# is given no tools.
import glob
import json
import os
import sys
import time
import traceback
from pathlib import Path

answers = sys.stdout
sys.stdout = sys.stderr

GPU_PROVIDER = "CUDAExecutionProvider"
CPU_PROVIDER = "CPUExecutionProvider"
# Kokoro's share of the GPU's memory, in bytes (1 GB), so the pool never keeps more.
GPU_MEMORY_LIMIT = 1024 * 1024 * 1024
# The audio of a line is sent in pieces of about this many seconds.
PIECE_SECONDS = 0.5

# The first letter of a Kokoro voice name tells the language.
LANGUAGES = {
    "a": "en-us",
    "b": "en-gb",
    "e": "es",
    "f": "fr-fr",
    "h": "hi",
    "i": "it",
    "j": "ja",
    "p": "pt-br",
    "z": "cmn",
}

try:
    import numpy
    import onnxruntime
    import kokoro_onnx
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


def physical_cores():
    """One thread for each physical core: the distinct (package, core) pairs in sysfs."""
    cores = set()
    for path in glob.glob("/sys/devices/system/cpu/cpu[0-9]*/topology/core_id"):
        try:
            package = Path(path).parent.joinpath("physical_package_id").read_text().strip()
            cores.add((package, Path(path).read_text().strip()))
        except OSError:
            continue
    return len(cores) or max(1, (os.cpu_count() or 2) // 2)


def nvidia_present():
    """Whether a PCI device of NVIDIA's vendor (0x10de) is on this machine."""
    for path in glob.glob("/sys/bus/pci/devices/*/vendor"):
        try:
            if Path(path).read_text().strip().lower() == "0x10de":
                return True
        except OSError:
            continue
    return False


def driver_loaded():
    return os.path.exists("/proc/driver/nvidia/version")


def why_no_gpu(available):
    """The reason the GPU is not used, when the loaded session runs on the CPU only."""
    if GPU_PROVIDER not in available:
        return "cuda_load_failed"
    if not driver_loaded():
        return "no_driver" if nvidia_present() else "no_gpu"
    return "cuda_load_failed"


def gpu_providers():
    return [
        (
            GPU_PROVIDER,
            {"gpu_mem_limit": GPU_MEMORY_LIMIT, "arena_extend_strategy": "kSameAsRequested"},
        ),
        CPU_PROVIDER,
    ]


def make_session(model_file, providers, threads=None):
    options = onnxruntime.SessionOptions()
    if threads:
        options.intra_op_num_threads = threads
    return onnxruntime.InferenceSession(model_file, sess_options=options, providers=providers)


def uses_gpu(session):
    providers = session.get_providers()
    return bool(providers) and providers[0] == GPU_PROVIDER


def check_voices(voices_file):
    """Loads every voice with pickled objects turned off, and returns the voice names. A file
    that holds any pickled object fails here."""
    try:
        voices = numpy.load(voices_file, allow_pickle=False)
        names = list(voices.files) if hasattr(voices, "files") else list(voices.keys())
        for name in names:
            voices[name]
    except Exception:
        traceback.print_exc()
        raise LoadFailure("voices_unsafe")
    return sorted(names)


def is_out_of_memory(error):
    text = str(error).lower()
    return "out of memory" in text or "failed to allocate" in text or "cudaerrormemoryallocation" in text


def lang_of(voice):
    return LANGUAGES.get(voice[:1], "en-us")


class Engine:
    """Kokoro on the GPU when the session uses it, else on the CPU."""

    def __init__(self, model_file, voices_file):
        self.model_file = model_file
        self.voices_file = voices_file
        self.voice_names = set()
        self.cpu = None
        available = onnxruntime.get_available_providers()
        try:
            session = make_session(model_file, gpu_providers())
        except Exception:
            traceback.print_exc()
            session = None
        if session is not None and uses_gpu(session):
            self.kokoro = kokoro_onnx.Kokoro.from_session(session, voices_file)
            self.device = "gpu"
            self.reason = None
            return
        self.reason = why_no_gpu(available)
        self.device = "cpu"
        self.kokoro = self.cpu_kokoro()

    def cpu_kokoro(self):
        if self.cpu is None:
            session = make_session(self.model_file, [CPU_PROVIDER], physical_cores())
            self.cpu = kokoro_onnx.Kokoro.from_session(session, self.voices_file)
        return self.cpu

    def create(self, text, voice):
        """The samples and sample rate of one line, and the device and reason it ran with.
        A CUDA out-of-memory error makes this line on the CPU; the next line tries the GPU."""
        language = lang_of(voice)
        try:
            samples, rate = self.kokoro.create(text, voice=voice, speed=1.0, lang=language)
            return samples, rate, self.device, self.reason
        except Exception as error:
            if self.device != "gpu" or not is_out_of_memory(error):
                raise
        samples, rate = self.cpu_kokoro().create(text, voice=voice, speed=1.0, lang=language)
        return samples, rate, "cpu", "gpu_out_of_memory"


def pcm_of(samples):
    values = numpy.asarray(samples).reshape(-1)
    return (numpy.clip(values, -1.0, 1.0) * 32767.0).astype(numpy.int16).tobytes()


def speak(engine, request):
    """Makes one line, then sends it in pieces of about half a second."""
    request_id = request["id"]
    if request["voice"] not in engine.voice_names:
        answer({"id": request_id, "error": "voice_missing"})
        return
    try:
        samples, rate, device, reason = engine.create(request["text"], request["voice"])
    except Exception:
        traceback.print_exc()
        answer({"id": request_id, "error": "failed"})
        return
    pcm = pcm_of(samples)
    piece = max(2, int(rate * PIECE_SECONDS) * 2)
    for start in range(0, len(pcm), piece):
        answer({"id": request_id, "chunk": pcm[start : start + piece].hex(), "rate": rate})
    answer({"id": request_id, "done": True, "device": device, "gpu_reason": reason})


def main():
    started = time.monotonic()
    model_file = argument("--model-file")
    voices_file = argument("--voices-file")
    voice = argument("--voice")
    try:
        if not (Path(model_file).is_file() and Path(voices_file).is_file()):
            raise LoadFailure("model_missing")
        if hasattr(onnxruntime, "preload_dlls"):
            # Finds the CUDA and cuDNN libraries in this environment's nvidia-*-cu12 packages.
            try:
                onnxruntime.preload_dlls()
            except Exception:
                traceback.print_exc()
        names = check_voices(voices_file)
        engine = Engine(model_file, voices_file)
        engine.voice_names = set(names)
        # One throwaway line, so a missing voice fails here and not on the first answer.
        try:
            engine.create("Ready.", voice)
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
            "device": engine.device,
            "gpu_reason": engine.reason,
            "voices": names,
        }
    )
    for line in sys.stdin:
        try:
            request = json.loads(line)
        except Exception:
            continue
        if request.get("op") == "speak":
            speak(engine, request)
        else:
            answer({"id": request.get("id"), "error": "failed"})


if __name__ == "__main__":
    main()
