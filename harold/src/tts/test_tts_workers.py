# Tests for the TTS workers (tts_worker_mlx.py and tts_worker_onnx.py) against fakes of
# mlx_audio, onnxruntime and kokoro_onnx. No model is loaded and no sound is made. Run with:
#   python3 -I harold/src/tts/test_tts_workers.py
import importlib.util
import io
import json
import shutil
import sys
import tempfile
import types
import unittest
from pathlib import Path

SRC = Path(__file__).resolve().parent.parent


class FakeArray:
    """The few NumPy operations the workers use, over a list of numbers."""

    def __init__(self, values):
        self.values = list(values)

    def reshape(self, *_):
        return self

    def __mul__(self, factor):
        return FakeArray(value * factor for value in self.values)

    def astype(self, kind):
        return FakeArray(int(value) for value in self.values)

    def tobytes(self):
        return b"".join(int(value).to_bytes(2, "little", signed=True) for value in self.values)


class FakeVoices:
    def __init__(self, names, pickled=()):
        self.files = list(names)
        self.pickled = set(pickled)

    def __getitem__(self, name):
        if name in self.pickled:
            raise ValueError("Object arrays cannot be loaded when allow_pickle=False")
        return FakeArray([0.0])


def fake_numpy(voices=None):
    module = types.ModuleType("numpy")
    module.int16 = "int16"
    module.asarray = lambda values: FakeArray(values.values if isinstance(values, FakeArray) else values)
    module.clip = lambda array, low, high: FakeArray(
        max(low, min(high, value)) for value in array.values
    )
    module.load_calls = []

    def load(path, allow_pickle=True):
        module.load_calls.append((path, allow_pickle))
        return voices if voices is not None else FakeVoices(["af_heart", "am_adam"])

    module.load = load
    return module


def load_worker(file, modules, argv=None):
    """Imports a worker file as a fresh module with `modules` standing in for its libraries."""
    saved = {name: sys.modules.get(name) for name in modules}
    saved_argv = sys.argv
    sys.modules.update(modules)
    sys.argv = ["worker.py", *(argv or [])]
    try:
        spec = importlib.util.spec_from_file_location("worker_under_test", SRC / file)
        module = importlib.util.module_from_spec(spec)
        real_stdout = sys.stdout
        spec.loader.exec_module(module)
        sys.stdout = real_stdout
        module.answers = io.StringIO()
        return module
    finally:
        sys.argv = saved_argv
        for name, old in saved.items():
            if old is None:
                sys.modules.pop(name, None)
            else:
                sys.modules[name] = old


def lines(module):
    return [json.loads(line) for line in module.answers.getvalue().splitlines()]


class Part:
    def __init__(self, audio):
        self.audio = audio


# ---------------------------------------------------------------------------------------------
# The MLX worker
# ---------------------------------------------------------------------------------------------


class FakeMlxModel:
    sample_rate = 24000

    def __init__(self, fail_voice=None, fail_line=False):
        self.fail_voice = fail_voice
        self.fail_line = fail_line
        self.calls = []

    def generate(self, text, voice, speed, lang_code):
        self.calls.append((text, voice, speed, lang_code))
        if voice == self.fail_voice or (self.fail_line and text != "Ready."):
            raise RuntimeError("no such voice")
        yield Part([0.0, 0.5, -0.5])
        yield Part([2.0, -2.0])


def mlx_modules(model=None, error=None):
    utils = types.ModuleType("mlx_audio.tts.utils")
    utils.loaded = []

    def load_model(target):
        utils.loaded.append(target)
        if error is not None:
            raise error
        return model or FakeMlxModel()

    utils.load_model = load_model
    package = types.ModuleType("mlx_audio")
    tts = types.ModuleType("mlx_audio.tts")
    return {
        "numpy": fake_numpy(),
        "mlx_audio": package,
        "mlx_audio.tts": tts,
        "mlx_audio.tts.utils": utils,
    }


