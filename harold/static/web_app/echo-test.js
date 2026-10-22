// The echo test: plays 15 seconds of speech through the phone while the microphone is open,
// the way a call does, and sends Harold one loudness number for every 20 ms the microphone
// heard. The sound itself never leaves the phone.
"use strict";

(function () {
  const start = document.getElementById("echo-start");
  const statusLine = document.getElementById("echo-status");
  const results = document.getElementById("echo-results");
  const summary = document.getElementById("echo-summary");
  const summaryList = document.getElementById("echo-summary-list");
  if (!start || !statusLine || !results) {
    return;
  }

  // The recorder's pieces are 16 kHz; a frame is 20 ms of them, as the speech detector's.
  const RATE = 16000;
  const FRAME = 320;
  const FRAMES_PER_SECOND = RATE / FRAME;
  const QUIET_SECONDS = 1;
  const PLAY_SECONDS = 15;
  const MAX_FRAMES = 1000;

  const done = [];

  function chosen(name) {
    const input = document.querySelector('input[name="' + name + '"]:checked');
    return input ? input.value : "";
  }

  function say(text) {
    statusLine.textContent = text;
  }

  function wait(seconds) {
    return new Promise((resolve) => setTimeout(resolve, seconds * 1000));
  }

  // The root mean square of each whole 20 ms of the recorded samples.
  function loudness(samples) {
    const frames = [];
    for (let at = 0; at + FRAME <= samples.length && frames.length < MAX_FRAMES; at += FRAME) {
      let sum = 0;
      for (let i = at; i < at + FRAME; i += 1) {
        sum += samples[i] * samples[i];
      }
      frames.push(Math.sqrt(sum / FRAME));
    }
    return frames;
  }

  // The pieces, in the order they came, as one run of samples and the context time of the
  // first of them.
  function joined(pieces) {
    const total = pieces.reduce((sum, piece) => sum + piece.samples.length, 0);
    const samples = new Int16Array(total);
    let at = 0;
    for (const piece of pieces) {
      samples.set(piece.samples, at);
      at += piece.samples.length;
    }
    return { samples, firstAt: pieces.length ? pieces[0].at : 0 };
  }

  const PLACES = { ear: "held to your ear", speaker: "on the table", headphones: "with headphones" };

  function describe(setup, kind) {
    return (kind === "silent" ? "Silent, " : "Talking, ") + PLACES[setup];
  }

  function showResult(setup, kind, text, failed) {
    results.replaceChildren(
      Object.assign(document.createElement("li"), {
        textContent: text,
        className: failed ? "fail" : "",
      }),
    );
    done.push({ text, failed });
    summaryList.replaceChildren(
      ...done.map((run) => {
        const row = document.createElement("li");
        row.textContent = run.text;
        if (run.failed) {
          row.className = "fail";
        }
        return row;
      }),
    );
    summary.hidden = false;
  }

  async function run() {
    const setup = chosen("setup");
    const kind = chosen("kind");
    const ctx = new AudioContext();
    let stream = null;
    try {
      // Start is a tap, so the page may make sound.
      await ctx.resume();
      say("Asking for the microphone…");
      stream = await navigator.mediaDevices.getUserMedia({
        audio: { echoCancellation: true, channelCount: 1 },
      });
      const settings = stream.getAudioTracks()[0].getSettings();
      await ctx.audioWorklet.addModule("/static/call-worklet.js");
      say("Getting Harold's voice…");
      const response = await fetch("/echo-test/sample");
      if (!response.ok) {
        throw new Error("sample");
      }
      const audio = await ctx.decodeAudioData(await response.arrayBuffer());

      const pieces = [];
      const recorder = new AudioWorkletNode(ctx, "harold-call-recorder");
      recorder.port.onmessage = (event) => {
        pieces.push({ at: event.data.at, samples: new Int16Array(event.data.samples) });
      };
      ctx.createMediaStreamSource(stream).connect(recorder);
      // The recorder outputs silence; joined to the speakers so the phone keeps running it.
      recorder.connect(ctx.destination);

      const seconds = Math.min(PLAY_SECONDS, audio.duration);
      const playAt = ctx.currentTime + QUIET_SECONDS;
      const analyser = ctx.createAnalyser();
      const source = ctx.createBufferSource();
      source.buffer = audio;
      source.connect(analyser);
      analyser.connect(ctx.destination);
      source.start(playAt, 0, seconds);
      say(
        kind === "silent"
          ? "Keep quiet while Harold speaks…"
          : "Harold is speaking. Talk over him now…",
      );
      await wait(QUIET_SECONDS + seconds + QUIET_SECONDS);

      const { samples, firstAt } = joined(pieces);
      const frames = loudness(samples);
      const body = {
        setup,
        kind,
        frames,
        playing_from: Math.max(0, Math.round((playAt - firstAt) * FRAMES_PER_SECOND)),
        playing_to: Math.min(frames.length, Math.round((playAt + seconds - firstAt) * FRAMES_PER_SECOND)),
        echo_cancellation:
          typeof settings.echoCancellation === "boolean" ? settings.echoCancellation : null,
        standalone:
          window.matchMedia("(display-mode: standalone)").matches || navigator.standalone === true,
        user_agent: navigator.userAgent,
      };
      say("Sending the loudness numbers to your Mac…");
      const sent = await fetch("/echo-test/runs", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      const answer = await sent.json().catch(() => ({}));
      if (sent.ok && answer.line) {
        showResult(setup, kind, answer.line, answer.verdict === "fail");
        say("Done.");
      } else {
        showResult(setup, kind, describe(setup, kind) + ": not saved. Harold did not keep this run.", true);
        say("Not saved.");
      }
    } catch {
      showResult(setup, kind, describe(setup, kind) + ": not run. The microphone, the sample or Harold could not be reached.", true);
      say("Not run.");
    } finally {
      if (stream) {
        stream.getTracks().forEach((track) => track.stop());
      }
      ctx.close().catch(() => {});
    }
  }

  start.addEventListener("click", async () => {
    start.disabled = true;
    try {
      await run();
    } finally {
      start.disabled = false;
    }
  });
})();
