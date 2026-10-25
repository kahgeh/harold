# TTS server: speech for the desk, for calls and for the CLI

Each machine runs one **TTS server**. It turns text into audio for the desk voice, for calls and for `harold tool tts`. A **TTS worker** keeps one model loaded. Kokoro is the default model. Other models come on request, and Harold downloads them only when they are on the approved list.

See [ADR 0020](../../adr/0020-each-machine-runs-one-tts-server-built-for-its-platform-and-downloads-only-approved-models.md) for the decisions and the exceptions the user approved.

## What runs where

| | macOS | Linux |
|---|---|---|
| TTS worker | `tts_worker_mlx.py`, through `mlx_audio`, on the Apple GPU | `tts_worker_onnx.py`, through `kokoro-onnx` and ONNX Runtime, on the NVIDIA GPU, with the CPU as the fallback |
| Python | `[voice_input] command` | `[tts] python` |
| Kokoro | `mlx-community/Kokoro-82M-bf16` from Hugging Face, pinned by `[tts] model_revision` | The release files `kokoro-v1.0.onnx` and `voices-v1.0.bin` of `thewh1teagle/kokoro-onnx`, pinned by size and SHA-256 |
| Plays with | `afplay` | `pw-play` |
| Default `[tts] engine` | `"command"` (`say`) | `"server"` |

A build holds one worker. The target the build is for chooses it, and no setting does.

## The desk voice

- `engine = "command"`: nothing changes. Harold runs `[tts] command`.
- `engine = "server"`: Harold asks the server for the line, writes the audio to a temporary WAV file (mode 0600), plays it, and deletes the file. A failure runs `[tts] fallback_command` when one is set.
- This Mac's hand-written Kokoro command (`args` with `mlx_audio.tts.generate`) and no `engine` become `engine = "server"` when Harold loads its settings. The command's `--voice` stays the default voice until you set another. `harold --check-config` prints `tts_engine`, so the install check shows the result.

Voice mute and the order of a call's lines work as before. The server is in every build.

## Calls

Calls use the server. The voice worker loads Whisper only. When a call rings, Harold starts the TTS worker, so the first line of the call does not wait for the load. The worker is not stopped for being idle while a call is open, and its idle time starts when the call ends. A call waits for the default TTS model to be on the machine.

## Settings

| Setting | Default | Meaning |
|---|---|---|
| `[tts] engine` | `"command"` on the Mac, `"server"` on Linux | What speaks the desk's lines. |
| `[tts] python` | none | On Linux, the Python with `kokoro-onnx` and `onnxruntime-gpu`. On the Mac, the server uses `[voice_input] command`. |
| `[tts] idle_minutes` | 10 | Minutes with no request and no call before the worker stops. 1 to 1,440. |
| `[tts] model_revision` | the pinned commit | The commit of the Kokoro model on macOS. 40 lowercase hex characters. |
| `tts-models.toml` | none | The approved model list, beside `local.toml`. |

`[tts] command`, `args`, `voice` and the fallback keys still configure the command engine and the fallback.

## The API

`harold-api/proto/tts.proto`, package `harold.tts.v1`, service `TtsService`, on Harold's gRPC address. A caller that is not on a loopback address gets `PermissionDenied` `not_local`.

| Call | Request | Answer |
|---|---|---|
| `Speak` | `text` (1 to 4,000 characters), optional `model` and `voice` | A stream of 16-bit PCM mono audio. The first message holds the sample rate. |
| `ListModels` | none | Each model: `id`, `source`, `revision`, `worker`, `voices`, `state`, `reason`, `size_bytes`, `is_default`, `default_voice`. |
| `AddModel` | `source` (`huggingface`), `repo`, `revision` | `id` and `state`. |
| `RemoveModel` | `id` | Done. The default model cannot be removed. |
| `SetDefault` | `model`, `voice` | Done, kept in Harold's store. |

| Refusal | Code and word |
|---|---|
| Not on a loopback address | `PermissionDenied` `not_local` |
| Empty text, or text over 4,000 characters | `InvalidArgument` `invalid_text` |
| Model or voice not known | `InvalidArgument` `unknown_model`, `unknown_voice` |
| Kokoro not on the machine yet | `Unavailable` `model_downloading` |
| The model failed its check | `Unavailable` `model_missing` |
| The worker exited | `Unavailable` `worker_failed` |
| No audio within 30 seconds | `Unavailable` `timed_out` |
| Revision is not 40 hex characters | `InvalidArgument` `invalid_revision` |
| Model is not on the approved list | `PermissionDenied` `not_approved` (Harold records it as `waiting_for_approval`) |
| Model is for the other worker | `FailedPrecondition` `unsupported_worker` |
| A weight file that can run code | `FailedPrecondition` `unsafe_format` |
| Remove the default model | `FailedPrecondition` `default_model` |

## The CLI

| Command | Does |
|---|---|
| `harold tool tts models` | Lists the models. A `*` marks the default. |
| `harold tool tts add REPO@REVISION` | Asks for a Hugging Face model at a pinned commit. |
| `harold tool tts remove ID` | Removes a model from the list. The files stay in the Hugging Face cache, which other programs share. |
| `harold tool tts default MODEL VOICE` | Sets the default model and voice. |
| `harold tool tts speak TEXT` | Says the text at the desk. |