class MlxWorkerTests(unittest.TestCase):
    def worker(self, modules=None, argv=None):
        modules = modules or mlx_modules()
        return load_worker("tts_worker_mlx.py", modules, argv), modules

    def run_main(self, module, argv, stdin=""):
        sys.argv = ["worker.py", *argv]
        sys.stdin = io.StringIO(stdin)
        try:
            module.main()
        finally:
            sys.stdin = sys.__stdin__

    def test_a_missing_library_gives_import_failed_and_exits(self):
        broken = types.ModuleType("mlx_audio.tts.utils")

        class Broken:
            def __getattr__(self, name):
                raise ImportError(name)

        modules = {"numpy": fake_numpy()}
        saved = sys.modules.get("mlx_audio.tts.utils")
        sys.modules.pop("mlx_audio.tts.utils", None)
        sys.modules.pop("mlx_audio", None)
        real_stdout = sys.stdout
        out = io.StringIO()
        sys.stdout = out
        try:
            with self.assertRaises(SystemExit):
                load_worker("tts_worker_mlx.py", modules)
        finally:
            sys.stdout = real_stdout
            if saved is not None:
                sys.modules["mlx_audio.tts.utils"] = saved
        del broken

    def test_it_loads_the_pinned_snapshot_folder_and_says_ready_with_the_voices(self):
        root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, root, True)
        snapshot = root / "hub" / "models--org--m" / "snapshots" / ("a" * 40)
        (snapshot / "voices").mkdir(parents=True, exist_ok=True)
        (snapshot / "voices" / "af_heart.safetensors").write_bytes(b"v")
        (snapshot / "voices" / "am_adam.safetensors").write_bytes(b"v")
        modules = mlx_modules()
        module, _ = self.worker(modules)

        self.run_main(
            module,
            ["--model", "org/m", "--snapshot", str(snapshot), "--commit", "a" * 40, "--voice", "af_heart"],
        )

        ready = lines(module)[0]
        self.assertTrue(ready["ready"])
        self.assertEqual(ready["device"], "apple_gpu")
        self.assertEqual(ready["voices"], ["af_heart", "am_adam"])
        self.assertEqual(modules["mlx_audio.tts.utils"].loaded, [snapshot])

    def test_a_snapshot_that_is_not_there_gives_model_missing(self):
        module, _ = self.worker()

        self.run_main(
            module,
            ["--model", "org/m", "--snapshot", "/nonexistent/hub/x", "--commit", "a" * 40, "--voice", "af_heart"],
        )

        self.assertEqual(lines(module), [{"ready": False, "cause": "model_missing"}])

    def test_a_voice_the_model_lacks_gives_voice_missing_before_ready(self):
        modules = mlx_modules(FakeMlxModel(fail_voice="nobody"))
        module, _ = self.worker(modules)

        self.run_main(module, ["--model", "org/m", "--voice", "nobody"])

        self.assertEqual(lines(module), [{"ready": False, "cause": "voice_missing"}])

    def test_a_load_that_raises_gives_load_failed(self):
        modules = mlx_modules(error=RuntimeError("boom"))
        module, _ = self.worker(modules)

        self.run_main(module, ["--model", "org/m", "--voice", "af_heart"])

        self.assertEqual(lines(module), [{"ready": False, "cause": "load_failed"}])

    def test_a_line_streams_one_chunk_for_each_part_then_done(self):
        model = FakeMlxModel()
        module, _ = self.worker(mlx_modules(model))
        request = json.dumps({"id": 7, "op": "speak", "text": "Hello.", "voice": "am_adam"})

        self.run_main(module, ["--model", "org/m", "--voice", "af_heart"], request + "\n")

        out = lines(module)
        self.assertTrue(out[0]["ready"])
        chunks = [line for line in out if "chunk" in line]
        self.assertEqual(len(chunks), 2)
        self.assertTrue(all(line["id"] == 7 and line["rate"] == 24000 for line in chunks))
        # 16-bit PCM: 0.5 is 16383, and samples past 1.0 are clipped to full scale.
        self.assertEqual(bytes.fromhex(chunks[0]["chunk"]), (0).to_bytes(2, "little", signed=True) + (16383).to_bytes(2, "little", signed=True) + (-16383).to_bytes(2, "little", signed=True))
        self.assertEqual(bytes.fromhex(chunks[1]["chunk"]), (32767).to_bytes(2, "little", signed=True) + (-32767).to_bytes(2, "little", signed=True))
        self.assertEqual(out[-1], {"id": 7, "done": True, "device": "apple_gpu"})
        self.assertEqual(model.calls[-1], ("Hello.", "am_adam", 1.0, "a"))

    def test_the_language_code_follows_the_first_letter_of_the_voice(self):
        model = FakeMlxModel()
        module, _ = self.worker(mlx_modules(model))
        request = json.dumps({"id": 1, "op": "speak", "text": "Hi.", "voice": "bf_emma"})

        self.run_main(module, ["--model", "org/m", "--voice", "bf_emma"], request + "\n")

        self.assertEqual(model.calls[0][3], "b", "the throwaway line")
        self.assertEqual(model.calls[-1], ("Hi.", "bf_emma", 1.0, "b"))
        self.assertEqual(module.lang_code_of("custom_voice", "a"), "a")

    def test_a_line_that_fails_answers_an_error_and_the_worker_goes_on(self):
        model = FakeMlxModel(fail_line=True)
        module, _ = self.worker(mlx_modules(model))
        requests = "\n".join(
            [
                json.dumps({"id": 1, "op": "speak", "text": "x", "voice": "v"}),
                json.dumps({"id": 2, "op": "unknown"}),
                "not json",
            ]
        )

        self.run_main(module, ["--model", "org/m", "--voice", "af_heart"], requests + "\n")

        out = lines(module)
        self.assertEqual(out[1], {"id": 1, "error": "failed"})
        self.assertEqual(out[2], {"id": 2, "error": "failed"})
        self.assertEqual(len(out), 3)

    def test_the_program_imports_only_what_it_needs_and_never_downloads(self):
        text = (SRC / "tts_worker_mlx.py").read_text()
        imports = sorted(
            line.split()[1]
            for line in text.splitlines()
            if line.strip().startswith(("import ", "from "))
        )
        self.assertEqual(
            imports,
            ["json", "mlx_audio.tts.utils", "numpy", "pathlib", "sys", "time", "traceback"],
        )
        self.assertNotIn("snapshot_download", text)
        self.assertNotIn("requests", text)


