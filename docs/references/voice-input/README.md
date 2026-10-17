# Voice input: calls with the chief

A call lets you talk to the chief of pods at the Mac instead of typing. You press one key in tmx-agent-dash. Harold listens through the microphone, turns what you say into text, types the text into the chief's pane, and plays the chief's spoken answer. Harold then listens again, until you end the call.

The chief is the agent running in the folder named by `[notify] away_messages_only_from`.

## Problem

Before calls, the chief could talk to you but you could not talk back. The chief answers aloud with `harold speak`, and Harold says the line with the local Kokoro voice. The only way to reach the chief was to type in its pane.

A call closes that loop. Harold never takes the chief's own voice for yours: it plays everything itself, so it knows exactly when a sound plays and does not listen then ([Not hearing the chief](#not-hearing-the-chief)).

## Do not send the chief a voice message from the phone

This reference covers the desk only. The same conversation on the phone is a later part of away-web-app, and until it exists, do not record a voice message to the chief from the phone. Harold drops it.

The reason: Messages stores a recorded message as an attachment, which reaches Harold as a row with no text and only an attachment placeholder character. Harold sends its own voice notes as attachments too, and those come back to it as the same kind of row. It therefore treats every attachment-only row as the echo of its own voice note and ignores it (`is_attachment_only` in `harold/src/channels/imessage.rs`). Your message looks identical, so it is ignored with them. Type a text message from the phone instead.

## The call

### Starting and ending

In tmx-agent-dash, select the chief's row and press `c`. The chief's row is the one whose pane Harold names in its answer to the dashboard, so the button follows the chief if it restarts in another pane.

