// The call with the chief. The server draws the call screen, #call, and the event stream
// morphs it as the call changes. This script records the microphone and posts it to
// Harold, plays the sound or says the line #call names, and keeps the screen's timer. It
// reads #call's data-harold-* attributes again after every change, so a morph that
// replaces #call loses nothing.
"use strict";

(function () {
  const timer = document.getElementById("call-timer");
  const notice = document.getElementById("call-notice");
  if (!document.getElementById("call-start") || !timer || !notice) {
    return;
  }

  // The microphone hears a sound's echo for a moment after it stops.
  const MUTE_TAIL_S = 0.3;
  const REFUSAL_MS = 3000;
  const ENDED_BY_HIDING = "Call ended: the app was closed or the phone locked";
  // Each bar's share of the level, so the five do not move as one.
  const BAR_SHARES = [0.55, 0.8, 1, 0.75, 0.5];

  // The open call, or null.
  let call = null;
  // What plays now, { id, stop }, or null.
  let playback = null;
  // #call's attributes as last read, so only a change acts.
  const seen = { sound: "", speech: "" };

  // Every request but the audio's says JSON: Harold takes no post from a page without a
  // header another site could not send.
  function send(method, path) {
    return fetch(path, { method, headers: { "Content-Type": "application/json" } });
  }

  function showNotice(text) {
    notice.textContent = text;
    notice.hidden = !text;
  }

  function showTimer(current) {
    const seconds = Math.floor((performance.now() - current.shownAt) / 1000);
    const ss = String(seconds % 60).padStart(2, "0");
    timer.textContent = "Call with the chief · " + Math.floor(seconds / 60) + ":" + ss;
  }

  // 1. The Call tap: the context, the microphone and the screen's wake lock are all asked
  // for inside the tap, which the phone requires, and then the call opens on the Mac.
  async function start() {
    if (call) {
      return;
    }
    showNotice("");
    // One context for the call; made in the tap, it unlocks sound for the page.
    const ctx = new AudioContext();
    ctx.resume().catch(() => {});
    // The phone's own voice also wants its first word said inside a tap.
    if ("speechSynthesis" in window) {
      speechSynthesis.speak(new SpeechSynthesisUtterance(""));
    }
    const current = {
      ctx,
      stream: null,
      wakeLock: null,
      // The context's time just before the call is asked for, which pieces are stamped from.
      start: 0,
      // Whether Harold answered that the call listens, and whether #call has shown it.
      open: false,
      live: false,
      // Pieces recorded before this context time are dropped.
      mutedUntil: 0,
      // The audio posts, chained so each is sent once the one before has answered.
      posts: Promise.resolve(),
      shownAt: performance.now(),
      timerId: 0,
    };
    call = current;
    document.body.classList.add("in-call");
    showTimer(current);
    current.timerId = setInterval(() => showTimer(current), 1000);
    try {
      const microphone = navigator.mediaDevices.getUserMedia({
        audio: { echoCancellation: true, channelCount: 1 },
      });
      if (navigator.wakeLock) {
        navigator.wakeLock.request("screen").then(
          (lock) => {
            if (call === current) {
              current.wakeLock = lock;
            } else {
              lock.release().catch(() => {});
            }
          },
          // No wake lock: the screen may sleep, which ends the call as leaving the app does.
          () => {},
        );
      }
      current.stream = await microphone;
      await ctx.audioWorklet.addModule("/static/call-worklet.js");
      if (call !== current) {
        release(current);
        return;
      }
      const recorder = new AudioWorkletNode(ctx, "harold-call-recorder");
      recorder.port.onmessage = (event) => post(current, event.data);
      ctx.createMediaStreamSource(current.stream).connect(recorder);
      // The recorder outputs silence; joined to the speakers so the phone keeps running it.
      recorder.connect(ctx.destination);
      current.start = ctx.currentTime;
      const response = await send("POST", "/call");
      if (call !== current) {
        // Hung up while it opened: a DELETE sent then may have come first, so end it again.
        if (response.ok && !call) {
          send("DELETE", "/call").catch(() => {});
        }
        return;
      }
      if (response.status === 409) {
        refuse(current, await response.text());
        return;
      }
      if (!response.ok) {
        end(current);
        return;
      }
      current.open = true;
      sync();
    } catch {
      // No microphone, or Harold could not be reached: back to the thread.
      if (call === current) {
        end(current);
      } else {
        release(current);
      }
    }
  }

  // Harold said why no call started: the screen shows it for a moment, then the thread.
  function refuse(current, why) {
    end(current, true);
    showNotice(why);
    setTimeout(() => {
      if (!call) {
        showNotice("");
        document.body.classList.remove("in-call");
      }
    }, REFUSAL_MS);
  }

  // Ends the call on the phone: the microphone, the wake lock, the sound and the timer
  // stop, and the thread shows unless `keepScreen`.
  function end(current, keepScreen) {
    if (call !== current) {
      return;
    }
    stopPlayback();
    call = null;
    release(current);
    if (!keepScreen) {
      document.body.classList.remove("in-call");
    }
  }

  // Lets go of what `current` holds; safe to call again, and for a call already ended.
  function release(current) {
    clearInterval(current.timerId);
    if (current.stream) {
      current.stream.getTracks().forEach((track) => track.stop());
    }
    if (current.wakeLock) {
      current.wakeLock.release().catch(() => {});
      current.wakeLock = null;
    }
    if (current.ctx.state !== "closed") {
      current.ctx.close().catch(() => {});
    }
  }

  // 3. Each piece goes to Harold stamped with when it was recorded, in milliseconds from
  // the call's start. Pieces recorded while something plays, or in the echo's tail after,
  // are dropped, so the chief never hears itself. The pieces are posted one after another,
  // since Harold hears them in the order they arrive; those still queued when the call
  // ends are dropped, and one that fails does not hold up the rest.
  function post(current, piece) {
    if (call !== current || !current.open || playback) {
      return;
    }
    if (piece.at < current.start || piece.at < current.mutedUntil) {
      return;
    }
    const at = Math.round((piece.at - current.start) * 1000);
    current.posts = current.posts.then(async () => {
      if (call !== current) {
        return;
      }
      const answer = await fetch("/call/audio?at=" + at, {
        method: "POST",
        headers: { "Content-Type": "application/octet-stream" },
        body: piece.samples,
      });
      // No call open on the Mac: it ended, and the stream that says so did not reach the
      // phone. Only this call's own pieces end it, so a late one cannot end a newer call.
      if (answer.status === 409 && call === current && current.open) {
        end(current);
      }
    }).catch(() => {});
  }

  // Lets go of what plays if it is still `id`, and starts the echo's tail; true when it
  // was. Cleared before it stops, so its own end event does not report it played.
  function letGo(id) {
    if (!playback || playback.id !== id) {
      return false;
    }
    const ended = playback;
    playback = null;
    ended.stop();
    if (call) {
      call.mutedUntil = call.ctx.currentTime + MUTE_TAIL_S;
    }
    return true;
  }

  function stopPlayback() {
    if (playback) {
      letGo(playback.id);
    }
  }

  function reportPlayed(id) {
    send("POST", "/call/played?id=" + encodeURIComponent(id)).catch(() => {});
  }

  // 4. A sound Harold offers: fetched, decoded and played through the call's context, with
  // an analyser for the bars, and reported once it has played to its end. The microphone
  // is muted from the moment it is offered.
  async function playSound(current, id) {
    stopPlayback();
    let source = null;
    playback = {
      id,
      stop() {
        setBars(null);
        if (source) {
          try {
            source.stop();
          } catch {
            // It had not started, or had already ended.
          }
          source.disconnect();
        }
      },
    };
    try {
      const response = await fetch("/call/answer/" + encodeURIComponent(id));
      if (!response.ok) {
        // No longer offered: nothing to play.
        letGo(id);
        return;
      }
      const audio = await current.ctx.decodeAudioData(await response.arrayBuffer());
      if (!playback || playback.id !== id) {
        return;
      }
      const analyser = current.ctx.createAnalyser();
      source = current.ctx.createBufferSource();
      source.buffer = audio;
      source.connect(analyser);
      analyser.connect(current.ctx.destination);
      source.onended = () => {
        if (letGo(id)) {
          reportPlayed(id);
        }
      };
      source.start();
      followLevel(analyser, id);
    } catch {
      // Not fetched or not decoded: Harold gives up on it after a while.
      letGo(id);
    }
  }

  // 4. A line for the phone's own voice works the same way through speechSynthesis.
  function say(id, text) {
    stopPlayback();
    if (!("speechSynthesis" in window)) {
      return;
    }
    const utterance = new SpeechSynthesisUtterance(text);
    utterance.onend = () => {
      if (letGo(id)) {
        reportPlayed(id);
      }
    };
    utterance.onerror = () => letGo(id);
    // Held in `playback` too: a dropped utterance may never fire its end event.
    playback = { id, utterance, stop: () => speechSynthesis.cancel() };
    speechSynthesis.speak(utterance);
  }

  // 5. The five bars follow the playing sound's loudness, frame by frame, and stay still
  // when the phone asks for reduced motion.
  function followLevel(analyser, id) {
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      return;
    }
    const samples = new Uint8Array(analyser.fftSize);
    function frame() {
      if (!playback || playback.id !== id) {
        return;
      }
      analyser.getByteTimeDomainData(samples);
      let sum = 0;
      for (const sample of samples) {
        const value = (sample - 128) / 128;
        sum += value * value;
      }
      // A spoken voice is about 0.1 to 0.25 in root mean square; scaled to fill the bars.
      setBars(Math.min(1, Math.sqrt(sum / samples.length) * 4));
      requestAnimationFrame(frame);
    }
    requestAnimationFrame(frame);
  }

  // Sets each bar's height for `level`, from 0 to 1; null leaves them to the style sheet.
  function setBars(level) {
    document.querySelectorAll("#call .bars i").forEach((bar, i) => {
      const scale = 0.15 + 0.85 * level * BAR_SHARES[i];
      bar.style.transform = level === null ? "" : "scaleY(" + scale + ")";
    });
  }

  // Reads #call after each change and acts on what changed.
  function sync() {
    const screen = document.getElementById("call");
    if (!screen) {
      return;
    }
    const state = screen.getAttribute("data-harold-state") || "off";
    const sound = screen.getAttribute("data-harold-sound") || "";
    const speech = screen.getAttribute("data-harold-speech") || "";
    const before = { sound: seen.sound, speech: seen.speech };
    seen.sound = sound;
    seen.speech = speech;
    const current = call;
    if (!current || !current.open) {
      return;
    }
    // 8. The call ended on the Mac, by a bye or for any other reason: back to the thread.
    if (state !== "off") {
      current.live = true;
    } else if (current.live) {
      end(current);
      return;
    }
    // 4. A new id plays; a cleared one stops.
    if (sound !== before.sound) {
      letGo(before.sound);
      if (sound) {
        playSound(current, sound);
      }
    }
    if (speech !== before.speech) {
      letGo(before.speech);
      if (speech) {
        say(speech, screen.getAttribute("data-harold-speech-text") || "");
      }
    }
  }

  // 6. Delegated from the document: the stream's morphs replace #call's buttons.
  document.addEventListener("click", (event) => {
    const target = event.target instanceof Element ? event.target : null;
    if (!target) {
      return;
    }
    if (target.closest("#call-start")) {
      start().catch(() => {});
    } else if (target.closest("#call-talk-now")) {
      stopPlayback();
      send("POST", "/call/talk-now").catch(() => {});
    } else if (target.closest("#call-end")) {
      if (call) {
        send("DELETE", "/call").catch(() => {});
        end(call);
      } else {
        // The screen still shows why no call started.
        showNotice("");
        document.body.classList.remove("in-call");
      }
    }
  });

  // 7. The app left the front or the phone locked: the call ends at once, on the Mac too,
  // with a request that outlives the page, and the screen says why on return.
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState !== "hidden" || !call) {
      return;
    }
    fetch("/call", {
      method: "DELETE",
      keepalive: true,
      headers: { "Content-Type": "application/json" },
    }).catch(() => {});
    end(call);
    showNotice(ENDED_BY_HIDING);
  });

  new MutationObserver(sync).observe(document.body, {
    subtree: true,
    childList: true,
    attributes: true,
    attributeFilter: ["data-harold-state", "data-harold-sound", "data-harold-speech"],
  });
  sync();
})();