# ---------------------------------------------------------------------------------------------
# The ONNX worker
# ---------------------------------------------------------------------------------------------


class FakeInferenceSession:
    def __init__(self, path, sess_options, providers, uses):
        self.path = path
        self.options = sess_options
        self.providers = providers
        self._uses = uses

    def get_providers(self):
        return self._uses


class FakeKokoro:
    def __init__(self, session, voices_file):
        self.session = session
        self.voices_file = voices_file
        self.calls = []
        self.error = None

    def create(self, text, voice, speed, lang):
        self.calls.append((text, voice, speed, lang))
        if self.error is not None and text != "Ready.":
            raise self.error
        return FakeArray([0.0, 0.25, -0.25, 0.5]), 24000


class Onnx:
    """The fakes of onnxruntime and kokoro_onnx, with what the worker asked of them."""

    def __init__(self, available=("CUDAExecutionProvider", "CPUExecutionProvider"), gpu_works=True):
        self.sessions = []
        self.kokoros = []
        self.available = list(available)
        self.gpu_works = gpu_works
        self.preloaded = False
        self.numpy = fake_numpy()

    def modules(self):
        runtime = types.ModuleType("onnxruntime")

        class SessionOptions:
            intra_op_num_threads = None

        runtime.SessionOptions = SessionOptions
        runtime.get_available_providers = lambda: list(self.available)

        def preload():
            self.preloaded = True

        runtime.preload_dlls = preload

        def inference_session(path, sess_options=None, providers=None):
            first = providers[0]
            name = first[0] if isinstance(first, tuple) else first
            if name == "CUDAExecutionProvider" and not self.gpu_works:
                uses = ["CPUExecutionProvider"]
            elif name == "CUDAExecutionProvider":
                uses = ["CUDAExecutionProvider", "CPUExecutionProvider"]
            else:
                uses = ["CPUExecutionProvider"]
            session = FakeInferenceSession(path, sess_options, providers, uses)
            self.sessions.append(session)
            return session

        runtime.InferenceSession = inference_session
        kokoro = types.ModuleType("kokoro_onnx")

        class Kokoro(FakeKokoro):
            @classmethod
            def from_session(cls, session, voices_file):
                instance = cls(session, voices_file)
                self.kokoros.append(instance)
                return instance

        kokoro.Kokoro = Kokoro
        return {"numpy": self.numpy, "onnxruntime": runtime, "kokoro_onnx": kokoro}


