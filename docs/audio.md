# Audio pipeline

Audio goes over the RDPSND static virtual channel (MS-RDPEA). The protocol layer lives in the vendored `crates/ironrdp-rdpsnd` crate; playback is handled by `wasm/src/audio.rs` and the JS side in `web/app.js` + `web/audio-worklet.js`.

## Codec negotiation

During session setup, `WasmRdpsndHandler::client_formats()` returns the list of formats the client supports, in preference order. Compressed codecs are listed first so the server prefers them:

1. **AAC** (`0xA106 / AAC_MS`) — if the browser's WebCodecs `AudioDecoder` supports `mp4a.40.2`
2. **Opus** (`0x704F`) — if the browser supports `opus`
3. **PCM** (`0x0001`) — always supported, listed last

The server picks its preferred format from the intersection. Whatever it picks, `WasmRdpsndHandler::wave()` receives the encoded audio data along with the format details (codec tag, channels, sample rate, bits per sample, and any codec-specific extradata). This gets forwarded to JS via `notify_audio_data`.

Do note that Windows RDP hosts happily negotiate Opus and AAC; xrdp typically falls back to PCM.

## JS playback engine

`window.__rdp_audio_data` is the entry point. It receives the codec tag plus the raw audio data and routes to one of two paths.

### PCM path

PCM samples arrive as 16-bit signed integers, interleaved across channels. The handler:
1. Converts the interleaved Int16 samples to per-channel Float32 arrays (`sample / 32768`).
2. Posts the Float32 arrays to the AudioWorklet via `port.postMessage`.

### Compressed path (Opus / AAC)

Compressed audio goes through a WebCodecs `AudioDecoder` (`ensureDecoder`). One decoder instance per codec tag is created lazily. For Opus, a synthesized `OpusHead` header is prepended as the `description` for `configure()` — WebCodecs needs this because raw Opus packets in RDP don't carry the Ogg container header. For AAC, the extradata from the RDPSND format record serves as the `description`.

`decode()` is fire-and-forget; decoded `AudioData` arrives in the `output` callback, gets converted to Float32, and is posted to the worklet.

Packets received before the worklet is ready are dropped silently.

## AudioWorklet (audio-worklet.js)

The worklet runs in a dedicated audio rendering thread driven by the OS audio clock. This is the key difference from a simple `AudioBufferSourceNode` approach — the clock is owned by the hardware, not a software cursor, so there is no drift.

The worklet maintains a ring buffer (a double-ended queue of Float32 chunks). `process()` is called at the device's render quantum (128 frames). It:
1. Drains from the front of the queue to fill the output buffer, resampling from the incoming sample rate to the `AudioContext.sampleRate` with linear interpolation and persistent phase.
2. If the queue is empty (underrun), outputs silence for that quantum.
3. If the queue has grown past the latency cap (0.12 s target, 0.04 s target latency), drops the oldest samples to snap latency back (`catchUp`).

The latency cap is the fix for the "audio plays seconds behind video and never recovers" failure mode that happens when a server bursts audio after a UI sound event.

## Volume

`window.__rdp_audio_volume(left, right)` sets the gain on the `AudioContext`'s `GainNode`. Values are in the range 0–0xFFFF (from the RDPSND Volume PDU), normalized to 0.0–1.0 using the larger of left and right channels.

## Connection between WASM and the worklet

The JS side initializes the `AudioContext` and loads the worklet once, on the first audio packet (lazy init). Subsequent packets just post messages. The worklet is not torn down between reconnects — it is reused with the ring buffer drained.

## Vendored crate vs upstream

The `crates/ironrdp-rdpsnd` crate is a fork of the upstream protocol crate. The differences that matter:

- **Permissive format matching** — upstream requires an exact match on codec tag, channel count, sample rate, and bit depth. The fork accepts any format whose wave format tag matches, regardless of the other fields. This is necessary for xrdp which sends non-standard sample rates.
- **Two-phase WaveInfo handling** — xrdp sends `WaveInfo` and the wave body as two separate SVC messages; the fork buffers the first and stitches them. Windows uses `Wave2` (single-message) and never hits this path.
- **Tolerates missing Training PDU** — upstream stops if the server skips Training; the fork warns and continues. The spec says SHOULD for Training, not MUST.

These changes are in the protocol crate only. The playback layer (`audio.rs`, `app.js`) is entirely our own code with no upstream equivalent.
