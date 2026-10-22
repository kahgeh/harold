// Turns the microphone into what Harold's call listens to: 16 kHz mono, little-endian
// 16-bit samples, in pieces of 4,000 (a quarter of a second). Each output sample is the
// average of the input samples its span covers, which keeps the speech and drops what
// 16 kHz cannot hold. Each piece goes to call.js with the context time of its first sample.
// When the call asks (processorOptions.report), once a second it also posts { empty: n }, the
// blocks so far that arrived with no input: a context that runs but a microphone that gives
// nothing shows as n rising, and no message at all means the worklet never ran. The echo
// test uses this worklet too, and asks for pieces only.
"use strict";

const OUTPUT_RATE = 16000;
const PIECE_SAMPLES = 4000;
const REPORT_EVERY_S = 1;

class CallRecorder extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.report = Boolean(options.processorOptions && options.processorOptions.report);
    // Input samples per output sample: 3 at 48 kHz, 2.75625 at 44.1 kHz.
    this.ratio = sampleRate / OUTPUT_RATE;
    // Input samples seen so far, and where the current output sample's span ends.
    this.seen = 0;
    this.spanEnd = this.ratio;
    this.sum = 0;
    this.count = 0;
    this.spanStart = 0;
    this.emptyBlocks = 0;
    this.nextReport = 0;
    this.newPiece();
  }

  newPiece() {
    this.piece = new DataView(new ArrayBuffer(PIECE_SAMPLES * 2));
    this.filled = 0;
    this.pieceStart = 0;
  }

  process(inputs) {
    if (this.report && currentTime >= this.nextReport) {
      this.port.postMessage({ empty: this.emptyBlocks });
      this.nextReport = currentTime + REPORT_EVERY_S;
    }
    // The first channel only: the call asked for one.
    const samples = inputs[0] && inputs[0][0];
    if (!samples) {
      this.emptyBlocks += 1;
      return true;
    }
    for (let i = 0; i < samples.length; i += 1) {
      if (this.count === 0) {
        // currentTime is the time of this block's first sample.
        this.spanStart = currentTime + i / sampleRate;
      }
      this.sum += samples[i];
      this.count += 1;
      this.seen += 1;
      if (this.seen >= this.spanEnd) {
        this.emit(this.sum / this.count, this.spanStart);
        this.sum = 0;
        this.count = 0;
        this.spanEnd += this.ratio;
      }
    }
    // Keeps the processor running while the microphone is open.
    return true;
  }

  emit(value, at) {
    if (this.filled === 0) {
      this.pieceStart = at;
    }
    const clamped = Math.max(-1, Math.min(1, value));
    this.piece.setInt16(this.filled * 2, Math.round(clamped * 32767), true);
    this.filled += 1;
    if (this.filled === PIECE_SAMPLES) {
      const buffer = this.piece.buffer;
      this.port.postMessage({ at: this.pieceStart, samples: buffer }, [buffer]);
      this.newPiece();
    }
  }
}

registerProcessor("harold-call-recorder", CallRecorder);
