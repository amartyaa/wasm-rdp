// RDP audio playback worklet.
//
// Pull-based playback that mirrors how the native IronRDP backend uses cpal:
// the audio device's render callback (process()) pulls exactly the samples it
// needs each quantum from an internal queue. The device clock owns timing, so
// there is no hand-maintained "nextPlayTime" cursor to drift — the failure mode
// that made the old BufferSource scheduler lag behind and never recover.
//
// Incoming PCM arrives at the RDP source sample rate; we resample continuously
// to the AudioContext rate using linear interpolation with persistent phase
// across messages (no per-fragment discontinuity). A small jitter buffer with
// drop-to-catch-up keeps latency bounded when the server bursts audio.

class RdpAudioProcessor extends AudioWorkletProcessor {
    constructor(options) {
        super();
        const opts = (options && options.processorOptions) || {};
        this.channels = Math.max(1, opts.channels || 2);
        this.sourceRate = opts.sourceRate || sampleRate;
        // Input source-samples consumed per output frame.
        this.ratio = this.sourceRate / sampleRate;

        // Queue of decoded chunks. Each entry is an array of per-channel
        // Float32Array, all of equal length (frames). queue[0] is current.
        this.queue = [];
        this.frameIdx = 0;       // read cursor into queue[0]
        this.queuedFrames = 0;   // total remaining source frames across queue

        // Continuous resampler state.
        this.phase = 0;
        this.cur = new Float32Array(this.channels);
        this.nxt = new Float32Array(this.channels);
        this.primed = false;

        // Jitter buffer targets, in seconds of source audio.
        this.targetLatency = 0.04; // steady-state target (~40 ms)
        this.maxLatency = 0.12;    // above this we drop oldest samples to catch up

        this.port.onmessage = (e) => {
            const d = e.data;
            if (d.type === 'pcm') {
                this.queue.push(d.channelData);
                this.queuedFrames += d.channelData[0].length;
                this.catchUp();
            } else if (d.type === 'reset') {
                this.queue = [];
                this.frameIdx = 0;
                this.queuedFrames = 0;
                this.phase = 0;
                this.primed = false;
            }
        };
    }

    // Drop the oldest buffered samples when latency grows past maxLatency,
    // snapping back down to targetLatency. This is the catch-up the old engine
    // never had — without it a server burst pushes audio permanently behind.
    catchUp() {
        const maxFrames = Math.floor(this.maxLatency * this.sourceRate);
        if (this.queuedFrames <= maxFrames) return;

        const targetFrames = Math.floor(this.targetLatency * this.sourceRate);
        let drop = this.queuedFrames - targetFrames;

        while (drop > 0 && this.queue.length > 0) {
            const cd = this.queue[0];
            const avail = cd[0].length - this.frameIdx;
            if (avail <= drop) {
                drop -= avail;
                this.queuedFrames -= avail;
                this.queue.shift();
                this.frameIdx = 0;
            } else {
                this.frameIdx += drop;
                this.queuedFrames -= drop;
                drop = 0;
            }
        }
        // Re-prime the interpolator after a discontinuous jump.
        this.primed = false;
    }

    // Pop the next source frame (all channels) into `dst`. Returns false on underrun.
    pullSourceFrame(dst) {
        if (this.queue.length === 0) return false;
        const cd = this.queue[0];
        for (let ch = 0; ch < this.channels; ch++) {
            dst[ch] = cd[ch][this.frameIdx];
        }
        this.frameIdx++;
        this.queuedFrames--;
        if (this.frameIdx >= cd[0].length) {
            this.queue.shift();
            this.frameIdx = 0;
        }
        return true;
    }

    process(_inputs, outputs) {
        const out = outputs[0];
        if (!out || out.length === 0) return true;
        const frames = out[0].length;

        for (let i = 0; i < frames; i++) {
            if (!this.primed) {
                if (!this.pullSourceFrame(this.cur)) {
                    this.silence(out, i, frames);
                    return true;
                }
                if (!this.pullSourceFrame(this.nxt)) {
                    this.nxt.set(this.cur);
                }
                this.phase = 0;
                this.primed = true;
            }

            // Advance to the source frames that bracket the current phase.
            while (this.phase >= 1) {
                this.cur.set(this.nxt);
                if (!this.pullSourceFrame(this.nxt)) {
                    // Underrun mid-stream: hold the last sample and re-prime next tick.
                    this.primed = false;
                    this.phase -= 1;
                    break;
                }
                this.phase -= 1;
            }

            const p = this.phase;
            for (let ch = 0; ch < out.length; ch++) {
                const sc = ch < this.channels ? ch : this.channels - 1; // mono -> dup
                out[ch][i] = this.cur[sc] * (1 - p) + this.nxt[sc] * p;
            }
            this.phase += this.ratio;
        }

        return true;
    }

    silence(out, from, frames) {
        for (let ch = 0; ch < out.length; ch++) {
            for (let i = from; i < frames; i++) out[ch][i] = 0;
        }
    }
}

registerProcessor('rdp-audio', RdpAudioProcessor);
