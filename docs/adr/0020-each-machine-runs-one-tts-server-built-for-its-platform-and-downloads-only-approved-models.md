# 20. Each machine runs one TTS server, built for its platform, and downloads only approved models

Date: 2026-10-26
Status: Accepted (tts-server). Amends 0011.

## Context

The desk voice ran `[tts] command`, and each line started a new Python process that loaded Kokoro again. Calls ran a second Kokoro in the voice worker. Linux (linux-omarchy-build) has no Apple GPU and no `say`. The user also wants other speech models, added on request. A model download is an outbound path, so it needs the rules of ADR 0011 and an audit.

## Decision

- Each machine runs one **TTS server**, `harold.tts.v1.TtsService`, on Harold's gRPC address. It trusts local callers (ADR 0002) and refuses a caller that is not on a loopback address with `PermissionDenied` `not_local`.
- A **TTS worker** keeps one model loaded. The target the build is for chooses it: the MLX worker (`tts_worker_mlx.py`) on macOS, the ONNX worker (`tts_worker_onnx.py`) on Linux. A build holds one of them. No setting chooses the worker.
- Harold starts the worker on the first request and stops it after `[tts] idle_minutes` (10) with no request and no call open. A call starts the worker while it rings, and keeps it until the call ends.
- Kokoro is the default model. The server downloads it on its first start and checks every file. Until the check passes, a request answers `Unavailable` `model_downloading`.
- Calls use the server. The voice worker loads Whisper only.
- The desk voice uses the server when `[tts] engine = "server"`. The default is `"command"` on the Mac and `"server"` on Linux. This Mac's hand-written Kokoro command (`mlx_audio.tts.generate`) with no `engine` becomes `engine = "server"` when Harold loads its settings. `harold --check-config` shows the result.
- A model other than Kokoro comes through `AddModel`. Harold downloads it only when `tts-models.toml` (beside `local.toml`) lists its source, repository and commit, with its worker, licence and audit row. A model that is not listed is recorded as `waiting_for_approval`, and the chief has it audited. Harold ignores a list that other users can write.
- Only safe weight formats load: `.safetensors`, `.onnx` and `.gguf`. A repository with a `.bin`, `.pt`, `.pth`, `.pkl`, `.ckpt` or `.pickle` file is refused `unsafe_format`. A safe format lowers the risk that loading a model runs code. It does not remove it.
- The worker runs with `HF_HUB_OFFLINE=1`, a cleared environment and no token. It reads text and writes audio. That is not a network sandbox: the worker is an ordinary process.
- Downloads follow ADR 0011: pinned commits, Harold's own file check, one download at a time on the machine (the speech models and the TTS models share one slot), and none on the iPhone's hotspot until a call or the user starts one.
- The default model and voice are one row of Harold's Turso store.
- Harold plays the audio: `afplay` on the Mac, `pw-play` on Linux, from a temporary WAV file at mode 0600 that Harold deletes after it plays.

### The exceptions the user approved (Q106)

- **Kokoro for Linux from a GitHub release (Choice E (a)).** The ONNX worker needs `kokoro-v1.0.onnx` and `voices-v1.0.bin` from the release `model-files-v1.0` of `thewh1teagle/kokoro-onnx`. That source is not Hugging Face, and `voices-v1.0.bin` is a `.bin` file. The release publishes no SHA-256, so Harold pins each file's size and SHA-256 in `release_download.rs`. It asks only for the two fixed URLs, follows redirects only to GitHub's release download hosts, sends no token, cookie or `Authorization` header, and keeps a file only when both pins match. The ONNX worker loads the voices file with NumPy's `allow_pickle=False` and fails when the file holds any pickled object.
- **NVIDIA's proprietary CUDA and cuDNN wheels (Choice F (a)).** The ONNX worker runs Kokoro on the NVIDIA GPU through `onnxruntime-gpu` and the `nvidia-*-cu12` wheels, pinned with hashes in `harold/src/tts/requirements.txt`. These wheels are under NVIDIA's proprietary licence, not an open one. They live inside the worker's Python environment only. The NVIDIA driver comes from the operating system. The worker falls back to the CPU when there is no GPU or driver, when a CUDA library fails to load, and for one line when the GPU runs out of memory.

## Consequences

- One Kokoro stays loaded on the machine, not two. A line starts without the load of a new Python process.
- A model you add is used by calls as well, since calls use the default model and voice.
- The Linux Python environment grows by about 1 to 2 GB of NVIDIA packages. Kokoro holds about 1 GB of the GPU's memory while loaded: the worker caps its pool at 1 GB.
- Approving a model is a person's act: a row in `tts-models.toml` with an audit. The API cannot approve.
- The release files' SHA-256 values are recorded from one download on the machine that runs Linux. Until then, the Linux download refuses with `check_failed` and keeps nothing.
- Choice B (b), any Hugging Face model at a pinned commit with no audit, was not chosen.

## Notes

- Numbering: this ADR takes 20 on the `tts-server` branch. The merge renumbers it if another ADR took 20 first.
