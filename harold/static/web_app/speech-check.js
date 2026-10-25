// The speech check: a one-off page, run once on the user's iPhone in the installed app. It
// tries Safari's speech recognition (`webkitSpeechRecognition`, as dictation.js and a phone
// call use it) in the ways a call needs, and plays Harold's own voice the way the call does.
// What it keeps and sends are yes or no answers, counts, milliseconds and the recognizer's
// error names. A word the user says is compared on the phone and thrown away: it is never
// stored and never sent. The results are held in `localStorage` and posted to
// `POST /speech-check/runs`, through Datastar's @post, once the last check is done, again and again until Harold takes
// them, because check 0 runs with the network off.
"use strict";

(function () {
  const checksList = document.getElementById("speech-checks");
  const title = document.getElementById("speech-title");
  const text = document.getElementById("speech-text");
  const networkLine = document.getElementById("speech-network");
  const go = document.getElementById("speech-go");
  const statusLine = document.getElementById("speech-status");
  if (!checksList || !title || !text || !networkLine || !go || !statusLine) {
    return;
  }

  const STORAGE_KEY = "harold-speech-check";
  const SENTENCE_URL = "/static/check-sentence.wav";
  const RUNS_URL = "/speech-check/runs";
  // The words of check-sentence.wav, for the count in check 5.
  const SENTENCE_WORDS = ["hello", "this", "is", "harold", "your", "phone", "checking", "my", "voice"];
  const NUMBER_WORDS = ["seven", "eight", "nine", "7", "8", "9"];
  // Three short sentences for check 4, shown one after another.
  const READ_SENTENCES = [
    "The weather is lovely today.",
    "Please send me the report tomorrow.",
    "Call me when you get there.",
  ];

  // The call's own wait between the tap and the start of listening.
  const CALL_DELAY_MS = 2500;
  // How long each check listens at most, and how long it goes on after its first result.
  const LISTEN_MAX_MS = 9000;
  const AFTER_RESULT_MS = 2500;
  const START_WAIT_MS = 3000;
  const RESTARTS = 5;
  const RESTART_EVERY_MS = 2000;
  const READ_MS = 4500;
  const PAUSE_MS = 2000;
  // A final that comes this long after the last pause still counts for the last sentence.
  const FINAL_GRACE_MS = 1500;
  const BEFORE_PLAYBACK_MS = 1000;
  const AFTER_PLAYBACK_MS = 500;
  const PLAYBACK_SLACK_MS = 3000;
  const MAX_MS = 10 * 60 * 1000;
  const RETRY_MIN_MS = 2000;
  const RETRY_MAX_MS = 10000;

  // The error names Harold accepts; any other name goes as `other`.
  const KNOWN_ERRORS = [
    "no-speech",
    "aborted",
    "audio-capture",
    "network",
    "not-allowed",
    "service-not-allowed",
    "bad-grammar",
    "language-not-supported",
  ];

  const Recognition = window.SpeechRecognition || window.webkitSpeechRecognition;
  // The run so far, as it is posted.
  const run = {
    standalone:
      window.matchMedia("(display-mode: standalone)").matches || navigator.standalone === true,
  };
  let current = 0;

  function wait(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  function elapsed(since) {
    return Math.min(MAX_MS, Math.max(0, Math.round(performance.now() - since)));
  }

  function errorName(name) {
    return KNOWN_ERRORS.includes(name) ? name : "other";
  }

  function say(line) {
    statusLine.textContent = line;
  }

  function showCheck(check, state) {
    const item = checksList.querySelector('[data-check="' + check + '"] span');
    if (item) {
      item.textContent = state;
    }
  }

  function hold() {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify({ run, step: current, done: current >= STEPS.length }));
    } catch (error) {
      // A private window may refuse; the run still posts from memory.
    }
  }

  function words(result) {
    return Array.from(result[0].transcript.toLowerCase().matchAll(/[a-z0-9]+/g), (m) => m[0]);
  }

  // One recognition, started by `listen`, that tells `on` what happened. The words of a
  // result reach only `on.result`, which counts them and keeps nothing.
  function open(on) {
    const r = new Recognition();
    r.lang = navigator.language;
    r.interimResults = true;
    r.continuous = true;
    const state = { closing: false, started: false, error: null, ended: false };
    r.onstart = () => {
      state.started = true;
      if (on.start) on.start();
    };
    r.onresult = (event) => {
      if (on.result) on.result(event);
    };
    r.onerror = (event) => {
      // The page's own stop or abort is not what the recognizer did.
      if (state.closing) {
        return;
      }
      state.error = errorName(event.error);
      if (on.error) on.error(state.error);
    };
    r.onend = () => {
      state.ended = true;
      if (on.end) on.end();
    };
    return {
      state,
      // Err with the name `start-failed` when `start()` throws.
      start() {
        try {
          r.start();
          return true;
        } catch (error) {
          state.error = "start-failed";
          return false;
        }
      },
      close() {
        state.closing = true;
        try {
          r.abort();
        } catch (error) {
          // Already over.
        }
      },
    };
  }

  // Resolves once `until()` is true or `ms` have passed, checking every 100 ms.
  async function waitUntil(until, ms) {
    const from = performance.now();
    while (!until() && performance.now() - from < ms) {
      await wait(100);
    }
  }

  // Listens until the recognizer errors or ends, `AFTER_RESULT_MS` after the first result
  // that `counts` accepts, or `LISTEN_MAX_MS`. Starts at once, from the tap that called it.
  // Returns { started, results, firstResultMs, error }.
  async function listen(counts) {
    const out = { started: false, results: false, firstResultMs: null, error: null };
    const from = performance.now();
    let resultAt = null;
    const rec = open({
      start: () => {
        out.started = true;
      },
      result: (event) => {
        out.results = true;
        if (resultAt === null && counts(event)) {
          resultAt = performance.now();
          out.firstResultMs = elapsed(from);
        }
      },
    });
    if (!rec.start()) {
      out.error = rec.state.error;
      return out;
    }
    await waitUntil(
      () =>
        rec.state.error !== null ||
        rec.state.ended ||
        (resultAt !== null && performance.now() - resultAt >= AFTER_RESULT_MS),
      LISTEN_MAX_MS,
    );
    out.error = rec.state.error;
    out.started = rec.state.started;
    rec.close();
    return out;
  }

  function anyResult() {
    return true;
  }

  function saveListen(prefix, out, withTime) {
    run[prefix + "_started"] = out.started;
    run[prefix + "_results"] = out.results;
    if (withTime && out.firstResultMs !== null) {
      run[prefix + "_first_result_ms"] = out.firstResultMs;
    }
    if (out.error) {
      run[prefix + "_error"] = out.error;
    }
  }

  // Check 0, with the network off, and check 1, inside a tap.
  async function check0() {
    // Whether the phone said it was offline when the check started: without it, a result
    // from a phone that was still online would read as recognition working on the device.
    run.check0_offline = navigator.onLine === false;
    say("Listening. Say “ten eleven twelve”.");
    saveListen("check0", await listen(anyResult), true);
  }

  async function check1() {
    say("Listening. Say “one two three”.");
    saveListen("check1", await listen(anyResult), true);
  }

  // Check 2: the tap posts to Harold, as a call does, and listening starts 2.5 s after it.
  async function check2() {
    say("Wait for it…");
    const tapped = performance.now();
    // The round trip a call makes before its ring: one request to Harold.
    fetch(SENTENCE_URL, { cache: "no-store" }).catch(() => {});
    await wait(Math.max(0, CALL_DELAY_MS - (performance.now() - tapped)));
    say("Say “four five six” now.");
    saveListen("check2", await listen(anyResult), false);
  }

  // Check 3: recognition is ended and started again five times, 2 s apart, with no tap
  // between. The first start, in the tap, is only the warm-up.
  async function check3() {
    say("Count aloud: one, two, three, and on…");
    run.check3_started = 0;
    run.check3_errors = [];
    let rec = open({});
    rec.start();
    await waitUntil(() => rec.state.started || rec.state.error !== null, START_WAIT_MS);
    for (let n = 0; n < RESTARTS; n += 1) {
      const attempt = performance.now();
      rec.close();
      await wait(400);
      rec = open({});
      if (rec.start()) {
        await waitUntil(() => rec.state.started || rec.state.error !== null, START_WAIT_MS);
      }
      if (rec.state.started) {
        run.check3_started += 1;
      } else {
        run.check3_errors.push(rec.state.error || "other");
      }
      await wait(Math.max(0, RESTART_EVERY_MS - (performance.now() - attempt)));
    }
    rec.close();
  }

  // Check 4: three sentences, each read aloud and then 2 s of quiet. One continuous
  // recognition runs throughout; each result event says what became final and when.
  async function check4() {
    run.check4_results = false;
    run.check4_final_at_ms = [];
    run.check4_gap_ms = [];
    const events = [];
    let lastInterim = null;
    const seenFinal = new Set();
    const rec = open({
      result: (event) => {
        const now = performance.now();
        run.check4_results = true;
        for (let i = event.resultIndex; i < event.results.length; i += 1) {
          if (event.results[i].isFinal && !seenFinal.has(i)) {
            seenFinal.add(i);
            events.push({ at: now, gap: lastInterim === null ? 0 : now - lastInterim });
          } else if (!event.results[i].isFinal) {
            lastInterim = now;
          }
        }
      },
    });
    if (!rec.start()) {
      run.check4_error = rec.state.error;
      return;
    }
    const starts = [];
    for (const sentence of READ_SENTENCES) {
      starts.push(performance.now());
      say("Read aloud: “" + sentence + "”");
      await wait(READ_MS);
      say("Pause. Stay quiet.");
      await wait(PAUSE_MS);
    }
    await wait(FINAL_GRACE_MS);
    if (rec.state.error) {
      run.check4_error = rec.state.error;
    }
    rec.close();
    starts.forEach((begin, k) => {
      const end = k + 1 < starts.length ? starts[k + 1] : performance.now();
      const final = events.find((event) => event.at >= begin && event.at < end);
      run.check4_final_at_ms.push(final ? Math.min(MAX_MS, Math.round(final.at - begin)) : null);
      run.check4_gap_ms.push(final ? Math.min(MAX_MS, Math.round(final.gap)) : null);
    });
  }

  // Checks 5 and 6, one tap: recognition runs while Harold's voice plays, through the
  // call's own path (fetch, decodeAudioData, a buffer source, an analyser, the
  // destination); then the page asks for “seven eight nine”.
  async function check5and6() {
    const AudioContextClass = window.AudioContext || window.webkitAudioContext;
    // Created in the tap, as the call's is.
    const ctx = new AudioContextClass();
    ctx.resume().catch(() => {});
    let playing = false;
    let lostDuringPlayback = false;
    const heard = new Set();
    const on = {
      result: (event) => {
        if (!playing) {
          return;
        }
        for (let i = 0; i < event.results.length; i += 1) {
          words(event.results[i]).forEach((word) => heard.add(word));
        }
      },
      error: () => {
        if (playing) lostDuringPlayback = true;
      },
      end: () => {
        if (playing) lostDuringPlayback = true;
      },
    };
    const rec = open(on);
    say("Stay quiet while Harold speaks.");
    run.check5_started = false;
    run.check5_kept_running = false;
    run.check5_playback_ok = false;
    run.check5_words_heard = 0;
    let audio = null;
    const loading = fetch(SENTENCE_URL, { cache: "no-store" })
      .then((response) => (response.ok ? response.arrayBuffer() : Promise.reject(response)))
      .then((bytes) => ctx.decodeAudioData(bytes))
      .then((buffer) => {
        audio = buffer;
      })
      .catch(() => {});
    if (rec.start()) {
      await waitUntil(() => rec.state.started || rec.state.error !== null, START_WAIT_MS);
    }
    run.check5_started = rec.state.started;
    await loading;
    await wait(BEFORE_PLAYBACK_MS);
    if (audio) {
      let ended = false;
      const analyser = ctx.createAnalyser();
      const source = ctx.createBufferSource();
      source.buffer = audio;
      source.connect(analyser);
      analyser.connect(ctx.destination);
      source.onended = () => {
        ended = true;
      };
      playing = true;
      source.start();
      const duration = Math.ceil(audio.duration * 1000);
      await waitUntil(() => ended, duration + PLAYBACK_SLACK_MS);
      await wait(AFTER_PLAYBACK_MS);
      playing = false;
      run.check5_playback_ok = ended && ctx.state === "running";
      try {
        source.disconnect();
      } catch (error) {
        // Already gone.
      }
    }
    run.check5_kept_running = run.check5_started && !lostDuringPlayback && !rec.state.ended;
    run.check5_words_heard = Math.min(
      20,
      SENTENCE_WORDS.filter((word) => heard.has(word)).length,
    );
    if (rec.state.error) {
      run.check5_error = rec.state.error;
    }
    heard.clear();
    ctx.close().catch(() => {});

    // Check 6: a recognition that kept running listens on, and one that stopped starts
    // again without a tap. Only results that come from here on can count.
    showCheck(5, "done");
    showCheck(6, "now");
    say("Now say “seven eight nine”.");
    let spoken = false;
    run.check6_restarted = false;
    run.check6_heard = false;
    const counts = (event) => {
      for (let i = event.resultIndex; i < event.results.length; i += 1) {
        if (words(event.results[i]).some((word) => NUMBER_WORDS.includes(word))) {
          spoken = true;
        }
      }
      return spoken;
    };
    if (rec.state.ended || rec.state.error) {
      run.check6_restarted = true;
      const out = await listen(counts);
      run.check6_heard = spoken;
      if (out.error) {
        run.check6_error = out.error;
      }
      return;
    }
    on.result = counts;
    on.error = null;
    on.end = null;
    await waitUntil(() => spoken || rec.state.error !== null || rec.state.ended, LISTEN_MAX_MS);
    if (spoken) {
      await wait(AFTER_RESULT_MS);
    }
    run.check6_heard = spoken;
    if (rec.state.error) {
      run.check6_error = rec.state.error;
    }
    rec.close();
  }

  // The steps, in order. A step's tap starts it; `checks` are the rows it fills in.
  const STEPS = [
    {
      checks: [0],
      title: "Check 0: no network",
      text: "Turn on airplane mode, or turn off Wi-Fi and cellular. Then come back to this page, tap Start and say “ten eleven twelve”.",
      network: "off",
      run: check0,
    },
    {
      checks: [],
      title: "Turn the network back on",
      text: "Now turn airplane mode off, or turn Wi-Fi and cellular back on. This page carries on when it sees the network, or tap the button.",
      network: "on",
      button: "The network is back on",
      run: null,
    },
    {
      checks: [1],
      title: "Check 1: a tap starts listening",
      text: "Tap Start and say “one two three”.",
      run: check1,
    },
    {
      checks: [2],
      title: "Check 2: a start after a delay",
      text: "Tap Start. Wait about three seconds until it says to speak, then say “four five six”.",
      run: check2,
    },
    {
      checks: [3],
      title: "Check 3: listening again with no tap",
      text: "Tap Start, then count aloud, “one, two, three”, and keep counting for about twelve seconds until the screen says done.",
      run: check3,
    },
    {
      checks: [4],
      title: "Check 4: pauses",
      text: "Tap Start. Three sentences show one after another. Read each aloud, then stay quiet while the screen says Pause.",
      run: check4,
    },
    {
      checks: [5, 6],
      title: "Checks 5 and 6: Harold's voice",
      text: "Tap Start and stay quiet while Harold speaks. When it asks, say “seven eight nine”.",
      run: check5and6,
    },
  ];

  function online() {
    return navigator.onLine !== false;
  }

  function showNetwork() {
    const step = STEPS[current];
    if (!step || !step.network) {
      networkLine.textContent = "";
      return;
    }
    if (step.network === "off") {
      networkLine.textContent = online()
        ? "The network still looks on. Switch it off before you tap Start."
        : "The network is off. Tap Start.";
    } else {
      networkLine.textContent = online()
        ? "The network is back on."
        : "Waiting for the network to come back…";
    }
  }

  function showStep() {
    const step = STEPS[current];
    if (!step) {
      return;
    }
    title.textContent = step.title;
    text.textContent = step.text;
    go.textContent = step.button || "Start";
    go.disabled = false;
    showNetwork();
    step.checks.forEach((check) => showCheck(check, "next"));
  }

  // The network came back while the page waited for it.
  function advance() {
    current += 1;
    hold();
    if (current >= STEPS.length) {
      finish();
      return;
    }
    showStep();
  }

  async function runStep() {
    const step = STEPS[current];
    go.disabled = true;
    if (!step.run) {
      advance();
      return;
    }
    step.checks.forEach((check) => showCheck(check, "now"));
    try {
      await step.run();
    } catch (error) {
      // A check that threw leaves its fields out; the run goes on.
    }
    step.checks.forEach((check) => showCheck(check, "done"));
    say(step.checks.length ? "Done." : "");
    advance();
  }

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

  // The run is the signals; Harold reads them back as the run's JSON.
  const RUN_SIGNALS = /^(standalone|check\d_[a-z_]+)$/;

  function clearHeld() {
    try {
      localStorage.removeItem(STORAGE_KEY);
    } catch (error) {
      // Nothing to clear.
    }
  }

  // Posts the run until Harold takes it. A refusal (4xx) will not change, so it stops.
  async function send() {
    let delay = RETRY_MIN_MS;
    for (;;) {
      try {
        const status = await post(statusLine, RUNS_URL, run, RUN_SIGNALS);
        if (status === 200) {
          clearHeld();
          say("Sent. Harold has your results. You can close this page.");
          return;
        }
        if (status >= 400 && status < 500) {
          say("Harold refused the results (" + status + "). Tell the chief.");
          return;
        }
      } catch (error) {
        // No network yet.
      }
      say("Couldn't reach Harold yet. Your results stay on this phone. Trying again…");
      await wait(delay);
      delay = Math.min(RETRY_MAX_MS, delay * 2);
    }
  }

  function finish() {
    title.textContent = "All done";
    text.textContent = "Thank you. That is every check.";
    networkLine.textContent = "";
    go.hidden = true;
    say("Sending the results to Harold…");
    send();
  }

  function showMissing() {
    run.check0_error = "missing";
    run.check1_error = "missing";
    run.check2_error = "missing";
    run.check4_error = "missing";
    run.check5_error = "missing";
    run.check6_error = "missing";
    current = STEPS.length;
    title.textContent = "No speech recognition";
    text.textContent = "This app has no speech recognition, so there is nothing to check.";
    go.hidden = true;
    hold();
    send();
  }

  window.addEventListener("online", () => {
    showNetwork();
    if (STEPS[current] && STEPS[current].network === "on") {
      advance();
    }
  });
  window.addEventListener("offline", showNetwork);
  go.addEventListener("click", runStep);

  // An earlier run that finished but was never taken: send it, and do not start again.
  let earlier = null;
  try {
    earlier = JSON.parse(localStorage.getItem(STORAGE_KEY));
  } catch (error) {
    earlier = null;
  }
  if (earlier && earlier.done && earlier.run && typeof earlier.run === "object") {
    Object.assign(run, earlier.run);
    current = STEPS.length;
    title.textContent = "All done";
    text.textContent = "An earlier run finished but did not reach Harold.";
    go.hidden = true;
    send();
  } else if (!Recognition) {
    showMissing();
  } else if (earlier && !earlier.done && earlier.step > 0 && earlier.step < STEPS.length) {
    // The app reloaded between checks, for example while the network was switched: carry on.
    Object.assign(run, earlier.run);
    current = earlier.step;
    STEPS.slice(0, current).forEach((step) => step.checks.forEach((check) => showCheck(check, "done")));
    showStep();
  } else {
    showStep();
  }
})();
