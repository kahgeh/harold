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
  // The page sends no more reports than this for each load.
  const TAP_MAX = 100;
  const STOPPED = "Dictation stopped. Tap the mic again.";
  const TAP_AGAIN = "Tap the mic again.";
  // The browser's error names, and the words the check carries for them.
  const ERROR_RESULTS = {
    "not-allowed": "not_allowed",
    "service-not-allowed": "service_not_allowed",
    network: "network",
    "no-speech": "no_speech",
    "audio-capture": "audio_capture",
    aborted: "aborted",
  };

  // `current` is the newest instance, until its `end` event or until a tap or the page
  // retires it. `lingering` is a retired instance that has not fired `end` yet. A tap starts
  // its instance inside the tap's own handler, never from an event or a timer, because iOS
  // Safari may refuse a start outside the user's gesture.
  let tapCount = 0;
  let current = null;
  let lingering = null;
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

  // Whether the instance is listening: not stopping and not failed.
  function listening(inst) {
    return inst === current && !inst.stopping && !inst.errored;
  }

  // The `end` event of an instance. A retired instance has nothing left to do.
  function ended(inst) {
    if (lingering === inst) {
      lingering = null;
    }
    if (inst !== current) {
      return;
    }
    current = null;
    settle(inst.t, inst.t.pageEnded ? "page_ended" : "no_result");
    if (inst.uiOn) {
      inst.uiOn = false;
      finishUi();
    }
  }

  // Asks the instance to finish; the words still to come are written, and its `end` follows.
  function stopInstance(inst) {
    if (!listening(inst)) {
      return;
    }
    inst.t.pageEnded = true;
    inst.stopping = true;
    try {
      inst.r.stop();
    } catch (error) {
      // Already over.
    }
    clearTimeout(quietTimer);
  }

  // Ends the instance at once and for good: its words so far stay in the box, its tap ends
  // with one report, and its late events are ignored.
  function retire(inst) {
    inst.t.pageEnded = true;
    inst.write = false;
    try {
      inst.r.abort();
    } catch (error) {
      // Already over.
    }
    settle(inst.t, "page_ended");
    if (inst.uiOn) {
      inst.uiOn = false;
      finishUi();
    }
    if (current === inst) {
      current = null;
      lingering = inst;
    }
  }

  // Ends the listening at once, for Send, the page hiding and a call.
  function abort() {
    if (current) {
      retire(current);
    }
  }

  function begin(t, hadOld) {
    let r;
    try {
      r = new Recognition();
    } catch (error) {
      settle(t, "start_failed");
      notice(STOPPED);
      return;
    }
    const inst = { r, t, write: true, stopping: false, errored: false, uiOn: false };
    r.lang = navigator.language;
    r.interimResults = true;
    r.continuous = true;
    r.onresult = (event) => {
      if (inst !== current || !inst.write) {
        return;
      }
      const results = Array.from(event.results);
      const words = results
        .map((result) => result[0].transcript.trim())
        .filter(Boolean)
        .join(" ");
      setWords(words, results.some((result) => !result.isFinal));
      t.results = true;
      if (listening(inst)) {
        armQuiet(inst);
      }
    };
    r.onerror = (event) => {
      const error = event.error;
      // The error of an instance that the page retired, or of an `abort` it asked for, is not
      // a failure.
      if (t.settled || (t.pageEnded && error === "aborted")) {
        return;
      }
      settle(t, ERROR_RESULTS[error] || "other_error");
      if (inst !== current) {
        return;
      }
      inst.errored = true;
      inst.write = false;
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
      // Some builds give no `end` after an error. The next tap retires this instance.
    };
    // The quiet timer waits for the service to listen, so the first-use prompts for the
    // microphone and speech recognition do not count against it.
    r.onstart = () => {
      t.started = true;
      if (listening(inst)) {
        armQuiet(inst);
      }
    };
    r.onend = () => ended(inst);
    current = inst;
    try {
      r.start();
    } catch (error) {
      current = null;
      settle(t, "start_failed");
      notice(hadOld ? TAP_AGAIN : STOPPED);
      return;
    }
    inst.uiOn = true;
    clearTimeout(noticeTimer);
    noticeTimer = null;
    showNote("Listening", false);
    setFlag(listeningInput, true);
  }

  // A tap on the mic. It retires an older instance that is still there (it asks it to abort)
  // and starts the new one in this same handler.
  function tap() {
    if (current && listening(current)) {
      return;
    }
    tapCount += 1;
    const t = { n: tapCount, started: false, results: false, pageEnded: false, settled: false };
    const old = current || lingering;
    if (old) {
      retire(old);
    }
    begin(t, Boolean(old));
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
    if (!current) {
      return;
    }
    // A second tap on stop, with no `end` after the first, ends it at once.
    if (current.stopping) {
      retire(current);
    } else {
      stopInstance(current);
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
