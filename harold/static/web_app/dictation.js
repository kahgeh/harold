// Dictation in the reply box. The mic asks the phone's own speech recognition (Safari's
// `webkitSpeechRecognition`) to turn speech into text, and every result is written into
// the box, so the words are the user's to check and edit before Send. The sound stays
// with the phone's recognition; nothing recorded goes to Harold. Harold hears only one
// word per page load, from `POST /dictation/check`: whether dictation worked here.
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
  // The browser's error names, and the words the check carries for them.
  const ERROR_RESULTS = {
    "not-allowed": "not_allowed",
    "service-not-allowed": "service_not_allowed",
    network: "network",
    "no-speech": "no_speech",
    "audio-capture": "audio_capture",
  };

  let reported = false;
  let recognition = null;
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

  // Tells Harold the first thing known about dictation on this page, and nothing after. It
  // may go as the page hides, so the request stays open then.
  function report(result) {
    if (reported) {
      return;
    }
    reported = true;
    post(null, "/dictation/check", { result }, /^result$/, {
      request: { openWhenHidden: true },
    }).catch(() => {});
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

  function armQuiet(r) {
    clearTimeout(quietTimer);
    quietTimer = setTimeout(() => {
      if (recognition === r) {
        r.stop();
      }
    }, QUIET_MS);
  }

  // The recognition is over: the last words stay, in the normal colour, and Send shows.
  function finish() {
    clearTimeout(quietTimer);
    quietTimer = null;
    recognition = null;
    box.classList.remove("interim");
    setFlag(listeningInput, false);
    if (!noticeTimer) {
      hideNote();
    }
  }

  // Ends the recognition at once; the words so far stay in the box.
  function abort() {
    const r = recognition;
    if (!r) {
      return;
    }
    recognition = null;
    try {
      r.abort();
    } catch (error) {
      // Already over.
    }
    finish();
  }

  function start() {
    if (recognition) {
      return;
    }
    let r;
    try {
      r = new Recognition();
    } catch (error) {
      return;
    }
    r.lang = navigator.language;
    r.interimResults = true;
    r.continuous = true;
    r.onresult = (event) => {
      if (recognition !== r) {
        return;
      }
      const results = Array.from(event.results);
      const words = results
        .map((result) => result[0].transcript.trim())
        .filter(Boolean)
        .join(" ");
      setWords(words, results.some((result) => !result.isFinal));
      report("ok");
      armQuiet(r);
    };
    r.onerror = (event) => {
      if (recognition !== r) {
        return;
      }
      const error = event.error;
      if (ERROR_RESULTS[error]) {
        report(ERROR_RESULTS[error]);
      }
      if (error === "not-allowed" || error === "service-not-allowed") {
        notice("Dictation isn't available in this app.");
      } else if (error === "network") {
        notice("Dictation needs a connection.");
      } else if (error === "audio-capture") {
        notice("The microphone gave no sound.");
      }
      // Some builds give no `end` after an error.
      finish();
    };
    // The quiet timer waits for the service to listen, so the first-use prompts for the
    // microphone and speech recognition do not count against it.
    r.onstart = () => {
      if (recognition === r) {
        armQuiet(r);
      }
    };
    r.onend = () => {
      if (recognition === r) {
        finish();
      }
    };
    recognition = r;
    try {
      r.start();
    } catch (error) {
      recognition = null;
      return;
    }
    clearTimeout(noticeTimer);
    noticeTimer = null;
    showNote("Listening", false);
    setFlag(listeningInput, true);
  }

  const Recognition = window.SpeechRecognition || window.webkitSpeechRecognition;
  if (!Recognition) {
    report("missing");
    return;
  }
  try {
    // Clears a hide that an older page remembered after a refusal.
    localStorage.removeItem(STORAGE_KEY);
  } catch (error) {
    // A private window may refuse; there is nothing to clear then.
  }
  setFlag(onInput, true);

  mic.addEventListener("click", start);
  stop.addEventListener("click", () => {
    if (recognition) {
      recognition.stop();
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
