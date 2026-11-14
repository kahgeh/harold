// Dictation in the reply box. The mic asks the phone's own speech recognition (Safari's
// `webkitSpeechRecognition`) to turn speech into text, and every result is written into
// the box, so the words are the user's to check and edit before Send. The sound stays
// with the phone's recognition; nothing recorded goes to Harold. Harold hears one word for
// each dictation tap, from `POST /dictation/check`, with the tap's number and whether the
// recognition started: how dictation went on that tap.
//
// Datastar owns what shows: the hidden inputs `dictation-on` and `dictation-listening` are
// bound to the `dictation` and `listening` signals, and this script sets them, the way
// photo.js sets the `photo` signal.
"use strict";

(function () {
  const form = document.getElementById("reply");
  const box = form ? form.querySelector("textarea") : null;
  const mic = document.getElementById("dictation-mic");
  const stop = document.getElementById("dictation-stop");
  const note = document.getElementById("listening-note");
  const noteText = document.getElementById("listening-text");
  const onInput = document.getElementById("dictation-on");
  const listeningInput = document.getElementById("dictation-listening");
  if (!form || !box || !mic || !stop || !note || !noteText || !onInput || !listeningInput) {
    return;
  }

  const STORAGE_KEY = "harold-dictation";
  const QUIET_MS = 4000;
  const NOTICE_MS = 3000;
  // The longest wait for the `end` of an instance that is ending.
  const END_WAIT_MS = 1000;
  // The page sends no more reports than this for each load.
  const TAP_MAX = 100;
  const STOPPED = "Dictation stopped. Tap the mic again.";
  // The browser's error names, and the words the check carries for them.
  const ERROR_RESULTS = {
    "not-allowed": "not_allowed",
    "service-not-allowed": "service_not_allowed",
    network: "network",
    "no-speech": "no_speech",
    "audio-capture": "audio_capture",
    aborted: "aborted",
  };

  // One instance is active at a time. `ending` is the one that was stopped, aborted or
  // failed, kept until its `end` event or END_WAIT_MS. `waiting` is a tap that waits for it.
  let tapCount = 0;
  let active = null;
  let ending = null;
  let waiting = null;
  let endTimer = null;
  let quietTimer = null;
  let noticeTimer = null;

  // Datastar's own module, the one the page loads: its signals and its @post.
  const datastar = import("/static/datastar-1.0.2.js");

  // Sets `signals`, then sends `include`d signals with Datastar's @post (or `action`). Resolves
  // to the HTTP status when Datastar announced an error for the request, else 200; rejects when
  // the request could not be sent. A request is never retried, and it stays open while the page hides (the
  // default would abort it and send it again on return), so one failed post is one
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

  // Tells Harold how one dictation tap went. It may go as the page hides, so the request
  // stays open then.
  function report(result, tap, started) {
    post(null, "/dictation/check", { result, tap, started }, /^(result|tap|started)$/, {
      request: { openWhenHidden: true },
    }).catch(() => {});
  }

  // Ends a tap with one report: `ok` when a result came, else `word`. A second call does nothing.
  function settle(t, word) {
    if (t.settled) {
      return;
    }
    t.settled = true;
    if (t.n <= TAP_MAX) {
      report(t.results ? "ok" : word, t.n, t.started);
    }
  }

  // Sets a signal through its hidden input; Datastar follows the input event.
  function setFlag(input, on) {
    input.value = on ? "1" : "";
    input.dispatchEvent(new Event("input", { bubbles: true }));
  }

  function showNote(text, notice) {
    noteText.textContent = text;
    note.classList.toggle("notice", notice);
    note.hidden = false;
  }

  function hideNote() {
    clearTimeout(noticeTimer);
    noticeTimer = null;
    note.hidden = true;
    note.classList.remove("notice");
  }

  function notice(text) {
    clearTimeout(noticeTimer);
    showNote(text, true);
    noticeTimer = setTimeout(hideNote, NOTICE_MS);
  }

  // Writes the words so far into the box and tells Datastar.
  function setWords(words, interim) {
    box.value = words;
    box.classList.toggle("interim", interim);
    box.dispatchEvent(new Event("input", { bubbles: true }));
  }

  function armQuiet(inst) {
    clearTimeout(quietTimer);
    quietTimer = setTimeout(() => stopInstance(inst), QUIET_MS);
  }

  // The listening is over: the last words stay, in the normal colour, and Send shows.
  function finishUi() {
    clearTimeout(quietTimer);
    quietTimer = null;
    box.classList.remove("interim");
    setFlag(listeningInput, false);
    if (!noticeTimer) {
      hideNote();
    }
  }

  // The instance is ending: keep it until its `end` event, or for END_WAIT_MS.
  function moveToEnding(inst) {
    if (active === inst) {
      active = null;
    }
    ending = inst;
    clearTimeout(endTimer);
    endTimer = setTimeout(() => ended(inst), END_WAIT_MS);
  }

  // The `end` event of an instance, or the wait for it ran out.
  function ended(inst) {
    settle(inst.t, inst.t.pageEnded ? "page_ended" : "no_result");
    if (inst.uiOn) {
      inst.uiOn = false;
      finishUi();
    }
    if (active === inst) {
      active = null;
    }
    if (ending !== inst) {
      return;
    }
    ending = null;
    clearTimeout(endTimer);
    endTimer = null;
    const next = waiting;
    waiting = null;
    if (next) {
      begin(next);
    }
  }

  // Asks the instance to finish; the words still to come are written, and its `end` follows.
  function stopInstance(inst) {
    if (active !== inst) {
      return;
    }
    inst.t.pageEnded = true;
    inst.write = true;
    try {
      inst.r.stop();
    } catch (error) {
      // Already over.
    }
    clearTimeout(quietTimer);
    moveToEnding(inst);
  }

  // Ends the listening at once; the words so far stay in the box. A tap that waits is dropped.
  function abort() {
    if (waiting) {
      settle(waiting, "page_ended");
      waiting = null;
    }
    const inst = active || ending;
    if (!inst || (inst.t.settled && !inst.uiOn)) {
      return;
    }
    inst.t.pageEnded = true;
    inst.write = false;
    try {
      inst.r.abort();
    } catch (error) {
      // Already over.
    }
    settle(inst.t, "page_ended");
    inst.uiOn = false;
    finishUi();
    if (active === inst) {
      moveToEnding(inst);
    }
  }

  function begin(t) {
    let r;
    try {
      r = new Recognition();
    } catch (error) {
      settle(t, "start_failed");
      notice(STOPPED);
      return;
    }
    const inst = { r, t, write: false, uiOn: false };
    r.lang = navigator.language;
    r.interimResults = true;
    r.continuous = true;
    r.onresult = (event) => {
      if (active !== inst && !(ending === inst && inst.write)) {
        return;
      }
      const results = Array.from(event.results);
      const words = results
        .map((result) => result[0].transcript.trim())
        .filter(Boolean)
        .join(" ");
      setWords(words, results.some((result) => !result.isFinal));
      t.results = true;
      if (active === inst) {
        armQuiet(inst);
      }
    };
    r.onerror = (event) => {
      const error = event.error;
      // Our own abort gives an `aborted` error: it is not a failure.
      if (t.settled || (t.pageEnded && error === "aborted")) {
        return;
      }
      settle(t, ERROR_RESULTS[error] || "other_error");
      if (active !== inst) {
        return;
      }
      if (error === "not-allowed" || error === "service-not-allowed") {
        notice("Dictation isn't available in this app.");
      } else if (error === "network") {
        notice("Dictation needs a connection.");
      } else if (error === "audio-capture") {
        notice("The microphone gave no sound.");
      } else if (!ERROR_RESULTS[error] || error === "aborted") {
        notice(STOPPED);
      }
      inst.uiOn = false;
      finishUi();
      // Some builds give no `end` after an error.
      moveToEnding(inst);
    };
    // The quiet timer waits for the service to listen, so the first-use prompts for the
    // microphone and speech recognition do not count against it.
    r.onstart = () => {
      t.started = true;
      if (active === inst) {
        armQuiet(inst);
      }
    };
    r.onend = () => ended(inst);
    active = inst;
    try {
      r.start();
    } catch (error) {
      active = null;
      settle(t, "start_failed");
      notice(STOPPED);
      return;
    }
    inst.uiOn = true;
    clearTimeout(noticeTimer);
    noticeTimer = null;
    showNote("Listening", false);
    setFlag(listeningInput, true);
  }

  // A tap on the mic. A tap while an instance is ending waits for its end.
  function tap() {
    if (active || waiting) {
      return;
    }
    tapCount += 1;
    const t = { n: tapCount, started: false, results: false, pageEnded: false, settled: false };
    if (ending) {
      waiting = t;
      return;
    }
    begin(t);
  }

  const Recognition = window.SpeechRecognition || window.webkitSpeechRecognition;
  if (!Recognition) {
    report("missing", 0, false);
    return;
  }
  try {
    // Clears a hide that an older page remembered after a refusal.
    localStorage.removeItem(STORAGE_KEY);
  } catch (error) {
    // A private window may refuse; there is nothing to clear then.
  }
  setFlag(onInput, true);

  mic.addEventListener("click", tap);
  stop.addEventListener("click", () => {
    if (active) {
      stopInstance(active);
    }
  });
  form.addEventListener("submit", abort);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") {
      abort();
    }
  });
  const call = document.getElementById("call-start");
  if (call) {
    call.addEventListener("click", abort);
  }
})();