## The approved model list

`tts-models.toml` sits beside `local.toml`. Create it with mode 0600. Harold ignores a list that other users can write, and reads it each time a model is asked for, so an entry takes effect without a restart.

```toml
[[model]]
source = "huggingface"
repo = "org/name"
revision = "<40 hex characters>"
worker = "mlx"          # or "onnx": the worker that runs it
licence = "Apache-2.0"
audit = "docs/references/dependency-audits.md row N"
```

Steps for a new model:

1. Run `harold tool tts add org/name@<commit>`. Harold answers `waiting for approval` and lists the model as `waiting_for_approval`.
2. The chief has the model audited: its source, licence, file formats and size. The chief adds a row to [dependency audits](../dependency-audits.md).
3. Add the entry to `tts-models.toml`.
4. Run the same `add` again. Harold lists the repository's files, refuses a `.bin`, `.pt`, `.pth`, `.pkl`, `.ckpt` or `.pickle` weight file, downloads the commit, checks each file and lists the model `ready`.

On Linux, a Hugging Face model needs `huggingface_hub` in `[tts] python`. The pinned Linux set does not hold it.

## Downloads

- Hugging Face models use `voice_download.py` as ADR 0011 describes: `python -I`, a cleared environment, no token, a pinned commit and Harold's own file check. The record of each check is in `tts-models/` in the data folder (mode 0700).
- Kokoro on Linux uses `harold/src/tts/release_download.rs`. It asks only for the two fixed URLs, follows redirects only to GitHub's release download hosts, and sends no token, cookie or `Authorization` header. It writes a temporary file, computes the SHA-256 while it writes, and renames the file into place only when the size and the SHA-256 equal the pins.
- One download runs at a time on the machine, and none starts on the iPhone's hotspot until a call or the user starts one.

### Recording the Linux pins

The SHA-256 of the two release files is not recorded in the code yet: the machine that built the server does not download real models. Until you record it, the Linux download refuses with `check_failed` and keeps nothing. On the Linux machine:

1. Download each file once into a scratch folder.
2. Run `sha256sum` on each file.
3. Put the two values in `FILES` in `harold/src/tts/release_download.rs`, in place of the 64 zeros.
4. Add the values to row 14 of [dependency audits](../dependency-audits.md).

## The GPU on Linux

The ONNX worker starts `CUDAExecutionProvider` first, with `gpu_mem_limit` set to 1 GB and `arena_extend_strategy` set to `kSameAsRequested`, and `CPUExecutionProvider` second. It checks which one the loaded session uses.

- On the GPU, `device` is `gpu`.
- With no NVIDIA GPU, no driver, or a CUDA library that fails to load, the worker runs on the CPU with one thread for each physical core. It logs `tts gpu unavailable` with a `reason` (`no_gpu`, `no_driver`, `cuda_load_failed`), once for each reason in one Harold run.
- If a CUDA out-of-memory error comes while the worker makes a line, the worker makes that line on the CPU, reports `device = cpu`, and logs `tts gpu unavailable` with `gpu_out_of_memory`. The next line tries the GPU again.

The Linux Python packages are pinned with hashes in `harold/src/tts/requirements.txt`. Install them with `pip install --require-hashes --only-binary=:all: --no-deps -r requirements.txt`. The NVIDIA driver comes from the operating system: linux-omarchy-build checks it.

## Telemetry

| Signal | Kind | Fields |
|---|---|---|
| `harold.tts.speak` | span | `model`, `caller` (`desk`, `call`), `result` (`spoke`, `failed`), `first_audio_ms`, `loaded`, `device` (`gpu`, `cpu`, `apple_gpu`) |
| `tts gpu unavailable` | warn, once for each reason in a run | `reason` |
| `tts failed` | warn | `reason` (`worker_failed`, `model_downloading`, `model_missing`, `timed_out`), `model`, `caller` |
| `tts model downloaded` | info | `model`, `bytes`, `ms` |
| `tts model download failed` | warn | `model`, `reason` (`not_approved`, `unsafe_format`, `check_failed`, `network`, `disk_full`, `not_allowed`, `failed`) |

No field holds the text. See [the telemetry reference](../telemetry/README.md) for the cards.

## What is not measured yet

The build uses fakes and fake servers only. These checks are for the user to run on each machine:

1. Speak a desk line, make a call, and add one approved model.
2. Read `first_audio_ms` with the model loaded and after a stop.
3. On the Omarchy machine, measure a short and a long line on the GPU and, with CUDA turned off for the test, on the CPU. Record both here. The aims are a p95 under 150 ms on the GPU and under 500 ms on the CPU and on the Mac, with the model loaded.
4. Record the Linux pins (above).

## Troubleshooting

- A line is silent on the Mac: read `tts failed` in `harold.log`. `model_downloading` means Kokoro is not on the machine yet. Run `harold tool tts models`.
- `tts gpu unavailable` on the Omarchy machine: the voice runs on the CPU. Check `nvidia-smi` and the CUDA 12 packages in the worker's environment.
- A model stays `waiting_for_approval`: it is not in `tts-models.toml`, or the file is writable by other users.
