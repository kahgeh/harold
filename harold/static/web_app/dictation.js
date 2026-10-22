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
  const box = form ? form.querySelector('input[type="text"]') : null;
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
  const REMEMBER_MS = 7 * 24 * 60 * 60 * 1000;
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

  // Tells Harold the first thing known about dictation on this page, and nothing after.
  function report(result) {
    if (reported) {
      return;
    }
    reported = true;
    fetch("/dictation/check", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ result }),
      keepalive: true,
    }).catch(() => {});
  }

  // Sets a signal through its hidden input; Datastar follows the input event.
  function setFlag(input, on) {
    input.value = on ? "1" : "";
    input.dispatchEvent(new Event("input", { bubbles: true }));
  }

  function failedRecently() {
    try {
      const age = Date.now() - Number(localStorage.getItem(STORAGE_KEY));
      return age >= 0 && age < REMEMBER_MS;
    } catch (error) {
      return false;
    }
  }

  function remember() {
    try {
      localStorage.setItem(STORAGE_KEY, String(Date.now()));
    } catch (error) {
      // A private window may refuse; the next page load tries again.
    }
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
        remember();
        setFlag(onInput, false);
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
  if (failedRecently()) {
    return;
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
