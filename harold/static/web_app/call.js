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

  // The notice floats under the header, so the pages pad by its height while it shows.
  new ResizeObserver(() => {
    const height = notice.hidden ? 0 : notice.offsetHeight;
    document.documentElement.style.setProperty("--notice-height", height + "px");
  }).observe(notice);

  // The microphone hears a sound's echo for a moment after it stops.
  const MUTE_TAIL_S = 0.3;
  const REFUSAL_MS = 3000;
  // How long after Harold answered POST /call the page reports what the microphone gave.
  const CHECK_MS = 3000;
  const ENDED_BY_HIDING = "Call ended: the app was closed or the phone locked";
  const NO_SOUND = "The microphone gave no sound. Tap Call to try again.";
  // Harold accepts an error name of 1 to 40 letters and nothing else.
  const ERROR_NAME = /^[A-Za-z]{1,40}$/;
  // Each bar's share of the level, so the five do not move as one.
  const BAR_SHARES = [0.55, 0.8, 1, 0.75, 0.5];

  // The open call, or null.
  let call = null;
  // What plays now, { id, stop }, or null.
  let playback = null;
  // #call's attributes as last read, so only a change acts.
  const seen = { sound: "", speech: "" };

  // Datastar's own module, the one the page loads: its signals and its @post.
  const datastar = import("/static/datastar-1.0.2.js");

  // Sets `signals`, then sends `include`d signals with Datastar's @post (or `action`). Resolves
  // to the HTTP status when Datastar announced an error for the request, else 200; rejects when
  // the request could not be sent. A request is never retried, so one failed post is one
  // post, and a 401 (the device is no longer paired) reloads to the pair screen.
  async function post(element, url, signals, include, options = {}) {
    // A request with no element of its own gets one, so another request's error is not taken for its.
    const el = element || document.createElement("span");
    const { actions, mergePatch } = await datastar;
    mergePatch(signals);
    let status = 200;
    const note = (event) => {
      const detail = event.detail;
      if (detail && detail.type === "error" && detail.el === el) {
        status = Number(detail.argsRaw && detail.argsRaw.status) || 0;
      }
    };
    document.addEventListener("datastar-fetch", note);
    try {
      await actions[options.action || "post"](
        { el, evt: undefined, error: (name) => new Error(name), cleanups: new Map() },
        url,
        {
          filterSignals: { include },
          openWhenHidden: true,
          requestCancellation: "disabled",
          retry: "never",
          retryMaxCount: 0,
          ...options.request,
        },
      );
    } finally {
      document.removeEventListener("datastar-fetch", note);
    }
    if (status === 401) {
      location.reload();
    }
    return status;
  }

  // Every request but the audio's goes through Datastar's @post or @delete, which sends
  // `Datastar-Request: true`: Harold takes no post from a page without a header another site
  // could not send. These send no signal, and one that hides with the page stays open.
  function send(method, path) {
    return post(null, path, {}, /^none$/, {
      action: method === "DELETE" ? "delete" : "post",
      request: { openWhenHidden: true },
    });
  }

  // The fields of the call check, as signals; each is cleared before a check sets its own.
  const CHECK_FIELDS = [
    "problem",
    "step",
    "error_name",
    "context_state",
    "track_state",
    "track_muted",
    "pieces_made",
    "pieces_posted",
    "pieces_dropped",
    "posts_failed",
    "empty_blocks",
  ];
  const CHECK_SIGNALS =
    /^(problem|step|error_name|context_state|track_state|track_muted|pieces_made|pieces_posted|pieces_dropped\..*|posts_failed|empty_blocks)$/;

  // A 401 on the audio post: this device is no longer paired, and the page reloads to the
  // pair screen.
  function reloadWhenUnpaired(response) {
    if (response.status === 401) {
      location.reload();
    }
    return response;
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
    // Once each time the context leaves `running` (the phone's audio session can suspend or
    // interrupt it): resume it. A closed context is the call's own end.
    ctx.onstatechange = () => {
      if (ctx.state === "suspended" || ctx.state === "interrupted") {
        ctx.resume().catch(() => {});
      }
    };
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
      // The id Harold answered POST /call with, which each piece carries.
      id: "",
      // Whether Harold answered that the call listens, and whether #call has shown it.
      open: false,
      live: false,
      // Pieces recorded before this context time are dropped.
      mutedUntil: 0,
      // The audio posts, chained so each is sent once the one before has answered.
      posts: Promise.resolve(),
      shownAt: performance.now(),
      timerId: 0,
      checkId: 0,
      // What the page saw, which the check reports: the first audio track, the pieces the
      // worklet made, those handed to be posted and those dropped first (by reason), the
      // posts that failed, and the blocks the worklet counted with no input.
      track: null,
      trackMuted: false,
      made: 0,
      posted: 0,
      dropped: { playing: 0, echo_tail: 0, before_start: 0, not_open: 0 },
      failed: 0,
      empty: 0,
    };
    call = current;
    document.body.classList.add("in-call");
    showTimer(current);
    current.timerId = setInterval(() => showTimer(current), 1000);
    // The step in hand, which a thrown error is reported under.
    let step = "microphone";
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
      watchTrack(current);
      step = "worklet";
      await ctx.audioWorklet.addModule("/static/call-worklet.js");
      if (call !== current) {
        release(current);
        return;
      }
      step = "context";
      // Starting the microphone can switch the phone's audio session and leave a context
      // made in the tap suspended.
      await ctx.resume().catch(() => {});
      const recorder = new AudioWorkletNode(ctx, "harold-call-recorder", {
        processorOptions: { report: true },
      });
      recorder.port.onmessage = (event) => heard(current, event.data);
      ctx.createMediaStreamSource(current.stream).connect(recorder);
      // The recorder outputs silence; joined to the speakers so the phone keeps running it.
      recorder.connect(ctx.destination);
      current.start = ctx.currentTime;
      step = "call";
      // A 200 patches `_callId` to the call's id, or `_callWhy` to the line that gives the
      // reason no call started.
      const { getPath, mergePatch } = await datastar;
      mergePatch({ _callId: "", _callWhy: "" });
      const status = await send("POST", "/call");
      const id = getPath("_callId");
      const why = getPath("_callWhy");
      if (call !== current) {
        // Hung up while it opened: a DELETE sent then may have come first, so end it again.
        if (id && !call) {
          send("DELETE", "/call").catch(() => {});
        }
        return;
      }
      if (why) {
        refuse(current, why);
        return;
      }
      if (status !== 200 || !id) {
        end(current);
        return;
      }
      current.id = id;
      current.open = true;
      current.checkId = setTimeout(() => check(current), CHECK_MS);
      sync();
    } catch (error) {
      // No microphone, or Harold could not be reached: back to the thread. Harold is told
      // which step threw and the error's name, unless the call was hung up meanwhile.
      if (call === current) {
        const name = error && typeof error.name === "string" ? error.name : "";
        postCheck({ ...facts(current), step, ...(ERROR_NAME.test(name) ? { error_name: name } : {}) });
        end(current);
      } else {
        release(current);
      }
    }
  }

  // Keeps the first audio track's `muted` as it changes; its `readyState` is read when asked.
  function watchTrack(current) {
    const track = current.stream.getAudioTracks()[0];
    if (!track) {
      return;
    }
    current.track = track;
    current.trackMuted = track.muted;
    const update = () => {
      current.trackMuted = track.muted;
    };
    track.addEventListener("mute", update);
    track.addEventListener("unmute", update);
    track.addEventListener("ended", update);
  }

  // The facts the check reports, in the words and counts Harold accepts.
  function facts(current) {
    const state = current.ctx.state;
    return {
      context_state: ["running", "suspended", "closed", "interrupted"].includes(state)
        ? state
        : undefined,
      track_state: current.track ? current.track.readyState : "none",
      track_muted: current.trackMuted,
      pieces_made: current.made,
      pieces_posted: current.posted,
      pieces_dropped: { ...current.dropped },
      posts_failed: current.failed,
      empty_blocks: current.empty,
    };
  }

  function postCheck(body) {
    post(
      null,
      "/call/check",
      { ...Object.fromEntries(CHECK_FIELDS.map((field) => [field, null])), ...body },
      CHECK_SIGNALS,
      { request: { openWhenHidden: true } },
    ).catch(() => {});
  }

  // 3 s after Harold answered POST /call: tells Harold what the microphone gave. With no
  // piece made by then the call ends here, saying so, instead of waiting out Harold's
  // 15 s of silence.
  function check(current) {
    if (call !== current) {
      return;
    }
    const noSound = current.made === 0;
    postCheck(noSound ? { ...facts(current), problem: "no_sound" } : facts(current));
    if (noSound) {
      send("DELETE", "/call?why=no_sound").catch(() => {});
      end(current);
      showNotice(NO_SOUND);
    }
  }

  // A message from the worklet: the count of blocks with no input, or a piece to post.
  function heard(current, message) {
    if ("empty" in message) {
      current.empty = message.empty;
      return;
    }
    current.made += 1;
    post(current, message);
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
    clearTimeout(current.checkId);
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

  // 3. Each piece goes to Harold with the call's id and stamped with when it was recorded,
  // in milliseconds from the call's start. Pieces recorded while something plays, or in the echo's tail after,
  // are dropped, so the chief never hears itself. The pieces are posted one after another,
  // since Harold hears them in the order they arrive; those still queued when the call
  // ends are dropped, and one that fails does not hold up the rest.
  function post(current, piece) {
    if (call !== current) {
      return;
    }
    // Counted by the first reason that holds, for the check.
    const reason = !current.open
      ? "not_open"
      : playback
        ? "playing"
        : piece.at < current.start
          ? "before_start"
          : piece.at < current.mutedUntil
            ? "echo_tail"
            : "";
    if (reason) {
      current.dropped[reason] += 1;
      return;
    }
    const at = Math.round((piece.at - current.start) * 1000);
    current.posted += 1;
    current.posts = current.posts.then(async () => {
      if (call !== current) {
        return;
      }
      const path = "/call/audio?call=" + encodeURIComponent(current.id) + "&at=" + at;
      const answer = await fetch(path, {
        method: "POST",
        headers: { "Content-Type": "application/octet-stream" },
        body: piece.samples,
      });
      reloadWhenUnpaired(answer);
      if (!answer.ok) {
        current.failed += 1;
      }
      // No call open on the Mac for this id: it ended, and the stream that says so did not
      // reach the phone, or a newer screen's Call took it over. Only this call's own pieces
      // end it, so a late one cannot end a newer call.
      if (answer.status === 409 && call === current && current.open) {
        end(current);
      }
    }).catch(() => {
      current.failed += 1;
    });
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

  // `startMs`, when the sound started: how long after it was offered, in whole milliseconds.
  function reportPlayed(id, startMs) {
    const gap = Number.isFinite(startMs)
      ? "&start_ms=" + Math.min(60000, Math.max(0, Math.round(startMs)))
      : "";
    send("POST", "/call/played?id=" + encodeURIComponent(id) + gap).catch(() => {});
  }

  // 4. A sound Harold offers: fetched, decoded and played through the call's context, with
  // an analyser for the bars, and reported once it has played to its end. The microphone
  // is muted from the moment it is offered.
  async function playSound(current, id) {
    const offeredAt = performance.now();
    let startedAt = null;
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
      reloadWhenUnpaired(response);
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
          reportPlayed(id, startedAt === null ? undefined : startedAt - offeredAt);
        }
      };
      source.start();
      startedAt = performance.now();
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
    const current = call;
    if (!current || !current.open) {
      // Nothing is recorded as seen yet: what the screen names before the call opens
      // plays once start() opens it and syncs again.
      return;
    }
    const before = { sound: seen.sound, speech: seen.speech };
    seen.sound = sound;
    seen.speech = speech;
    // 8. The call ended on the Mac, by a bye or for any other reason: back to the thread.
    // A call that ended because a speech model is not on the Mac says why, until the next
    // Call tap or End; one that ended before the screen showed it live ends the same way.
    const why = screen.getAttribute("data-harold-notice") || "";
    if (state !== "off") {
      current.live = true;
    } else if (current.live || why) {
      end(current);
      if (why) {
        showNotice(why);
      }
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
    send("DELETE", "/call").catch(() => {});
    end(call);
    showNotice(ENDED_BY_HIDING);
  });

  new MutationObserver(sync).observe(document.body, {
    subtree: true,
    childList: true,
    attributes: true,
    attributeFilter: [
      "data-harold-state",
      "data-harold-sound",
      "data-harold-speech",
      "data-harold-notice",
    ],
  });
  sync();
})();