class OnnxWorkerTests(unittest.TestCase):
    def setUp(self):
        self.folder = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.folder, True)
        (self.folder / "model.onnx").write_bytes(b"m")
        (self.folder / "voices.bin").write_bytes(b"v")
        self.argv = [
            "--model-file", str(self.folder / "model.onnx"),
            "--voices-file", str(self.folder / "voices.bin"),
            "--voice", "af_heart",
        ]

    def worker(self, onnx):
        return load_worker("tts_worker_onnx.py", onnx.modules())

    def run_main(self, module, stdin=""):
        sys.argv = ["worker.py", *self.argv]
        sys.stdin = io.StringIO(stdin)
        try:
            module.main()
        finally:
            sys.stdin = sys.__stdin__

    def request(self, text="Hello.", voice="af_heart", id=1):
        return json.dumps({"id": id, "op": "speak", "text": text, "voice": voice}) + "\n"

    def test_it_asks_for_cuda_first_with_the_memory_cap_and_the_cpu_second(self):
        onnx = Onnx()
        module = self.worker(onnx)

        self.run_main(module)

        providers = onnx.sessions[0].providers
        self.assertEqual(providers[0][0], "CUDAExecutionProvider")
        self.assertEqual(
            providers[0][1],
            {"gpu_mem_limit": 1024 * 1024 * 1024, "arena_extend_strategy": "kSameAsRequested"},
        )
        self.assertEqual(providers[1], "CPUExecutionProvider")
        self.assertTrue(onnx.preloaded)

    def test_when_the_session_uses_cuda_the_device_is_gpu(self):
        onnx = Onnx()
        module = self.worker(onnx)

        self.run_main(module, self.request())

        out = lines(module)
        self.assertEqual(out[0]["device"], "gpu")
        self.assertIsNone(out[0]["gpu_reason"])
        self.assertEqual(out[0]["voices"], ["af_heart", "am_adam"])
        self.assertEqual(out[-1], {"id": 1, "done": True, "device": "gpu", "gpu_reason": None})
        # One session only: the CPU one is made when it is needed.
        self.assertEqual(len(onnx.sessions), 1)

    def test_a_session_that_fell_back_to_the_cpu_runs_on_the_cpu_with_one_thread_per_core(self):
        onnx = Onnx(gpu_works=False)
        module = self.worker(onnx)
        module.driver_loaded = lambda: True
        module.physical_cores = lambda: 6

        self.run_main(module, self.request())

        out = lines(module)
        self.assertEqual(out[0]["device"], "cpu")
        self.assertEqual(out[0]["gpu_reason"], "cuda_load_failed")
        self.assertEqual(out[-1]["device"], "cpu")
        cpu_session = onnx.sessions[-1]
        self.assertEqual(cpu_session.providers, ["CPUExecutionProvider"])
        self.assertEqual(cpu_session.options.intra_op_num_threads, 6)

    def test_the_reason_for_no_gpu_is_no_gpu_no_driver_or_cuda_load_failed(self):
        for present, driver, expected in [
            (False, False, "no_gpu"),
            (True, False, "no_driver"),
            (True, True, "cuda_load_failed"),
        ]:
            onnx = Onnx(gpu_works=False)
            module = self.worker(onnx)
            module.nvidia_present = lambda present=present: present
            module.driver_loaded = lambda driver=driver: driver

            self.run_main(module)

            self.assertEqual(lines(module)[0]["gpu_reason"], expected, (present, driver))

    def test_a_cuda_provider_that_is_not_installed_is_a_cuda_load_failure(self):
        onnx = Onnx(available=("CPUExecutionProvider",), gpu_works=False)
        module = self.worker(onnx)
        module.driver_loaded = lambda: True

        self.run_main(module)

        self.assertEqual(lines(module)[0]["gpu_reason"], "cuda_load_failed")

    def test_a_cuda_out_of_memory_error_makes_that_line_on_the_cpu_and_the_next_tries_the_gpu(self):
        onnx = Onnx()
        module = self.worker(onnx)
        module.physical_cores = lambda: 4
        requests = self.request("First.", id=1) + self.request("Second.", id=2)
        original = module.Engine.create
        state = {"failures": 1}

        def create(engine, text, voice):
            if text == "First." and engine.device == "gpu":
                engine.kokoro.error = RuntimeError("CUDA failure 2: out of memory")
            else:
                engine.kokoro.error = None
            return original(engine, text, voice)

        module.Engine.create = create
        self.run_main(module, requests)

        done = [line for line in lines(module) if line.get("done")]
        self.assertEqual(
            done,
            [
                {"id": 1, "done": True, "device": "cpu", "gpu_reason": "gpu_out_of_memory"},
                {"id": 2, "done": True, "device": "gpu", "gpu_reason": None},
            ],
        )
        self.assertEqual(onnx.sessions[-1].providers, ["CPUExecutionProvider"])
        del state

    def test_an_error_that_is_not_out_of_memory_fails_the_line_without_falling_back(self):
        onnx = Onnx()
        module = self.worker(onnx)
        original = module.Engine.create

        def create(engine, text, voice):
            engine.kokoro.error = RuntimeError("something else")
            return original(engine, text, voice)

        module.Engine.create = create
        self.run_main(module, self.request())

        self.assertEqual(lines(module)[-1], {"id": 1, "error": "failed"})
        self.assertEqual(len(onnx.sessions), 1)

    def test_a_voice_the_voices_file_lacks_is_voice_missing(self):
        onnx = Onnx()
        module = self.worker(onnx)

        self.run_main(module, self.request(voice="nobody"))

        self.assertEqual(lines(module)[-1], {"id": 1, "error": "voice_missing"})

    def test_the_audio_is_sent_in_half_second_pieces_as_16_bit_pcm(self):
        onnx = Onnx()
        module = self.worker(onnx)
        module.PIECE_SECONDS = 0.5

        self.run_main(module, self.request())

        chunks = [line for line in lines(module) if "chunk" in line]
        pcm = b"".join(bytes.fromhex(line["chunk"]) for line in chunks)
        self.assertEqual(len(pcm), 8)
        self.assertTrue(all(line["rate"] == 24000 for line in chunks))

    def test_the_voices_file_is_loaded_with_pickled_objects_turned_off(self):
        onnx = Onnx()
        module = self.worker(onnx)

        self.run_main(module)

        self.assertTrue(onnx.numpy.load_calls)
        self.assertTrue(all(allow is False for _, allow in onnx.numpy.load_calls))

    def test_a_voices_file_that_holds_pickled_objects_fails_to_load(self):
        onnx = Onnx()
        onnx.numpy = fake_numpy(FakeVoices(["af_heart", "bad"], pickled=["bad"]))
        module = self.worker(onnx)

        self.run_main(module)

        self.assertEqual(lines(module), [{"ready": False, "cause": "voices_unsafe"}])
        # The model was not loaded.
        self.assertEqual(onnx.sessions, [])

    def test_files_that_are_not_there_give_model_missing(self):
        onnx = Onnx()
        module = self.worker(onnx)
        self.argv = ["--model-file", "/nonexistent/m.onnx", "--voices-file", "/nonexistent/v.bin", "--voice", "af_heart"]

        self.run_main(module)

        self.assertEqual(lines(module), [{"ready": False, "cause": "model_missing"}])

    def test_the_language_follows_the_first_letter_of_the_voice(self):
        onnx = Onnx()
        module = self.worker(onnx)
        for voice, language in [("af_heart", "en-us"), ("bm_george", "en-gb"), ("jf_alpha", "ja"), ("zf_xiaobei", "cmn"), ("xx", "en-us")]:
            self.assertEqual(module.lang_of(voice), language)

    def test_the_program_imports_only_what_it_needs_and_never_downloads(self):
        text = (SRC / "tts_worker_onnx.py").read_text()
        imports = sorted(
            line.split()[1]
            for line in text.splitlines()
            if line.strip().startswith(("import ", "from "))
        )
        self.assertEqual(
            imports,
            ["glob", "json", "kokoro_onnx", "numpy", "onnxruntime", "os", "pathlib", "sys", "time", "traceback"],
        )
        self.assertNotIn("snapshot_download", text)
        self.assertNotIn("urllib", text)


if __name__ == "__main__":
    unittest.main()