| Key | On the chief's row | On any other row |
|---|---|---|
| `c` | Starts a call when none is on and Harold's voice is not muted. Ends the call during one. | Nothing |
| `t` | Talk now, while the chief speaks ([Talk now](#talk-now)). | Nothing |

The command palette (opened with `/`) also lists `Call: start` when a call can start and `Call: end` during a call, from any row. Typing `call` in the palette finds them. Talk now has no palette entry. The dashboard captures no mouse events, so the buttons are pressed only with their keys. The footer lists `c call` and `t talk now`.

When a call starts, Harold:

1. stops any line it is saying at the desk;
2. checks that an agent runs in the chief's folder (if none does, it says `There's no chief running.` and the call ends);
3. starts `caffeinate -d -w <Harold's pid>`, which keeps the display awake for the length of the call, and exits by itself when Harold does;
4. starts `ffmpeg` with avfoundation on the default microphone, reading 16 kHz mono samples;
5. starts the voice worker, which loads while you say the first turn ([The voice worker](#the-voice-worker)).

The button then shows `listening`.

### One turn

1. You speak. Harold finds the end of your speech ([The end-of-speech rule](#the-end-of-speech-rule)) and writes the turn's audio to a WAV file in a new folder under `$TMPDIR/harold-voice-input/`.
2. The voice worker transcribes the file with Whisper large-v3-turbo. Harold deletes the turn's folder as soon as the answer returns, also when transcription fails.
3. Harold drops the turn or types it:
   - **Dropped** when the transcript is empty, or when Whisper's no-speech probability is above 0.6. Harold says `Sorry, I didn't catch that.` and listens again. The probability is the lowest over Whisper's segments, and 1.0 when there are none.
   - **Typed** otherwise. Harold types `🎙 <transcript>` and Enter into the chief's pane, with the rules timed prompts use: the pane must be idle and its input box empty. It takes the same per-pane lock as other writers, so a spoken line never interleaves with an alert or a timed prompt. Like a phone reply, it is not held back by the one-minute "typed recently" hold, because you are speaking now. The turn is recorded in harold-telemetry with `started_by = voice`.
4. If the chief's answer has not started 3 seconds after the turn is typed, Harold plays a soft tick every 3 seconds, so the call is not silent. The button shows `chief thinking`.
5. The chief answers with `harold speak`. During a call, the voice worker makes the line into audio with Kokoro and the call plays it with `afplay`. If the worker cannot make a line, or `afplay` cannot play it, Harold says it with the `[tts]` command instead. When the answer ends, Harold listens again.

The chief treats `🎙` lines as possibly misheard, and confirms anything consequential before acting. That is the chief's behavior, not Harold's: the transcript is typed without a confirmation step.

### When something gets in the way

| Case | What happens |
|---|---|
| The chief's pane is busy | Harold says `The chief is busy. I'll pass it on when it's free.` It tries to type the turn every second, for up to 2 minutes, and listens meanwhile. A second turn spoken meanwhile is joined to the waiting one with a space, so the chief gets one `🎙` line. After 2 minutes the turn is dropped, with no phrase. When the call ends before the turn is typed, the turn keeps waiting ([A turn waiting for a busy chief](#a-turn-waiting-for-a-busy-chief)). |
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
| `microphone_failed` | `ffmpeg` could not start, or ended during the call. |
| `shutdown` | Harold stopped while the call was open. |
| `idle` | For 10 minutes no turn was cut and no sound played. Harold says `Bye.` and ends the call ([Where this differs from the spec](#where-this-differs-from-the-spec)). |
| `voice_muted` | Harold's voice was muted during the call ([No call while Harold's voice is muted](#no-call-while-harolds-voice-is-muted)). |

When the voice or the microphone fails, Harold says `The voice failed. Call ended.` through the `[tts]` command. Ending the call stops `ffmpeg`, the voice worker and `caffeinate`, deletes the call's folder, and clears the transcript the dashboard shows. A turn still waiting for a busy chief is not dropped: it keeps its retry after the call ends ([A turn waiting for a busy chief](#a-turn-waiting-for-a-busy-chief)).

The 10-minute clock starts again on every turn cut and every sound played, the tick included. When it runs out while something is going on (you are speaking, a sound plays or waits, a turn waits for a busy chief, or Harold is transcribing or waiting for the chief), the clock starts again instead.

### Hanging up by voice

Harold checks each transcript before typing it. It takes the transcript's last words, ignoring case and punctuation. When they are `bye`, `bye bye`, `goodbye` or `bye chief`, the turn ends the call.

| You say | Harold types | The call |
|---|---|---|
| `Bye.` | Nothing. The chief hears nothing of the hang-up. | Says `Bye.` and ends. |
| `approve q19, bye` | `🎙 approve q19` | Says `Bye.` and ends. |
| `say bye to the old store first` | The whole line | Goes on. A `bye` inside the sentence does not hang up. |

The chief's answer to a last turn arrives as it would with no call open: said aloud at the desk.

A turn said before `bye` while the chief is busy keeps its ordinary 2-minute retry after the call has ended. Harold hands it to a background task that tries every second, holds no audio, and gives up after 2 minutes.

### A turn waiting for a busy chief

When Harold has said `The chief is busy. I'll pass it on when it's free.`, the turn reaches the chief even if the call ends first. You press `c`, say `bye`, or the call ends by itself (idle, a muted voice, or a failed voice), and Harold hands the waiting turn to the same background task a turn said before `bye` uses. The task tries every second, holds no audio, and logs `spoken prompt typed` once the chief is free. It gives up 2 minutes after the turn was first held, not 2 minutes after the call ended, and then logs `spoken prompt dropped` with `reason = chief_busy`. A turn said before `bye` that joins a waiting turn keeps that turn's 2 minutes too. When Harold itself stops, the waiting turn is dropped and nothing more runs.

### The button

The button sits at the right edge of the chief's work cell, so a call never changes the list's column widths. During a call the cell's text is the last thing typed, `🎙 <transcript>`, or else what became of the last turn (`nothing heard`, `chief busy`, `failed`).

The dashboard asks Harold for the call's state once when its stream opens, every 500 ms while a call is on, and every 5 seconds otherwise, so the button finds the chief's pane before any call. A poll that fails hides the button and shows no status line, so a dashboard talking to an older Harold without calls shows nothing. Only a failed `c` or `t` press shows a status.

| State | Button | Colour |
|---|---|---|
| No call | `[c] 📞 call` | ink on a dark key |
| Listening | `[c] ● listening · end call` | green |
| Transcribing, or waiting for the chief | `[c] … chief thinking · end call` | amber |
| The chief speaking | `[c] 📞 ▂▄▆▄▂ chief speaking · end call` and `[t] talk now` | amber, bars moving |
| `[voice_input]` absent | `[c] 📞 call: not set up` | muted; `c` does nothing |
| Harold's voice is muted | `[c] 📞 call: voice muted` | muted; `c` does nothing until you unmute |

The bars use `▁▂▃▄▅▆▇` and step once every 150 ms. They come from the clock, so the dashboard's redraw rate (every 100 ms) does not change.

**Narrow terminals.** The button uses the full text whenever it fits in the work cell, and otherwise shortens in this order:

1. Drop the state words and keep the glyph or bars and `end call`: `[c] ● end call`, `[c] … end call`, `[c] 📞 ▂▄▆▄▂ end call`.
2. While the chief speaks, drop the bars too: `[c] 📞 end call`.

While the chief speaks, `[t] talk now` is kept ahead of the state words. The pair shrinks as `[c] 📞 ▂▄▆▄▂ chief speaking · end call [t] talk now`, then `[c] 📞 ▂▄▆▄▂ end call [t] talk now`, then `[c] 📞 end call [t] talk now`, then `[c] 📞 end call [t] talk`, then `[c] 📞 end call` alone. The last step hides the talk-now button, and `t` still works.

The other states shorten like this: off is always `[c] 📞 call`; not set up and voice muted shorten to `[c] 📞 not set up` and `[c] 📞 voice muted`. The narrowest text is used when none fits. The dashboard's tests cover `end call` at every width.

### Talk now

You cannot talk over the chief: speech during an answer is not heard. To cut an answer short, press `t` on the chief's row while the chief speaks. Harold stops the sound and listens at once.

After talk now, lines the chief sends until your next turn is typed are reported as said and not played. Other agents' updates and the restart phrase (`The voice stopped. One moment.`) wait until your next turn is cut, then play. If you start no turn within 10 seconds of talk now, they play then, so a delivery waiting on them is never held for long. The chief's answer to a turn and Harold's own replies to a turn (such as `Sorry, I didn't catch that.`) keep playing.

### The display and the lock screen

During a call Harold holds `caffeinate -d -w <Harold's pid>` as a child program, so the display does not sleep and the screen does not lock from being idle. It is stopped when the call ends, whatever the reason. With `-w`, `caffeinate` also exits when Harold does, even when Harold is killed without stopping it. If it cannot start, the call goes on and `harold.log` gets `display not kept awake`.

A known limit: a lock you make by hand still sends the chief's line to the phone, because Harold sees the screen as locked.

### No call while Harold's voice is muted

A call plays its sound through Harold's voice, so Harold starts no call while the voice is muted. The state stays off, the last result is `voice muted`, and the log gets `call not started` with `reason = voice_muted`. The button shows `[c] 📞 call: voice muted`, and `c` does nothing until you unmute the voice.

Muting the voice during a call ends the call at once, with no last words. The call checks the mute as each block of samples arrives. The state turns off, the last result is `voice muted`, and the log gets `call ended` with `reason = voice_muted`.

### Where this differs from the spec

Harold differs from the spec in these behaviours, each for the reason given:

- **The button shortens in a narrow terminal.** The spec gives only the full button texts. Harold drops the state words first, then the bars, and keeps `[t] talk now` ahead of the state words ([Narrow terminals](#the-button)). The reason: the button sits inside the chief's work cell so a call never changes the list's column widths, and in a narrow terminal the full text would cut off `end call`, the words that say how to end the call.
- **A turn said before `bye` keeps its 2-minute retry after the call ends.** The spec drops any turn not yet typed when the call ends. Harold passes the turn on once the chief is free ([Hanging up by voice](#hanging-up-by-voice)). The reason: you said the turn for the chief, and hanging up in the same breath should not lose it. The retry needs no microphone or speaker.
- **`caffeinate` keeps the display awake for the call.** The spec does not cover the screen. Harold holds `caffeinate -d -w <Harold's pid>` while a call is open ([The display and the lock screen](#the-display-and-the-lock-screen)). The reason: when the screen locks from being idle, Harold sends the chief's line to the phone instead of saying it into the call. The known limit: a lock you make by hand still sends the line to the phone.
- **START refuses while Harold's voice is muted.** The spec does not cover Harold's mute. Harold starts no call, and the button shows `[c] 📞 call: voice muted` ([No call while Harold's voice is muted](#no-call-while-harolds-voice-is-muted)). The reason: a call plays its sound through Harold's voice, so a call with the voice muted could hear you but never answer.
- **A forgotten call ends by itself.** A call with no turn cut and no sound played for 10 minutes says `Bye.` and ends with `reason = idle`. The spec has no such limit. The reason: `caffeinate` keeps the Mac awake and unlocked for as long as a call is open, so a call left open must end by itself.
- **Muting the voice ends an open call.** The spec does not cover Harold's mute. In the same spirit as START refusing while the voice is muted, muting the voice during a call ends it at once, with `reason = voice_muted`.
- **Held lines wait at most 10 seconds after talk now.** The spec holds other agents' updates until your next turn. Harold plays them 10 seconds after talk now when you start no turn, because another agent's update is delivered from a thread that waits until its line has been said, and a held line would stall that delivery.
- **A turn Harold promised to pass on is not dropped when the call ends.** The spec drops any turn not yet typed when the call ends. Harold keeps trying to type it until 2 minutes after it was first held ([A turn waiting for a busy chief](#a-turn-waiting-for-a-busy-chief)). The reason: Harold has said `I'll pass it on when it's free.`, and ending the call must not break that promise. Only Harold stopping drops it.

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

The worker takes Kokoro's model, voice and language from `[tts] args`: `--model` (default `mlx-community/Kokoro-82M-bf16`), `--voice` (default `af_heart`) and `--lang_code` (default `a`), so a call sounds like the desk.

With no `[voice_input]` section, the button shows `[c] 📞 call: not set up`, and nothing runs. Harold reads the configuration only at startup.

The fixed limits are not settings: the 1.2-second silence, the 60-second turn, the 3-second tick, the 90-second wait, the 2-minute retry, the 30-second worker answer, the 10-second hold after talk now and the 10-minute idle end.

### The microphone permission

macOS charges the microphone permission to the program that runs `ffmpeg`, which is `harold`, signed with a stable identity, so the grant should survive updates. macOS should ask the first time a call starts. If it does not (not known for a program that launchd started), add `harold` under System Settings > Privacy & Security > Microphone. Until it is allowed, `ffmpeg` produces no audio or exits, and the call ends with `microphone_failed`.

## The voice worker

The voice worker keeps the speech models loaded for the length of a call, so a turn does not wait for them to load.

**What it is.** A Python program (`harold/src/voice_worker.py`) compiled into `harold` as text. When a call starts, Harold writes it to the call's folder as `voice_worker.py` (mode 0600, in a folder of mode 0700) and runs it with `[voice_input]`'s Python, as `python -I voice_worker.py` with the model, voice and language arguments. `HF_HUB_OFFLINE=1` is set, so a missing model fails to load instead of downloading. Nothing downloads a model during a call.

**What it loads.** Two models, once:

- Whisper large-v3-turbo (the model named in `[voice_input]`), through `mlx_audio.stt.utils.load_model`;
- Kokoro, through `mlx_audio.tts.utils.load_model`.

It then says a throwaway line, so a missing voice fails at load and not on the first answer. It imports only what the installed `mlx_audio` already brings, plus `numpy` and `soundfile`.

**The protocol.** One JSON object per line, on standard input and standard output. The worker redirects its other output to standard error, which Harold discards.

| Direction | Line | Meaning |
|---|---|---|
| worker to Harold | `{"ready": true}` | Both models are loaded. `{"ready": false}` means loading failed. It must come within 120 seconds, or the load counts as `failed_to_load`. |
| Harold to worker | `{"op": "transcribe", "wav": <path>}` | Answered with `{"ok": true, "text": ..., "no_speech_prob": ...}` |
| Harold to worker | `{"op": "say", "text": ..., "wav": <path>}` | Answered with `{"ok": true}` once the file is written |

Any answer with `"ok"` other than true means that one request failed. The worker stays up, the turn is dropped with result `failed`, and a line that was to be spoken goes through the `[tts]` command instead. A worker answers one request at a time and must answer each within 30 seconds, or Harold kills it.

**When it starts and stops.** It starts with the call, and the first turn's transcription waits for the load if it ends sooner (up to 15 seconds more on the first turn). It stops when the call ends: Harold closes its input, waits up to 5 seconds for it to exit, then kills it. Outside a call, `harold speak` works as before, one `[tts]` command per line.

**Phrases.** Harold's own phrases are made once by the worker and kept in `$TMPDIR/harold-voice-phrases/` as `<first 16 hex of sha256(voice + "\n" + phrase)>.wav`, so a later call plays them without waiting. Two phrases always go through the `[tts]` command, never the worker, because the worker is the thing that failed: `The voice stopped. One moment.` and `The voice failed. Call ended.` `There's no chief running.` at the start comes before any worker exists, so it plays from the phrase folder when the file is there, and otherwise through `[tts]`.

**Memory.** About 2 GB while a call is open, between turns as well: Whisper large-v3-turbo about 1.6 GB and Kokoro about 0.35 GB. None between calls. This is the spec's estimate, not measured.

### If the worker dies

Harold notices when the worker's output ends, when it does not answer a request within 30 seconds, or when it does not load within 120 seconds. Each is logged as `voice worker exited` with `reason` `exited`, `timed_out` or `failed_to_load`.

- **First death, after it had loaded.** The turn in hand is dropped, with result `failed`. Harold starts the worker again and says `The voice stopped. One moment.` through `[tts]`, then listens again.
- **Second death in the same call, a worker that never loaded (`failed_to_load`), or a restart that cannot start.** Harold says `The voice failed. Call ended.` and ends the call with `voice_failed`.

For example, if Whisper is not downloaded, the worker fails to load, and the call ends at once with `The voice failed. Call ended.`

Before the ready line, the worker's output may hold a line that is not JSON, such as a library's stray print. Harold skips such lines. The first JSON line must be `{"ready": true}`, or the load fails. After the ready line, a line that is not JSON ends the worker.

### Why did the voice fail to load?

Harold discards the worker's standard error, so `harold.log` shows only `voice worker exited` with `reason = failed_to_load`. To see the error, run the worker by hand the way Harold runs it. Harold writes the script into the call's folder, runs it from there with `HF_HUB_OFFLINE=1`, and passes the values from `[voice_input]` and `[tts] args`:

```sh
cd "$(mktemp -d)"
cp <harold repo>/harold/src/voice_worker.py .
HF_HUB_OFFLINE=1 <[voice_input] command> -I voice_worker.py \
  --stt-model <Whisper model> --tts-model <Kokoro model> \
  --voice <voice> --lang-code <lang code> < /dev/null
```

`<Whisper model>` is the word after `--model` in `[voice_input] args`. The Kokoro model, voice and language come from `[tts] args`, with the defaults in [Settings](#settings). The worker prints `{"ready": true}` when it loads, or `{"ready": false}` when it does not, and then exits, because its input is empty. Either way of failing prints its traceback to standard error, where you see it: an import error, such as a Python without `mlx_audio`, and a model or voice that fails to load. Harold still discards standard error, so no traceback reaches `harold.log`.

## Privacy

- **On the Mac.** Audio and transcripts stay on the Mac. The models and commands are local, and no cloud service is used.
- **Audio.** Each turn's audio lives only until its transcript returns, and is deleted even on failure. Samples between turns are kept only in memory. The voice worker keeps no audio or text after answering. The audio of the lines it made for a call is deleted after it plays, and the worker's script and the call's folder are deleted when the call ends. If Harold is killed during a call, its folder stays in `$TMPDIR/harold-voice-input/` until Harold next starts: at start-up Harold removes every `call-*` folder left there, and nothing else in that folder.
- **Phrases kept.** The phrase folder holds only Harold's own fixed phrases, never your words.
- **Transcripts.** Harold does not store the transcript as an event, and never writes it to `harold.log`. It is shown in tmx-agent-dash while the call lasts, through the `Call` answer's `last_transcript`, and cleared when the call ends.
- **Telemetry.** harold-telemetry never exports text. The typed line becomes a turn's prompt like any typed prompt.
- **The chief's session.** The prompt typed into the chief's pane is in the chief's own session. That is how the chief hears it.

### Log lines

Messages are fixed text, with values in fields, as harold-telemetry's test `every_log_message_is_fixed_text` requires. These are the lines the call writes to `harold.log`:

| Level | Message | Fields |
|---|---|---|
| info | `call started` | none |
| info | `call ended` | `reason`: `stopped`, `bye`, `no_chief`, `voice_failed`, `microphone_failed`, `shutdown`, `idle` or `voice_muted` |
| info | `call not started` | `reason`: `voice_muted` |
| info | `spoken prompt typed` | `pane_id`, `words` (a count) |
| info | `spoken prompt dropped` | `reason`: `nothing_heard`, `no_chief`, `chief_busy` or `failed` |
| warn | `voice worker exited` | `reason`: `exited`, `timed_out` or `failed_to_load` |
| warn | `display not kept awake` | `error` |
| warn | `microphone not opened` | `error` |
| warn | `call folder not made` | `error` |
| warn | `call folder not removed` | `error`; at the end of a call, or for a leftover folder at start-up |
| warn | `turn not written` | `error` |
| warn | `phrase not kept` | `error` |
| warn | `call sound not played` | `error` |
| warn | `turn-end update not made` | none |

`call ended` is also logged for a call that ended before it started listening, without a `call started`.

harold-telemetry exports only fields on its allowed list. `pane_id` and `reason` go out. `words` and `error` are not on the list, so they stay in `harold.log` only. No transcript and no spoken line is in any log line, event or exported record; a test with a canary transcript checks this.

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

The first turn of a call can take longer, while the worker loads: up to 15 seconds more. Nothing in the table has been measured on a real call yet.

## The `Call` RPC

tmx-agent-dash controls the call over one gRPC call in `harold-api/proto/harold.proto`: `Call(CallRequest { action }) returns (CallResponse { state, chief_pane_id, last_transcript, last_result })`.

| Field | Values |
|---|---|
| `action` | `START`, `STOP`, `TALK_NOW`, `STATUS` |
| `state` | `OFF`, `LISTENING`, `TRANSCRIBING`, `WAITING`, `SPEAKING`, `NOT_SET_UP` |
| `last_result` | `TYPED`, `NOTHING_HEARD`, `NO_CHIEF`, `CHIEF_BUSY`, `FAILED`, `VOICE_MUTED`; unspecified before any turn has ended in the call |

The dashboard shows `TRANSCRIBING` and `WAITING` both as `chief thinking`. It waits up to 8 seconds for the answer to `START`, `STOP` or `TALK_NOW`, longer than the 5 seconds Harold may take to close the voice worker when a call ends, and up to 2 seconds for a `STATUS` poll, which Harold answers at once. `chief_pane_id` is empty when no chief runs. `START` answers once the call listens or has ended. A state the dashboard does not know counts as not set up.

## The shared core and away-web-app

The conversation core is one design with two places for audio. It does four things: it finds the end of speech, takes turns, turns speech into text, and types the text into the chief's pane marked `🎙`. This page's call is the desk's: the microphone through `ffmpeg` and the speakers through `afplay`. away-web-app's call mode (its stage 1b) adds only the phone's side, recording and playing audio in the app and carrying it to and from the Mac, by supplying its own source and sink. The phone's side is built; [The phone's side](#the-phones-side) describes it.

The core, in `harold/src/voice_input.rs`, is internal Rust, not an API. A call is built from three traits.

**`AudioSource`** is where a call's audio comes from.

- `next_block(&mut self)` resolves to the next block of 16 kHz mono samples as `i16` values, of any length, an empty block included. The byte order of whatever carries them is the source's business.
- `None` means the source has ended (`ffmpeg` exited, the phone hung up), and every later call returns `None`. The core then ends the call with `microphone_failed`.
- It is cancel-safe: dropping the future before it resolves loses no samples and shifts none.

**`AudioSink`** is where a call's sound goes. The core plays one file at a time and knows when each ends.

- `play(&self, wav)` plays the 16-bit PCM WAV at `wav`, at any sample rate. Only one `play` runs at a time. It resolves `Ok` when the sound has ended or `stop` ended it, and `Err` when it could not be played. The core then says the line through `[tts]` instead.
- `stop(&self)` stops what `play` is playing now and returns at once; the sound stops promptly. It does nothing when nothing plays, and does not affect a later `play`.

**`CallPorts`** bundles everything a call reads and does outside the core. The daemon's is `MacPorts`; the phone's is `PhonePorts`, built on the desk's `MacPorts`. It names a `Source`, a `Sink`, a `Worker`, the chief's `Panes`, and an `Awake` value held for the call (`caffeinate` at the desk; dropping it lets the display sleep). Its methods are:

| Method | Does |
|---|---|
| `open_source`, `sink` | Open the call's source; give the one sink every call plays through. |
| `start_worker(call_dir)` | Start the voice worker; loading goes on in the background. |
| `panes`, `chief_folder` | The chief's pane and the folder (`[notify] away_messages_only_from`). |
| `speak_with_tts`, `stop_desk_line` | Say a line with the `[tts]` command and resolve when it ends; stop such a line. |
| `turn_end_update(prompt)` | The desk's turn-end update for a turn that ended without `harold speak`. |
| `calls_dir`, `phrases_dir`, `voice_name` | Where call folders and kept phrases live, and the Kokoro voice in each phrase's file name. |
| `voice_muted` | Whether Harold's voice is muted now. |
| `keep_awake` | Hold the display awake for the call. |

`CallControl` runs at most one call. `start`, `stop`, `talk_now` and `status` are what the `Call` RPC calls. `say_blocking` plays a line in the open call from a blocking thread, which is how `harold speak` and other agents' turn-end updates reach it. `CallControl::say`, its async twin, is compiled only in tests (`#[cfg(test)]`). The phone's call does not need it: every line it says arrives on a blocking thread (the projector's `spawn_blocking`) and goes through `say_blocking`, as at the desk.

Tests use a fake source, sink, worker and pane; nothing in them touches the microphone, the speakers or a model.

## The phone's side

away-web-app's call mode (its stage 1b) runs on this core. [The away app reference](../away-app/README.md#calls-stage-1b) describes what you see and hear; this section is what the phone supplies in place of the desk's parts. The phone's calls are built only when `[web_app]` is set, on the same `MacPorts`, so the transcription worker, the chief's pane and the voice name are the desk's.

**The source and the sink.**

- **The source** is `PhoneSource`. The app posts quarter-second pieces of 16 kHz mono audio (`POST /call/audio`), and the source hands each to the core with the time the app recorded it. A piece with an odd number of bytes, more than one second of audio, or no time is refused and the call goes on; one that arrives with no call open, or carries no id or another call's id, is refused with `409`. Each piece carries the id `POST /call` answered: a new one for each call, so a piece from an earlier call never reaches a later one. The source ends, and the call with it, when the phone posts nothing for 15 seconds while nothing plays.
- **The sink** is `PhoneSink`. `play` does not play anything on the Mac. It offers the WAV under a new id; the app fetches it (`GET /call/answer/<id>`), plays it and reports it (`POST /call/played`). `play` resolves when the phone reports, or `stop` (Talk now) takes the sound back. A sound the phone fetched and never reported counts as played after its length plus 10 seconds. One it never fetched fails, so the core says the line through `[tts]`, which on the phone is its own voice. Nothing is offered for more than 2 minutes.

**Muting by capture time.** The core ignores a block recorded while a sound played or within 0.3 seconds after. At the desk a block's capture time is when it was read. On the phone a piece may be delayed by the network, so the source stamps each with when the app recorded it (the call's start plus the piece's `at`), and the core compares that with the sound's end. The app counts `at` from just before it sends `POST /call`, and the source counts from when that request arrived, before a desk call is ended, so a piece's stamp is late by no more than the request's time on the network. The app also stops sending during that time, so the core's rule is the second guard.

**The phone's own voice.** What Harold says only through `[tts]` at the desk, `The voice stopped. One moment.` and `The voice failed. Call ended.`, and any sound the phone never fetched, goes to the phone as a line, not a sound. The app says it with the phone's `speechSynthesis`, and reports it said. The core waits for that report, or for 2 seconds plus 80 milliseconds for each character, whichever comes first. Harold's voice mute silences the Mac's speakers, not the phone, so it does not end or refuse a phone's call. Until the call opens (its worker has started), the sink and the phone's own voice offer nothing and return at once: a call that ends before it opens answers `POST /call` with `409` and the line, which the phone shows, so `There's no chief running.` and a voice failure at the start are shown, not heard.

**`say_in_open_call`.** `harold speak` and other agents' turn-end updates reach an open call through `say_in_open_call`. It asks the desk's call first, then the phone's, and the first with a call open takes the line. Before the screen lock decides where the chief's spoken line goes, the projector asks only the phone's call (`say_in_phone_call`). With no phone call open, the line goes by the lock's rules, as at the desk: with the screen unlocked it is said through `say_in_open_call`, into the desk's call if one is open; with the screen locked it goes to the phone, as [The display and the lock screen](#the-display-and-the-lock-screen) describes. A line the chief says in the phone's call plays on the phone and is added to the thread as text, with no push and no iMessage. Starting a call on the phone ends a call open at the desk first. Nothing stops the dashboard from starting a desk call while the phone's call is open.

**A newer screen.** A `POST /call` while the phone's call is open takes it over: the call and its worker go on, the call gets a new id, and its pieces are stamped from that request's arrival. The older screen's pieces are refused, and those it posted that still wait are not read. The call forgets the turn being spoken (`CallControl::forget_turn`, which resets the speech detector and keeps the background level), and the source forgets it again just before the newer screen's first piece, so whatever the older screen was saying is dropped, not typed.

**Logs.** A phone's call writes `app call started` and `app call ended` with `reason`, and its turns write the lines above. No transcript, audio or line text is in any of them.
