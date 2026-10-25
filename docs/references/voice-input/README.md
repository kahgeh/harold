# Voice input: calls with the chief

A call lets you talk to the chief of pods at the Mac instead of typing. You press one key in tmx-agent-dash. Harold listens through the microphone, turns what you say into text, types the text into the chief's pane, and plays the chief's spoken answer. Harold then listens again, until you end the call.

The chief is the first agent pane of the chief's tmux session among the agents running in a folder `[notify] chief` matches. Other agents in that folder do not hide it.

Calls need the `calls` feature, which is in `mac-defaults`. A build without it has no call answerer, no call models and no echo test page. Its web app call routes answer 409, and it refuses a `[voice_input]` section at start.

## Problem

Before calls, the chief could talk to you but you could not talk back. The chief answers aloud with `harold speak`, and Harold says the line with the local Kokoro voice. The only way to reach the chief was to type in its pane.

A call closes that loop. Harold never takes the chief's own voice for yours: it plays everything itself, so it knows exactly when a sound plays and does not listen then ([Not hearing the chief](#not-hearing-the-chief)).

## Do not send the chief a voice message from the phone

The sections below cover the desk's call. The web app's call, built when `[web_app]` is set (see [The phone's side](#the-phones-side)), is the phone's way to talk to the chief. Do not record a voice message to the chief from the phone outside a call. Harold drops it.

The reason: Messages stores a recorded message as an attachment, which reaches Harold as a row with no text and only an attachment placeholder character. In older versions, Harold sent its own voice notes as attachments, and their echoes still come back as the same kind of row. Harold therefore ignores every attachment-only row (`is_attachment_only` in `harold/src/channels/imessage.rs`), and your recorded message with them. Type a text message from the phone instead.

## The call

### Starting and ending

In tmx-agent-dash, put the cursor on the chief's row and press `c`. The chief's row is the one whose pane Harold names in its answer to the dash, so the button follows the chief if it restarts in another pane.

| Key | On the chief's row | On any other row |
|---|---|---|
| `c` | Starts a call when none is on and Harold's voice is not muted. Ends the call during one. | Nothing |
| `t` | Talk now, while the chief speaks ([Talk now](#talk-now)). | Nothing |

The command palette (opened with `/`) also lists `Call: start` when a call can start and `Call: end` during a call, from any row. Typing `call` in the palette finds them. Talk now has no palette entry. The dash captures no mouse events, so the buttons are pressed only with their keys. The footer lists `c call` and `t talk now`.

When a call starts, Harold:

1. stops any line it is saying at the desk;
2. checks that an agent runs in the chief's folder (if none does, it says `There's no chief running.` and the call ends);
3. starts `caffeinate -d -w <Harold's pid>`, which keeps the display awake for the length of the call, and exits by itself when Harold does;
4. starts `ffmpeg` with avfoundation on the default microphone, reading 16 kHz mono samples;
5. starts the voice worker, or takes the one already loaded, and waits first for the speech models to be on the Mac ([The voice worker](#the-voice-worker), [Speech models](#speech-models)).

Every call starts *connecting*, like a phone call, whatever the worker's state. Harold plays the ringback, "ring-ring": a 400 Hz and 450 Hz pair for 0.4 s, 0.2 s of quiet, the pair again for 0.4 s, then 2.0 s of quiet, 3.0 s a cycle. At least one whole cycle plays, and the ring never stops part-way. At the end of a cycle Harold checks whether the call is ready: the voice worker is ready, and the call answerer's warm-up (below) has finished or passed its 5 s limit. If not, another whole cycle plays. If so, Harold plays the pick-up, two quick soft notes rising from 660 Hz to 880 Hz in 0.15 s, and when it ends the call is listening. So the tap-to-listening time is at least 3.15 s. The phone's screen shows `Connecting…` until then; the dash's button shows `listening`, since the `Call` RPC has no value for it.

The ringing is sound like any other: the microphone's pieces are dropped while it plays (reason `playing`), so you are not heard before the pick-up, and no turn can start. The ringing is when the call starts what it needs. The voice worker starts if no worker is loaded or loading (it loads in about 3 s from memory and 6 to 8.5 s cold, so a call whose page loaded it rings once, and a cold one rings two or three cycles). With the call answerer on, Harold reads the call facts (todos, queue, pods) once, as for a turn, and keeps them for the call's first turn if that turn comes within 30 s; and it runs `<claude cli> --version` once, which makes no model call and costs nothing, so the CLI's files are in memory for the first answer. The warm-up has a 5 s limit; past it, the call picks up anyway. The span `harold.call_ring` records the cycles, whether the worker was `ready` or `loaded_now`, and the warm-up's `warm_ms` ([the telemetry reference](../telemetry/README.md)).

### One turn

1. You speak. Harold finds the end of your speech ([The end-of-speech rule](#the-end-of-speech-rule)) and writes the turn's audio to a WAV file in a new folder under `$TMPDIR/harold-voice-input/`.
2. The voice worker transcribes the file with Whisper large-v3-turbo. Harold deletes the turn's folder as soon as the answer returns, also when transcription fails.
3. With the call answerer on (`[call_answerer] enabled`, the default), a small model answers the turn; only a request for work reaches the chief, as a hand-over. Harold says nothing before the answer, and if the answer takes more than 3 s the soft tick plays every 3 s. A turn that ends the call (`bye`) or is not heard gets its own phrase. [The call answerer](call-answerer.md) describes it. The steps below are what happens when it is off or cannot answer, and every turn went this way before it existed. Harold drops the turn or types it:
   - **Dropped** when the transcript is empty, or when Whisper's no-speech probability is above 0.6. Harold says `Sorry, I didn't catch that.` and listens again. The probability is the lowest over Whisper's segments, and 1.0 when there are none.
   - **Typed** otherwise. Harold types `🎙 <transcript>` and Enter into the chief's pane, with the rules timed prompts use: the pane must be idle and its input box empty. It takes the same per-pane lock as other writers, so a spoken line never interleaves with an alert or a timed prompt. Like a phone reply, it is not held back by the one-minute "typed recently" hold, because you are speaking now. The turn is recorded in harold-telemetry with `started_by = voice`.
4. If the chief's answer has not started 3 seconds after the turn is typed, Harold plays a soft tick every 3 seconds, so the call is not silent. The button shows `chief thinking`.
5. The chief answers with `harold speak`. During a call, the voice worker makes the line into audio with Kokoro and the call plays it with `afplay`. If the worker cannot make a line, or `afplay` cannot play it, Harold says it with the `[tts]` command instead. When the answer ends, Harold listens again.

The chief treats `🎙` lines as possibly misheard, and confirms anything consequential before acting. That is the chief's behavior, not Harold's: the transcript is typed without a confirmation step.

### When something gets in the way

| Case | What happens |
|---|---|
| The chief's pane is busy | With the call answerer on, an answered turn needs no idle pane, and a hand-over is typed into a busy chief ([hand-overs](call-answerer.md#hand-overs)). A turn that falls back to the chief: Harold says `The chief is busy. I'll pass it on when it's free.` It tries to type the turn every second, for up to 2 minutes, and listens meanwhile. A second turn spoken meanwhile is joined to the waiting one with a space, so the chief gets one `🎙` line. After 2 minutes the turn is dropped, with no phrase. When the call ends before the turn is typed, the turn keeps waiting ([A turn waiting for a busy chief](#a-turn-waiting-for-a-busy-chief)). |
| The chief takes longer than 3 seconds | The tick, every 3 seconds. At 90 seconds Harold says `No answer yet.` and listens again. |
| The chief's turn ends without `harold speak` | Harold reads the chief's pane every 500 ms. When it sees the pane busy and then idle, and no line arrives within 2 seconds, it says the desk's turn-end update for that prompt, so every turn gets an answer. |
| Nothing was heard, or the transcript is empty | `Sorry, I didn't catch that.`, and listening again. |
| No chief is running | `There's no chief running.`, and the call ends. |
| A line is typed that cannot be sent (Enter failed) | The turn is dropped with result `failed`, and the text is not typed again. |

Another agent's turn-end update that arrives during a call is said into the call, not through the `[tts]` command. It waits while you are speaking, and plays when your turn is cut, so playing never chops your speech.

### Ending

The call ends in one of these ways. Each is the `reason` of the `call ended` log line.

| `reason` | Cause |
|---|---|
| `stopped` | You pressed `c` again, or chose `Call: end`. |
| `bye` | You said a hang-up word ([Hanging up by voice](#hanging-up-by-voice)). |
| `no_chief` | No chief was running at the start, or none was found when Harold typed a turn. |
| `voice_failed` | The voice worker failed to load or died twice ([If the worker dies](#if-the-worker-dies)), or the call's folder could not be made. |
| `microphone_failed` | At the desk, `ffmpeg` could not start, or ended during the call. On the phone, the page found no sound from the microphone 3 seconds after the call opened (it asked for `DELETE /call?why=no_sound`), or nothing was posted for 15 seconds. |
| `shutdown` | Harold stopped while the call was open. |
| `idle` | For 10 minutes no turn was cut and no sound played. Harold says `Bye.` and ends the call ([Where this differs from the original design](#where-this-differs-from-the-original-design)). |
| `voice_muted` | Harold's voice was muted during the call ([No call while Harold's voice is muted](#no-call-while-harolds-voice-is-muted)). |

When the voice or the microphone fails, Harold says `The voice failed. Call ended.` through the `[tts]` command. Ending the call stops `ffmpeg`, the voice worker and `caffeinate`, deletes the call's folder, and clears the transcript the dash shows. A turn still waiting for a busy chief is not dropped: it keeps its retry after the call ends ([A turn waiting for a busy chief](#a-turn-waiting-for-a-busy-chief)).

The 10-minute clock starts again on every turn cut and every sound played, the tick included. When it runs out while something is going on (you are speaking, a sound plays or waits, a turn waits for a busy chief, or Harold is transcribing or waiting for the chief), the clock starts again instead.

### Hanging up by voice

Harold checks each transcript before typing it. It takes the transcript's last words, ignoring case and punctuation. When they are `bye`, `bye bye`, `goodbye` or `bye chief`, the turn ends the call.

| You say | Harold types | The call |
|---|---|---|
| `Bye.` | Nothing. The chief hears nothing of the hang-up. | Says `Bye.` and ends. |
| `approve q19, bye` | `🎙 approve q19` | Says `Bye.` and ends. |
| `say bye to the old store first` | The whole line | Goes on. A `bye` inside the sentence does not hang up. |

The chief's answer to a last turn arrives as it would with no call open: said aloud at the desk.

A turn said before `bye` while the chief is busy keeps its ordinary 2-minute retry after the call has ended. Harold hands it to a background job that tries every second, holds no audio, and gives up after 2 minutes.

### A turn waiting for a busy chief

When Harold has said `The chief is busy. I'll pass it on when it's free.`, the turn reaches the chief even if the call ends first. You press `c`, say `bye`, or the call ends by itself (idle, a muted voice, or a failed voice), and Harold hands the waiting turn to the same background job a turn said before `bye` uses. The job tries every second, holds no audio, and logs `spoken prompt typed` once the chief is free. It gives up 2 minutes after the turn was first held, not 2 minutes after the call ended, and then logs `spoken prompt dropped` with `reason = chief_busy`. A turn said before `bye` that joins a waiting turn keeps that turn's 2 minutes too. When Harold itself stops, the waiting turn is dropped and nothing more runs.

### The button

The button sits at the right edge of the chief's work cell, so a call never changes the list's column widths. During a call the cell's text is the last thing typed, `🎙 <transcript>`, or else what became of the last turn (`nothing heard`, `chief busy`, `failed`).

The dash asks Harold for the call's state once when its stream opens, every 500 ms while a call is on, and every 5 seconds otherwise, so the button finds the chief's pane before any call. A poll that fails hides the button and shows no status line, so a dash talking to an older Harold without calls shows nothing. Only a failed `c` or `t` press shows a status.

| State | Button | Colour |
|---|---|---|
| No call | `[c] 📞 call` | ink on a dark key |
| Listening | `[c] ● listening · end call` | green |
| Transcribing, or waiting for the chief | `[c] … chief thinking · end call` | amber |
| The chief speaking | `[c] 📞 ▂▄▆▄▂ chief speaking · end call` and `[t] talk now` | amber, bars moving |
| `[voice_input]` absent | `[c] 📞 call: not set up` | muted; `c` does nothing |
| Harold's voice is muted | `[c] 📞 call: voice muted` | muted; `c` does nothing until you unmute |

The bars use `▁▂▃▄▅▆▇` and step once every 150 ms. They come from the clock, so the dash's redraw rate (every 100 ms) does not change.

**Narrow terminals.** The button uses the full text whenever it fits in the work cell, and otherwise shortens in this order:

1. Drop the state words and keep the glyph or bars and `end call`: `[c] ● end call`, `[c] … end call`, `[c] 📞 ▂▄▆▄▂ end call`.
2. While the chief speaks, drop the bars too: `[c] 📞 end call`.

While the chief speaks, `[t] talk now` is kept ahead of the state words. The pair shrinks as `[c] 📞 ▂▄▆▄▂ chief speaking · end call [t] talk now`, then `[c] 📞 ▂▄▆▄▂ end call [t] talk now`, then `[c] 📞 end call [t] talk now`, then `[c] 📞 end call [t] talk`, then `[c] 📞 end call` alone. The last step hides the talk-now button, and `t` still works.

The other states shorten like this: off is always `[c] 📞 call`; not set up and voice muted shorten to `[c] 📞 not set up` and `[c] 📞 voice muted`. The narrowest text is used when none fits. The dash's tests cover `end call` at every width.

### Talk now

You cannot talk over the chief: speech during an answer is not heard. To cut an answer short, press `t` on the chief's row while the chief speaks. Harold stops the sound and listens at once.

After talk now, lines the chief sends until your next turn is typed are reported as said and not played. Other agents' updates and the restart phrase (`The voice stopped. One moment.`) wait until your next turn is cut, then play. If you start no turn within 10 seconds of talk now, they play then, so a delivery waiting on them is never held for long. The chief's answer to a turn and Harold's own replies to a turn (such as `Sorry, I didn't catch that.`) keep playing.

### The display and the lock screen

During a call Harold holds `caffeinate -d -w <Harold's pid>` as a child program, so the display does not sleep and the screen does not lock from being idle. It is stopped when the call ends, whatever the reason. With `-w`, `caffeinate` also exits when Harold does, even when Harold is killed without stopping it. If it cannot start, the call goes on and `harold.log` gets `display not kept awake`.

A known limit: a lock you make by hand still sends the chief's line to the phone, because Harold sees the screen as locked.

### No call while Harold's voice is muted

A call plays its sound through Harold's voice, so Harold starts no call while the voice is muted. The state stays off, the last result is `voice muted`, and the log gets `call not started` with `reason = voice_muted`. The button shows `[c] 📞 call: voice muted`, and `c` does nothing until you unmute the voice.

Muting the voice during a call ends the call at once, with no last words. The call checks the mute as each block of samples arrives. The state turns off, the last result is `voice muted`, and the log gets `call ended` with `reason = voice_muted`.

### Where this differs from the original design

Harold differs from the original design in these behaviours, each for the reason given:

- **The button shortens in a narrow terminal.** The original design gives only the full button texts. Harold drops the state words first, then the bars, and keeps `[t] talk now` ahead of the state words ([Narrow terminals](#the-button)). The reason: the button sits inside the chief's work cell so a call never changes the list's column widths, and in a narrow terminal the full text would cut off `end call`, the words that say how to end the call.
- **A turn said before `bye` keeps its 2-minute retry after the call ends.** The original design drops any turn not yet typed when the call ends. Harold passes the turn on once the chief is free ([Hanging up by voice](#hanging-up-by-voice)). The reason: you said the turn for the chief, and hanging up in the same breath should not lose it. The retry needs no microphone or speaker.
- **`caffeinate` keeps the display awake for the call.** The original design does not cover the screen. Harold holds `caffeinate -d -w <Harold's pid>` while a call is open ([The display and the lock screen](#the-display-and-the-lock-screen)). The reason: when the screen locks from being idle, Harold sends the chief's line to the phone instead of saying it into the call. The known limit: a lock you make by hand still sends the line to the phone.
- **START refuses while Harold's voice is muted.** The original design does not cover Harold's mute. Harold starts no call, and the button shows `[c] 📞 call: voice muted` ([No call while Harold's voice is muted](#no-call-while-harolds-voice-is-muted)). The reason: a call plays its sound through Harold's voice, so a call with the voice muted could hear you but never answer.
- **A forgotten call ends by itself.** A call with no turn cut and no sound played for 10 minutes says `Bye.` and ends with `reason = idle`. The original design has no such limit. The reason: `caffeinate` keeps the Mac awake and unlocked for as long as a call is open, so a call left open must end by itself.
- **Muting the voice ends an open call.** The original design does not cover Harold's mute. In the same spirit as START refusing while the voice is muted, muting the voice during a call ends it at once, with `reason = voice_muted`.
- **Held lines wait at most 10 seconds after talk now.** The original design holds other agents' updates until your next turn. Harold plays them 10 seconds after talk now when you start no turn, because another agent's update is delivered from a thread that waits until its line has been said, and a held line would stall that delivery.
- **A turn Harold promised to pass on is not dropped when the call ends.** The original design drops any turn not yet typed when the call ends. Harold keeps trying to type it until 2 minutes after it was first held ([A turn waiting for a busy chief](#a-turn-waiting-for-a-busy-chief)). The reason: Harold has said `I'll pass it on when it's free.`, and ending the call must not break that promise. Only Harold stopping drops it.

## The end-of-speech rule

Harold finds the end of your speech from the samples, with no model:

- It splits the audio into 20 ms frames and measures each frame's loudness (root mean square).
- The first half second of a call, 25 frames not played over, gives the room's background level. The level is never taken below 30.0 on the 16-bit scale, so a silent room gives a threshold of 90.0, not 0.
- Speech starts when frames stay above 3 times the background for 0.2 seconds (10 frames).
- Speech ends when frames stay below it for 1.2 seconds (60 frames).
- A turn is cut at 60 seconds.

A turn's audio runs from the first of the 10 loud frames that started speech to the last loud frame; the closing 1.2 seconds of silence is not kept. Speech followed by 1.0 second of silence and more speech stays one turn. The 1.2 seconds is the delay this adds to each turn.

The background level, the factor of 3 and the 1.2 seconds are fixed values in `harold/src/voice_input/speech.rs`. How well they fit a real room is not known until real use, and they may need tuning.

## Not hearing the chief

While Harold plays an answer, a phrase or the tick, and for 0.3 seconds after, it takes no sample as speech. `ffmpeg` keeps running, and the call discards those samples. Muted frames count as silence and are not kept. While Harold is transcribing, waiting for the chief or speaking, it also discards samples and forgets any speech in progress, keeping the background level.

The microphone capture has no echo cancellation, so this muting is the guard. With headphones nothing changes.

## Settings

Add a `[voice_input]` section to `local.toml`. The template (`harold/config/local.template.toml`) carries it commented out:

```toml
# Calls with the chief at the desk (tmx-agent-dash: c). Off when absent. The worker loads the Whisper model named by --model, with command as its Python.
# [voice_input]
# command = "/Users/you/Dev/xn/mlx-audio/.venv/bin/python"
# args = ["-m", "mlx_audio.stt.generate", "--model", "mlx-community/whisper-large-v3-turbo"]
```

| Key | Meaning |
|---|---|
| `command` | The Python that runs the voice worker. It must have `mlx_audio` installed. It must not be empty. |
| `args` | The word after `--model` names the Whisper model. The worker uses only that value; the rest of `args` is not passed on. Without `--model` and a value, Harold refuses to start and `harold --check-config` fails. |
| `model_revision` | The commit of the Whisper model that Harold downloads, checks and loads: 40 lowercase hex characters. Default `a4aaeec0636e6fef84abdcbe3544cb2bf7e9f6fb` (committed 2026-04-12). |
| `keep_worker_minutes` | How long the voice worker is kept after no web app page is on screen and no call is on. Default `10`. |
| `language` | The Whisper language code every turn is transcribed in. Default `en`. Whisper is told the language, so it does not guess it from a clip of two or three seconds, which it can get wrong. A code outside Harold's short list of Whisper codes (`en` among them) is refused at start-up. `harold.call_transcribe` records the language the worker used. |

`[tts] model_revision` is the same for Kokoro: default `a71e4d38b236d968966a2002c4c895dbd12b1c3c` (committed 2025-12-02). Both defaults are the commits already in this Mac's cache, so pinning changes nothing here. The defaults are in `harold/config/default.toml` (`[tts]`) and in `harold/src/settings.rs`; `[voice_input]` is set in `local.toml`, so its defaults come from the code.

The worker takes Kokoro's model, voice and language from `[tts] args`: `--model` (default `mlx-community/Kokoro-82M-bf16`), `--voice` (default `af_heart`) and `--lang_code` (default `a`), so a call sounds like the desk.

A call's answers are always in English: `language = "en"` for Whisper, and rule 7 of the [call answerer](call-answerer.md)'s rules. A user who speaks another language sets both `[voice_input] language` and `[tts] --lang_code`, and changes rule 7 by spec; the rules are a constant, not a setting.

With no `[voice_input]` section, the button shows `[c] 📞 call: not set up`, and nothing runs. Harold reads the configuration only at startup.

The fixed limits are not settings: the 1.2-second silence, the 60-second turn, the 3-second tick, the 90-second wait, the 2-minute retry, the 30-second worker answer, the 10-second hold after talk now and the 10-minute idle end.

### The microphone permission

macOS charges the microphone permission to the program that runs `ffmpeg`, which is `harold`, signed with a stable identity, so the grant should survive updates. macOS should ask the first time a call starts. If it does not (not known for a program that launchd started), add `harold` under System Settings > Privacy & Security > Microphone. Until it is allowed, `ffmpeg` produces no audio or exits, and the call ends with `microphone_failed`.

## The voice worker

The voice worker keeps the speech models loaded for the length of a call, so a turn does not wait for them to load.

**What it is.** A Python program (`harold/src/voice_worker.py`) compiled into `harold` as text. When a call starts, Harold writes it to the call's folder as `voice_worker.py` (mode 0600, in a folder of mode 0700) and runs it with `[voice_input]`'s Python, as `python -I voice_worker.py` with the model, voice and language arguments, and for a model Harold manages the folder of its pinned commit (`--stt-snapshot`, `--stt-commit`, `--tts-snapshot`, `--tts-commit`). `HF_HUB_OFFLINE=1` is set, so a missing model fails to load instead of downloading. The worker never downloads: Harold downloads a missing speech model at start-up ([Speech models](#speech-models)).

**What it loads.** Two models, once, exactly the pinned commit of each:

- Whisper large-v3-turbo (the model named in `[voice_input]`), through `mlx_audio.stt.utils.load_model`;
- Kokoro, through `mlx_audio.tts.utils.load_model`.

For each, `mlx_audio` is given the snapshot folder `<cache>/models--<org>--<name>/snapshots/<commit>` as a path, which it reads the model's name from when the cache folder is called `hub` (the default cache is). In a cache folder not called `hub`, the worker passes the name instead, under `HF_HUB_OFFLINE=1`, and first checks that the cache's `refs/main` names the pinned commit. The ready line says which applied (`stt_load` and `tts_load`: `folder` or `name`); the `mlx_audio` of this Mac's `[voice_input]` Python takes the folder (checked in its source: both `load_model` functions take a `Path` under `hub`).

It then says a throwaway line, so a missing voice fails at load and not on the first answer. It imports only what the installed `mlx_audio` already brings, plus `numpy` and `soundfile`.

**The protocol.** One JSON object per line, on standard input and standard output. The worker redirects its other output to standard error, which Harold discards.

| Direction | Line | Meaning |
|---|---|---|
| worker to Harold | `{"ready": true, "load_ms": <n>}` | Both models are loaded, in `load_ms` milliseconds. `{"ready": false, "cause": "<cause>", "model": "<model>"}` means loading failed: `model_missing` (not in the cache), `import_failed`, `voice_missing` (Kokoro's voice from `[tts] args` did not load) or `load_failed` (anything else), with the model it was loading when that is known. It must come within 120 seconds, or the load counts as `failed_to_load`. |
| Harold to worker | `{"op": "transcribe", "wav": <path>}` | Answered with `{"ok": true, "text": ..., "no_speech_prob": ...}` |
| Harold to worker | `{"op": "say", "text": ..., "wav": <path>}` | Answered with `{"ok": true}` once the file is written |

Any answer with `"ok"` other than true means that one request failed. The worker stays up, the turn is dropped with result `failed`, and a line that was to be spoken goes through the `[tts]` command instead. A worker answers one request at a time and must answer each within 30 seconds, or Harold kills it.

**When it starts and stops.** The desk's calls start it with the call, and it stops when the call ends: Harold closes its input, waits up to 5 seconds for it to exit, then kills it.

The phone's calls keep it ahead: while a page of the web app is on screen (the push skip's state), Harold loads the worker once both speech models are checked, and a phone call takes it if it is loaded or loading, so the call rings one cycle and the first turn is transcribed in about half a second. The call gives the worker back when it ends. The worker stops `[voice_input] keep_worker_minutes` (10) minutes after no page is on screen and no call is on. After a failed early load Harold does not start another until a call is tapped or 10 minutes have passed. Outside a call, `harold speak` works as before, one `[tts]` command per line.

The log says how it went: `voice worker ready` with `load_ms` and `started_by` (`page` for one loaded for a page, `call` for one a call started) and `voice worker stopped` with `reason` `no_page`, `call_ended` or `exited`. A worker started by a call under the page rule means the page was not seen in time.

**Phrases.** Harold's own phrases are made once by the worker and kept in `$TMPDIR/harold-voice-phrases/` as `<first 16 hex of sha256(voice + "\n" + phrase)>.wav`, so a later call plays them without waiting. Two phrases always go through the `[tts]` command, never the worker, because the worker is the thing that failed: `The voice stopped. One moment.` and `The voice failed. Call ended.`. On the phone, `The speech model isn't on the Mac yet.` is said by the phone's own voice, not the worker. `There's no chief running.` at the start comes before any worker exists, so it plays from the phrase folder when the file is there, and otherwise through `[tts]`.

**Memory.** About 3.1 GB while the worker is loaded (3.5 GB at its peak): Whisper large-v3-turbo about 1.6 GB and Kokoro about 0.35 GB of files, plus what the models use. The desk's worker is loaded only around calls; the phone's is loaded while a web app page is on screen and for the keep time after.

### If the worker dies

Harold notices when the worker's output ends, when it does not answer a request within 30 seconds, or when it does not load within 120 seconds. Each is logged as `voice worker exited` with `reason` `exited`, `timed_out` or `failed_to_load`. A worker that fails to load also gives `cause` (`model_missing`, `import_failed`, `voice_missing` or `load_failed`) and, for `model_missing`, the `model`. A worker that dies before its ready line has `cause = "load_failed"`.

- **First death, after it had loaded.** The turn in hand is dropped, with result `failed`. Harold starts the worker again and says `The voice stopped. One moment.` through `[tts]`, then listens again.
- **Second death in the same call, a worker that never loaded (`failed_to_load`), or a restart that cannot start.** Harold says `The voice failed. Call ended.` and ends the call with `voice_failed`.

A `model_missing` is different: Harold checks the model again and downloads it if it is one it manages ([Speech models](#speech-models)), and the call goes on connecting and starts the worker once more, once. If the model is still missing, the call ends as described there.

Before the ready line, the worker's output may hold a line that is not JSON, such as a library's stray print. Harold skips such lines. The first JSON line must be `{"ready": true}`, or the load fails. After the ready line, a line that is not JSON ends the worker.

### Why did the voice fail to load?

Look at the `cause` on `voice worker exited` first, and at `speech model not downloaded` (its `reason` says why a model is not on the Mac). Harold discards the worker's standard error, so the log shows the cause and not the traceback. To see the error, run the worker by hand the way Harold runs it. Harold writes the script into the call's folder, runs it from there with `HF_HUB_OFFLINE=1`, and passes the values from `[voice_input]` and `[tts] args`:

```sh
cd "$(mktemp -d)"
cp <harold repo>/harold/src/voice_worker.py .
HF_HUB_OFFLINE=1 <[voice_input] command> -I voice_worker.py \
  --stt-model <Whisper model> --tts-model <Kokoro model> \
  --voice <voice> --lang-code <lang code> < /dev/null
# For a model Harold manages, also: --stt-snapshot <cache>/models--<org>--<name>/snapshots/<commit> --stt-commit <commit>
```

`<Whisper model>` is the word after `--model` in `[voice_input] args`. The Kokoro model, voice and language come from `[tts] args`, with the defaults in [Settings](#settings). The worker prints `{"ready": true, "load_ms": ...}` when it loads, or `{"ready": false, "cause": ...}` when it does not, and then exits, because its input is empty. Either way of failing prints its traceback to standard error, where you see it: an import error, such as a Python without `mlx_audio`, and a model or voice that fails to load. Harold still discards standard error, so no traceback reaches `harold.log`.

## Speech models

Harold downloads a missing speech model at start-up, pinned to a commit and checked file by file ([ADR 0011](../../adr/0011-harold-downloads-missing-speech-models-at-start-up.md)). The voice worker never downloads: it loads the checked models with `HF_HUB_OFFLINE=1`. On 20 October the first phone call ended with `voice worker exited reason="failed_to_load"` and "The voice failed.", because the Whisper model was not downloaded and nothing said so; this section is the answer to that.

**The two models.** Harold downloads only `mlx-community/whisper-large-v3-turbo` (1,613,979,758 bytes) and `mlx-community/Kokoro-82M-bf16` (389,410,807 bytes), a list held in `harold/src/voice_input/models.rs`, each at the commit set in `[voice_input] model_revision` and `[tts] model_revision`.

- A revision must be 40 lowercase hex characters, and a model `<org>/<name>` in letters, digits, `.`, `_` and `-`. Otherwise Harold logs `speech model setting refused` and downloads nothing.
- A configured model outside the list is never downloaded. If the worker cannot find it, the call ends with the notice below, which names the model and the command.

**The check, at start-up.** The cache is `$HF_HUB_CACHE`, else `$HF_HOME/hub`, else `~/.cache/huggingface/hub`. Harold looks for its record of the last check of each model in `speech-models/<org>--<name>@<commit>.json` in its data folder (beside the store). The record lists each file's path, size and sha256.

- **With a record,** Harold checks that every listed file is in `snapshots/<commit>/` with its size. This is a folder read, every start-up.
- **Without a record,** as on this Mac after the install, `voice_download.py` runs in its `--check` mode: it checks the cached files against the commit's file list, writes the record, and downloads nothing. It contacts `huggingface.co` for the list. It downloads only if something is missing or wrong.
- **A missing or short file** starts the download.

**The script.** Harold writes `voice_download.py` into `speech-models/` (folder 0700, file 0600) and runs it as `<[voice_input] command> -I voice_download.py <org>/<name> <commit> <record path> [--check]`, with the environment cleared to `PATH`, `HOME`, `HF_HOME` and `HF_HUB_CACHE`. The model and the commit come only from the checked settings, as arguments. Inside it:

- No token and no telemetry: it sets `HF_HUB_DISABLE_IMPLICIT_TOKEN=1` and `HF_HUB_DISABLE_TELEMETRY=1` before importing `huggingface_hub`, and passes `token=False` to `HfApi`, `model_info` and `snapshot_download`. Without that the library would send the token it finds in `~/.cache/huggingface/token`. It never sets `HF_ENDPOINT` or `HF_HUB_ENABLE_HF_TRANSFER`, and never passes `local_dir`. A test reads the script's source for each.
- It refuses unless `model_info(<model>, revision=<commit>, files_metadata=True).sha` is the pinned commit, prints `{"total": <bytes>}`, and calls `snapshot_download`, which must return a path ending in `snapshots/<commit>`.
- It then checks every file the commit lists: it is there, it has its size, and for each large file the sha256 of its bytes matches the listed `lfs.sha256`. The library itself checks only byte counts, and returns a cached folder without checking at all. It writes the record only when every file passes, and prints `{"done": true, "bytes": <n>}`, or `{"done": false, "reason": "..."}` (`network`, `not_found`, `disk_full`, `check_failed`, `not_allowed`, `failed`; with `--check` also `missing`).

**What leaves the Mac.** `huggingface.co`, for the file list and each file, and the download hosts Hugging Face redirects each large file to, which the server chooses (its CDN and its Xet storage, expected under `hf.co`), through `hf_xet`, installed with `huggingface_hub`. It sends the repository name, the commit, the file paths and a user agent naming the `huggingface_hub` and Python versions. It sends no token, no user data, no call content and no file of Harold's. It runs only at start-up on a missing or unchecked model, and when a call is tapped after a failed download.

**The download.** In the background, one model at a time, Whisper first.

- Before it starts, Harold checks the disk: with less free space than the model plus 1 GB, it does not start, and logs `speech model not downloaded` with `reason = disk_full`.
- A run that has not finished in 2 hours is stopped as `failed`; its process group is killed, as the call answerer's runs are. A Harold that stops during a download stops it too. The next start-up finds the model still missing and starts again, and `huggingface_hub` resumes partial files.
- **A metered network.** When the default route's gateway is in `172.20.10.0/28`, the iPhone Personal Hotspot's own range, the start-up download waits: Harold logs `speech model download waiting` (`reason = metered`) once and looks again every 10 minutes. A call that is tapped starts the download at once, on the hotspot (1.6 to 2 GB of phone data), by the user's choice. macOS's own "expensive network" flag is in its Network framework, which Harold cannot reach without a new dependency, so a hotspot from another phone, or a metered Wi-Fi, is not noticed.

**Log lines.**

| Level | Message | Fields |
|---|---|---|
| info | `speech model download started` | `model`, `total_bytes` |
| info | `speech model downloaded` | `model`, `bytes`, `ms` |
| info | `speech model download waiting` | `reason` (`metered`); once, then every 10 minutes silently |
| warn | `speech model not downloaded` | `model`, `reason`: `network`, `not_found`, `disk_full`, `check_failed`, `not_allowed` or `failed` |
| warn | `speech model setting refused` | none |
| warn | `call ended for a speech model` | `model`, `reason` |

No progress line is logged: the phone shows progress. `ms` per GB of `speech model downloaded` is the download's speed.

**Calls during a download.** The call stays *connecting*, the ringback goes on, whole cycle after cycle, and the hint reads `Getting the speech model: 45%` (Harold adds up the bytes under the model's `blobs/` folder every 10 s, partial files included). When the download and its check finish the worker loads and the call goes on to listening. If the download fails, the ringing stops, Harold says `The speech model isn't on the Mac yet.` (on the phone in its own voice), and the call ends with `voice_failed`. The phone's notice (`#call-notice`) reads: *The speech model mlx-community/whisper-large-v3-turbo didn't download (<reason>). To get it, run on the Mac: `<[voice_input] command> -c "from huggingface_hub import snapshot_download; snapshot_download('<model>', revision='<commit>', token=False)"`, then call again.* A call tapped after a failed download starts the download again, unless the reason was `disk_full` or `not_allowed`: those end the call at once with the notice. A worker that answers `model_missing` makes Harold check and download as at start-up, and the call starts the worker once more.

**Running the script by hand,** for a small public repository at a pinned commit and a scratch cache:

```sh
env -i PATH=/usr/bin:/bin HOME=$(mktemp -d) HF_HUB_CACHE=$(mktemp -d) \
  <[voice_input] command> -I harold/src/voice_download.py <org>/<name> <40-hex commit> /tmp/record.json
```

## Privacy

- **On the Mac.** Audio and transcripts stay on the Mac. The models and commands are local. The one thing the voice sends out is the speech-model download, which carries no call content ([Speech models](#speech-models)).
- **Audio.** Each turn's audio lives only until its transcript returns, and is deleted even on failure. Samples between turns are kept only in memory. The voice worker keeps no audio or text after answering. The audio of the lines it made for a call is deleted after it plays, and the call's folder is deleted when the call ends. The phone's early worker runs from its own folder (`worker/` beside the call folders), which stays while it is kept. If Harold is killed during a call, its folder stays in `$TMPDIR/harold-voice-input/` until Harold next starts: at start-up Harold removes every `call-*` folder left there, and nothing else in that folder.
- **Phrases kept.** The phrase folder holds only Harold's own fixed phrases, never your words.
- **Transcripts.** Harold does not store the transcript as an event, and never writes it to `harold.log`. It is shown in tmx-agent-dash while the call lasts, through the `Call` answer's `last_transcript`, and cleared when the call ends.
- **Telemetry.** harold-telemetry never exports text. The typed line becomes a turn's prompt like any typed prompt.
- **A call turn's trace.** One trace per turn, from the start of speech to the answer's first sound, with spans for the speech, Whisper, Kokoro and the playback: see [the call turn's span table](../telemetry/README.md#turns).
- **The chief's session.** The prompt typed into the chief's pane is in the chief's own session. That is how the chief hears it.

### Log lines

Messages are fixed text, with values in fields, as harold-telemetry's test `every_log_message_is_fixed_text` requires. These are the lines the call writes to `harold.log`:

| Level | Message | Fields |
|---|---|---|
| info | `call started` | none |
| info | `call ended` | `reason`: `stopped`, `bye`, `no_chief`, `voice_failed`, `microphone_failed`, `shutdown`, `idle` or `voice_muted`. A call that listened also carries whole numbers: `heard_ms` (sound read, in ms), `first_heard_ms` (ms from the start to the first block), `background` (the calibrated level, left out when calibration did not finish), `loudest` (the loudest unmuted frame), `loud_frames` (unmuted frames over 3 times the background) and `speech_cut` (turns cut) |
| info | `call not started` | `reason`: `voice_muted` |
| info | `spoken prompt typed` | `pane_id`, `words` (a count) |
| info | `spoken prompt held` | `reason`: why the chief was busy, one of `agent_busy`, `agent_state_unknown`, `input_box_holds_text`, `input_box_not_found`, `input_box_unreadable`, `several_agents_in_folder` or `pane_list_failing`; logged when a call first holds a turn |
| info | `spoken prompt dropped` | `reason`: `nothing_heard`, `no_chief`, `chief_busy` or `failed`; `busy_reason` (the last reason the chief was busy) with `chief_busy` |
| warn | `call audio refused` | `reason`: `full`, `malformed` or `too_large`; once per reason per call, for the phone's open call only |
| info | `voice worker ready` | `load_ms` (how long the worker took to load), `started_by`: `page` (loaded for a web app page) or `call` |
| info | `voice worker stopped` | `reason`: `no_page` (10 minutes after no page and no call), `call_ended` or `exited` |
| warn | `voice worker exited` | `reason`: `exited`, `timed_out` or `failed_to_load`; for a worker that never loaded also `cause` (`model_missing`, `import_failed`, `voice_missing` or `load_failed`), and for `model_missing` the `model` |
| warn | `display not kept awake` | `error` |
| warn | `microphone not opened` | `error` |
| warn | `call folder not made` | `error` |
| warn | `call folder not removed` | `error`; at the end of a call, or for a leftover folder at start-up; not logged when the folder is already gone |
| warn | `turn not written` | `error` |
| warn | `phrase not kept` | `error` |
| warn | `call sound not played` | `error` |
| warn | `turn-end update not made` | none |

`call ended` is also logged for a call that ended before it started listening, without a `call started`.

harold-telemetry exports only fields on its allowed list. `pane_id`, `reason`, `busy_reason`, `heard_ms`, `first_heard_ms`, `background`, `loudest`, `loud_frames`, `speech_cut`, `cause`, `model`, `bytes`, `total_bytes`, `ms`, `load_ms` and `started_by` go out; each is a count, a level or a fixed word. `words` and `error` are not on the list, so they stay in `harold.log` only. No transcript and no spoken line is in any log line, event or exported record; a test with a canary transcript checks this.

## Latency of one turn

These are the steps from the end of your speech to the start of the chief's answer.

| Step | At the desk | Source |
|---|---|---|
| Silence that marks the end of speech | 1.2 s | the end-of-speech rule |
| Transcription, with Whisper already loaded | about 1 s for up to 15 s of speech | target, measured in real use; it was 5 s with a reload each turn |
| Typing into an idle chief pane | under 0.5 s | |
| The chief thinks | 13 s median, 26 s at the 90th percentile | the chief's 19 phone-started turns in harold-telemetry, to the turn's end, so an upper bound |
| Making the line into audio, with Kokoro already loaded | under 1 s for a line of up to 300 characters | target, measured in real use; it was about 5 s with a reload (4 voice notes, 4.6 to 5.1 s) |
| **Total** | **about 17 s median, of which about 4 s is Harold's** | was about 25 s, with 12 s of Harold's |

The first turn of a call used to wait for the worker to load, 6 to 8.5 s after the call started. The phone's worker is loaded before the call (while a page is on screen), and a call rings until the worker is ready and picks up then. With the worker loaded, the first turn is transcribed in about 0.5 s and the call answerer's answer, the turn's first sound, plays in about 5 s: the facts hold only live specs, about 3k tokens a turn instead of 10k. About 1.4 s of every answer is Claude Code starting before it calls the model. The table above is from the desk's design; `first_sound_ms` and `answer_ms` on `harold.call_turn` ([the telemetry reference](../telemetry/README.md)) are the real ones.

## The `Call` RPC

tmx-agent-dash controls the call over one RPC in `harold-api/proto/harold.proto`: `Call(CallRequest { action }) returns (CallResponse { state, chief_pane_id, last_transcript, last_result })`.

| Field | Values |
|---|---|
| `action` | `START`, `STOP`, `TALK_NOW`, `STATUS` |
| `state` | `OFF`, `LISTENING`, `TRANSCRIBING`, `WAITING`, `SPEAKING`, `NOT_SET_UP`; a connecting call reads as `LISTENING` (the contract has no value for it) |
| `last_result` | `TYPED`, `NOTHING_HEARD`, `NO_CHIEF`, `CHIEF_BUSY`, `FAILED`, `VOICE_MUTED`; unspecified before any turn has ended in the call |

The dash shows `TRANSCRIBING` and `WAITING` both as `chief thinking`. It waits up to 8 seconds for the answer to `START`, `STOP` or `TALK_NOW`, longer than the 5 seconds Harold may take to close the voice worker when a call ends, and up to 2 seconds for a `STATUS` poll, which Harold answers at once. `chief_pane_id` is empty when no chief runs. `START` answers once the call listens or has ended. A state the dash does not know counts as not set up.

## The shared core and the web app

The conversation core is one design with two places for audio. It does four things: it finds the end of speech, takes turns, turns speech into text, and types the text into the chief's pane marked `🎙`. This page's call is the desk's: the microphone through `ffmpeg` and the speakers through `afplay`. away-web-app's call mode (its stage 1b) adds only the phone's side, recording and playing audio in the app and carrying it to and from the Mac, by supplying its own source and sink. The phone's side is built; [The phone's side](#the-phones-side) describes it.

The core, in `harold/src/voice_input.rs`, is internal Rust, not an API. A call is built from three traits.

**`AudioSource`** is where a call's audio comes from.

- `next_block(&mut self)` resolves to the next block of 16 kHz mono samples as `i16` values, of any length, an empty block included. The byte order of whatever carries them is the source's business.
- `None` means the source has ended (`ffmpeg` exited, the phone hung up), and every later read returns `None`. The core then ends the call with `microphone_failed`.
- It is cancel-safe: dropping the future before it resolves loses no samples and shifts none.

**`AudioSink`** is where a call's sound goes. The core plays one file at a time and knows when each ends.

- `play(&self, wav)` plays the 16-bit PCM WAV at `wav`, at any sample rate. Only one `play` runs at a time. It resolves `Ok` when the sound has ended or `stop` ended it, and `Err` when it could not be played. The core then says the line through `[tts]` instead.
- `stop(&self)` stops what `play` is playing now and returns at once; the sound stops promptly. It does nothing when nothing plays, and does not affect a later `play`.

**`CallPorts`** bundles everything a call reads and does outside the core. The daemon's is `MacPorts`; the phone's is `PhonePorts`, built on the desk's `MacPorts`. It names a `Source`, a `Sink`, a `Worker`, the chief's `Panes`, and an `Awake` value held for the call (`caffeinate` at the desk; dropping it lets the display sleep). Its methods are:

| Method | Does |
|---|---|
| `open_source`, `sink` | Open the call's source; give the one sink every call plays through. |
| `start_worker(call_dir)` | Start the voice worker; loading goes on in the background. |
| `panes`, `chief_folder` | The chief's pane and the folder (`[notify] chief`). |
| `speak_with_tts`, `stop_desk_line` | Say a line with the `[tts]` command and resolve when it ends; stop such a line. |
| `turn_end_update(prompt)` | The desk's turn-end update for a turn that ended without `harold speak`. |
| `calls_dir`, `phrases_dir`, `voice_name` | Where call folders and kept phrases live, and the Kokoro voice in each phrase's file name. |
| `voice_muted` | Whether Harold's voice is muted now. |
| `keep_awake` | Hold the display awake for the call. |

`CallControl` runs at most one call. `start`, `stop`, `talk_now` and `status` are what the `Call` RPC invokes. `say_blocking` plays a line in the open call from a blocking thread, which is how a `harold speak` line from a pane on `web_app` reaches the phone's call and how `harold speak` and other agents' turn-end updates reach the desk's call (under `machine_audio_output`). `CallControl::say`, its async twin, is compiled only in tests (`#[cfg(test)]`). The phone's call does not need it: every line it says arrives on a blocking thread (the projector's `spawn_blocking`) and goes through `say_blocking`, as at the desk.

Tests use a fake source, sink, worker and pane; nothing in them touches the microphone, the speakers or a model.

## The phone's side

The web app's call (its stage 1b) runs on this core. [The web app reference](../away-app/README.md#calls-stage-1b) describes what you see and hear; this section is what the phone supplies in place of the desk's parts. The phone's calls are built only when `[web_app]` is set, on the same `MacPorts`, so the transcription worker, the chief's pane and the voice name are the desk's.

**The source and the sink.**

- **The source** is `PhoneSource`. The app posts quarter-second pieces of 16 kHz mono audio (`POST /call/audio`), and the source hands each to the core with the time the app recorded it. A piece with an odd number of bytes, more than one second of audio, or no time is refused and the call goes on; one that arrives with no call open, or carries no id or another call's id, is refused with `409`. Each piece carries the id `POST /call` answered: a new one for each call, so a piece from an earlier call never reaches a later one. The source ends, and the call with it, when the phone posts nothing for 15 seconds while nothing plays. Three seconds after Harold answers `POST /call`, the page posts what the microphone gave to `POST /call/check` (the audio context's state, the first audio track's state and `muted`, the pieces the worklet made, posted and dropped by reason, the posts that failed, and the blocks the worklet counted with no input). Harold logs it as `app call check` (info), or as `app call microphone failed` (WARN) when the page found a `problem` (`no_sound`: no piece made) or a `step` (`microphone`, `context`, `worklet` or `call`, with the browser's `error_name`) that threw. With no sound the page then ends the call with `DELETE /call?why=no_sound`, which ends it as `microphone_failed`; the 15 seconds stays as the fallback for a page that can post nothing. The page also resumes its audio context after the microphone starts, and each time the context leaves `running`.
- **The sink** is `PhoneSink`. `play` does not play anything on the Mac. It offers the WAV under a new id; the app fetches it (`GET /call/answer/<id>`), plays it and reports it (`POST /call/played`). `play` resolves when the phone reports, or `stop` (Talk now) takes the sound back. A sound the phone fetched and never reported counts as played after its length plus 10 seconds. One it never fetched fails, so the core says the line through `[tts]`, which on the phone is its own voice. Nothing is offered for more than 2 minutes.

**Muting by capture time.** The core ignores a block recorded while a sound played or within 0.3 seconds after. At the desk a block's capture time is when it was read. On the phone a piece may be delayed by the network, so the source stamps each with when the app recorded it (the call's start plus the piece's `at`), and the core compares that with the sound's end. The app counts `at` from just before it sends `POST /call`, and the source counts from when that request arrived, before a desk call is ended, so a piece's stamp is late by no more than the request's time on the network. The app also stops sending during that time, so the core's rule is the second guard.

**The phone's own voice.** What Harold says only through `[tts]` at the desk, `The voice stopped. One moment.` and `The voice failed. Call ended.`, and any sound the phone never fetched, goes to the phone as a line, not a sound. The app says it with the phone's `speechSynthesis`, and reports it said. The core waits for that report, or for 2 seconds plus 80 milliseconds for each character, whichever comes first. Harold's voice mute silences the Mac's speakers, not the phone, so it does not end or refuse a phone's call. Until the call opens (its worker has started), the sink and the phone's own voice offer nothing and return at once: a call that ends before it opens answers `POST /call` with `409` and the line, which the phone shows, so `There's no chief running.` and a voice failure at the start are shown, not heard.

**`say_in_open_call`.** `harold speak` and other agents' turn-end updates reach an open call through `say_in_open_call`. It asks the desk's call only; the phone's call is reached through `say_in_phone_call`, under the `web_app` list, so a line held off that list is never heard in it, and no turn-end update goes into the phone's call. With the screen unlocked and only the phone's call open, such a line is spoken with the `[tts]` command on the Mac. Before the screen lock decides where the chief's spoken line goes, the projector asks only the phone's call (`say_in_phone_call`). With no phone call open, the line goes by the lock's rules, as at the desk: with the screen unlocked it is said through `say_in_open_call`, into the desk's call if one is open; with the screen locked it goes to the phone, as [The display and the lock screen](#the-display-and-the-lock-screen) describes. A line the chief says in the phone's call plays on the phone and is added to the thread as text, with no push and no iMessage. Starting a call on the phone ends a call open at the desk first. Nothing stops the dash from starting a desk call while the phone's call is open.

**A newer screen.** A `POST /call` while the phone's call is open takes it over: the call and its worker go on, the call gets a new id, and its pieces are stamped from that request's arrival. The older screen's pieces are refused, and those it posted that still wait are not read. The call forgets the turn being spoken (`CallControl::forget_turn`, which resets the speech detector and keeps the background level), and the source forgets it again just before the newer screen's first piece, so whatever the older screen was saying is dropped, not typed.

**Logs.** A phone's call writes `app call started` and `app call ended` with `reason`, and its turns write the lines above. No transcript, audio or line text is in any of them.

## Testing a call end to end

`scripts/call_test.py` plays the phone through a whole call and prints one line per step with its time in milliseconds. It exits 0 when every step passes. Otherwise it names the failing step and exits 1.

```
python3 scripts/call_test.py [--keep] [--log-level debug]
```

It needs the installed Harold's config (`~/bin/harold`), `tmux`, `say` and `claude`, and it uses nothing else outside the Python standard library. `--harold <binary>` (with `--default-config <default.toml>`) tests another build, such as one of main and one of a branch, to compare them; `--wait-for-page-load` waits for a Harold that loads the voice worker for an open page. Like the phone, the script says a page is on screen (`POST /page/seen`) from the start. The test Harold is never given a speech model to fetch: the script writes it a record of the models already in the cache, so it has nothing to check or download, and the cache is only read. It touches neither the chief's pane, nor the installed Harold's calls, nor your thread.

**What it starts.** All of it lives in a scratch folder, which the script removes at the end. A failed run keeps it and prints it; `--keep` keeps it after a pass. `harold.log` in it is the test Harold's log.

- **A test Harold**, `~/bin/harold/harold` with `HAROLD_CONFIG_DIR` set to the scratch config. The config copies `[voice_input]` and `[tts]` from the installed `local.toml` and sets free loopback ports, `allowed_login = "call-test"`, a scratch store and `[telemetry] export = false`. `[notify]` names the scratch chief folder, the session `harold-call-test` and `web_app` for that folder, and empties `imessage` and `machine_audio_output`. It sets no timed prompts, and `[chat_db]` points at an empty scratch file, so the test Harold never reads your Messages. Harold refuses to start without an iMessage recipient, so `[imessage]` holds a placeholder that names nobody (`call-test@harold.invalid`). The test Harold also gets its own `TMPDIR`, since a starting Harold sweeps leftover `call-*` folders.
- **A scratch tmux server** (`TMUX_TMPDIR` in the scratch folder, `TMUX` unset), so the test Harold sees no other pane. It holds one session, `harold-call-test`, whose pane runs `claude --model haiku` in the scratch chief folder, with `HAROLD_CONFIG_DIR` in its environment. The folder's `CLAUDE.md` tells it to answer each line starting with 🎙 by running `harold speak --src-id <its pane> "Call test answer."`, and `.claude/settings.json` allows only that command. It also sets `disableAllHooks`: a Stop hook set up for the installed Harold reports to that Harold's address, not to `HAROLD_CONFIG_DIR`'s, so a turn here would reach the installed Harold otherwise. Claude Code asks once whether it trusts the scratch folder and the script says yes; that is recorded in your `~/.claude.json`, as it is for any folder. The pane gets one first turn (`Hello`), because a fresh Claude Code shows a suggestion in its input box, which Harold reads as text and then holds the call's turn.

**The phone.** Every request carries `Tailscale-User-Login: call-test`, the loopback Host and, on posts, `Origin`. The script follows `GET /updates` and reads `#call`'s `data-harold-state`, `data-harold-sound` and the "You said" line. It makes the question with `say` (16 kHz mono) and posts it to `/call/audio` as `call.js` does: pieces of 4,000 samples stamped with `at`, sent in real time, with 1 s of silence before and 2 s after.

**The steps.** Each has a time limit. The times of the steps after the question count from the end of the spoken question.

1. `POST /call` answers 200, and `tap_to_listening` is the time until `#call` shows `listening`: at least 3 s of ringing, then the pick-up. The script fetches and reports each sound of the ringing as the phone does, prints them in order (ringback cycles, then the pick-up; `none` on a main that rings nothing when the worker is loaded), and prints whether any `One moment.` line appeared in `#call`.
2. The question is posted.
3. The turn is cut (`#call` leaves `listening`).
4. The "You said" line holds at least 80% of the question's words.
5. The log has `spoken prompt typed` and the stand-in's pane shows the 🎙 line.
6. `data-harold-sound` names an answer.
7. `GET /call/answer/<id>` returns WAV audio; the line gives its length. For each turn the script prints `first_sound_ms` (the first sound with words: the answer itself for an answered turn) and `answer_ms`, as the phone sees them from the end of the speech; Harold cuts the turn 1.2 s later.
8. `POST /call/played`, then `DELETE /call`, and the log's `call ended reason="stopped"`.

A last check reads the test Harold's log: no `iMessage listener started`, and every `spoken prompt typed` names the stand-in's pane.

**Teardown, always:** the call ends, the tmux server is killed and the test Harold stops.
