# 11. Harold downloads missing speech models at start-up, pinned and checked

Date: 2026-10-23
Status: Accepted (call-first-sound-fast)

## Context

The voice worker runs with `HF_HUB_OFFLINE=1`, so a speech model missing from the Hugging Face cache made a call fail with "The voice failed." and nothing said why (20 October). The user first chose to tell and never download, and then reversed it: Harold fetches a missing model at start-up. This adds an outbound path to Hugging Face from a Mac that holds the user's Hugging Face token.

## Decision

- At start-up Harold checks the two speech models, Whisper and Kokoro, and downloads one that is missing or fails its check, in the background, one at a time.
- Only two repositories are allowed, held in a list in `models.rs`. Each is pinned to a 40-character commit set in `[voice_input] model_revision` and `[tts] model_revision`. A model outside the list is reported missing and never downloaded.
- A script, `voice_download.py`, does the download with `huggingface_hub` and `hf_xet` in the `[voice_input]` Python. It runs with `python -I`, a cleared environment (`PATH`, `HOME`, `HF_HOME`, `HF_HUB_CACHE`), `token=False` and `HF_HUB_DISABLE_IMPLICIT_TOKEN`. It never sets `HF_ENDPOINT`, `HF_HUB_ENABLE_HF_TRANSFER` or `local_dir`.
- Harold's own file check follows: every file the commit lists is present with its size and, for large files, its sha256. A record of the check is kept in `speech-models/` in the data folder, and the voice worker loads only a checked model.
- The voice worker stays offline and never downloads.
- On the iPhone's hotspot range, the start-up download waits, because a hotspot is metered, but a call starts it at once (the user's Choice B(b): they will not wait for Wi-Fi, and accept the phone data).
- Calls during a download stay in `Connecting`; a failed download tells the phone, with the model, the reason and the command.

## Consequences

- Harold contacts `huggingface.co` and the download hosts it redirects to (its CDN and Xet storage). It sends no token, no user data and no call content.
- A first start-up on a Mac with a model cached but no record runs the script once to check and record it, and downloads nothing if the files match.
- A new pin takes a settings change and a new check; weights cannot change under Harold.
- A call tapped on the hotspot downloads the missing model there, 1.6 to 2 GB of phone data, by the user's choice.
- The metered test only recognises the iPhone Personal Hotspot's range; other metered networks are not noticed.
- `huggingface_hub` 0.34.3 and `hf_xet` 1.1.5 are audited for this use only (`docs/references/dependency-audits.md`).

## Notes

- 2026-10-23: the connecting tone became a ringback that plays at every call, for at least one whole 3 s cycle, whatever the worker's state, and the ringing is when the call starts what it needs: the voice worker, and the call answerer's warm-up. A call during a download still stays in `Connecting`, but it now rings whole cycles until the models are there and the worker is ready (call-tone-no-filler).
- 2026-10-26: the TTS server downloads Kokoro and the approved TTS models by the same path (`voice_download.py`, pinned commits, Harold's own file check, one download at a time on the machine) and logs `tts model downloaded`. The voice worker loads Whisper only, so the list in `models.rs` holds Whisper only and `speech model downloaded` stays for Whisper. A call still waits for the default TTS model. See [ADR 0020](0020-each-machine-runs-one-tts-server-built-for-its-platform-and-downloads-only-approved-models.md) (tts-server).
