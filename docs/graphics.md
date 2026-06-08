# Graphics pipeline

There are two distinct paths depending on what the RDP host negotiates: RemoteFX (RFX), which is the legacy software codec, and H.264 over MS-RDPEGFX, which is the modern hardware-accelerated path. Windows hosts will use H.264 when the client advertises the RDPGFX capability; Linux/xrdp hosts only do RFX.

## Common entry point — WebSocket to IronRDP

All graphics PDUs come in over the WebSocket as binary messages. The relay server passes them through unchanged; it never parses graphics content.

On the WASM side, `framed.rs` (`WasmFramed`) buffers incoming WebSocket data and extracts whole PDUs before handing them to IronRDP. The length parsing differs by PDU type:

- **TPKT / X224 PDUs** — the 4-byte TPKT header at bytes 2–3 (big-endian) carries the total length.
- **FastPath PDUs** — identified by the low 2 bits of the first byte. Length is encoded in 1 byte if the high bit is clear, or 2 bytes (the second byte's high bit is set) otherwise. FastPath is the path graphics PDUs take during an active session.

Once a complete PDU is in the buffer, `split_to(len).freeze()` returns a zero-copy `Bytes` handle — no allocation, just a refcount on the underlying buffer — which goes straight into `active_stage.process()`.

`active_stage.process()` returns a list of `ActiveStageOutput` variants. The rendering loop in `run_session` handles two of them: `GraphicsUpdate` and `ResponseFrame`. Everything else (pointer changes, terminate) is handled inline.

---

## RemoteFX path

RemoteFX is a tile-based codec. The server divides the changed area into 64×64 tiles and sends them progressively in transmission order (left-to-right, top-to-bottom within the dirty region). IronRDP decodes each tile into the `DecodedImage` framebuffer (a single RGBA32 buffer for the full session desktop).

When `active_stage.process()` returns `GraphicsUpdate(region)`, the region is a dirty rectangle in framebuffer coordinates. The render loop unions this into a `pending` rectangle rather than painting immediately. A `requestAnimationFrame` callback fires at display refresh rate and flushes whatever has accumulated:

```
FastPath PDU arrives
  → active_stage.process()
  → GraphicsUpdate(dirty_rect)
  → union into `pending`
  → schedule rAF if not already scheduled

rAF callback fires (~16ms at 60 Hz):
  → FPS cap check (skip if last paint was too recent)
  → canvas.draw(pending_rect)
  → notify_frame() (increments FPS counter in JS)
  → clear pending
```

`canvas.draw()` in `canvas.rs` writes the dirty region from `DecodedImage` to the HTML canvas. There's a fast path for regions that span the full canvas width — those can be sliced directly from the framebuffer as a contiguous byte range without a row-by-row copy. Partial-width regions go through a persistent `rgba_buf` scratch buffer that is reused across frames to avoid per-frame allocation.

The final step is `putImageData`, which copies from WASM linear memory into the browser's canvas backing store. This copy is unavoidable with the 2D canvas API.

### Why tiles march top-to-bottom

Because tiles arrive in raster order and the rAF fires while the server is still sending, some display frames show only the first N rows of a screen change. This is a fundamental property of RFX — the server does not buffer the full frame before sending. With bounded frame-marker gating (BEGIN/END markers that IronRDP parses for the ack but which can be surfaced to gate presentation) it is possible to hold rendering until a complete frame arrives, at the cost of slightly higher latency for large screen changes.

---

## H.264 / RDPEGFX path

This path requires explicit negotiation. If the client does not advertise `SUPPORT_DYN_VC_GFX_PROTOCOL` (0x0100) in the GCC client core early-capability flags, the Windows host never opens the `Microsoft::Windows::RDS::Graphics` Dynamic Virtual Channel and nothing happens — no log, no error, just silence.

### Negotiation

1. During connection, `build_connector_config()` sets `config.support_graphics_pipeline = true` (controlled by the `enable_gfx` flag passed from JS, which reflects the H.264 toggle in settings and a WebCodecs capability probe).
2. IronRDP's connector includes the GFX flag in the GCC client core data.
3. Windows opens the RDPGFX DVC once it sees the flag.
4. Our EGFX client (`WasmGfxHandler`) advertises codec capabilities: **AVC420 only (V8.1 + V8 fallback)**. We deliberately do not advertise V10.7, which would allow AVC444. AVC444 sends two H.264 streams that need to be combined into YUV444, which WebCodecs cannot handle as a single decoder input.
5. The server confirms AVC420 and sends H.264 frames.

### AVC420 passthrough

IronRDP's `GraphicsPipelineClient` normally decodes H.264 via a synchronous `H264Decoder` trait. WebCodecs `VideoDecoder` is asynchronous — you can't implement a sync trait with it. The solution is raw passthrough: the egfx client parses the AVC420 envelope (which contains surface ID, destination rectangle, and the H.264 bitstream) but instead of decoding, calls `on_avc420_raw()` on the handler with the raw bitstream and geometry. No decoded pixels cross the WASM/JS boundary — just compressed H.264 bytes.

### AVC format to Annex B

RDP sends H.264 in AVC format: each NAL unit is prefixed with a 4-byte big-endian length. WebCodecs needs Annex B format: each NAL unit prefixed with the 4-byte start code `00 00 00 01`. The conversion happens in-place in JS (`convertAvcInPlace`) — the length value is overwritten with the start code bytes, same memory, no reallocation. The only exception is keyframes, where the SPS NAL (type 7) is parsed to derive the codec string for `VideoDecoder.configure()`.

### Codec string from SPS

`VideoDecoder.configure()` requires a codec string like `avc1.PPCCLL` where PP is the profile IDC, CC is the constraint flags, and LL is the level IDC — all from bytes 1, 2, 3 of the SPS NAL. This string is derived once per keyframe from the incoming bitstream (`spsToCodecString`), not hardcoded.

### VideoDecoder lifecycle

Each surface gets its own `VideoDecoder` instance (lazy-initialized on first frame for that surface). On `output`, the decoded `VideoFrame` is drawn to the surface canvas with `drawImage` — cheaper than `putImageData` because it avoids the CPU copy. The decoder is configured with `optimizeForLatency: true`.

### Surface routing for multi-monitor

In multi-monitor sessions, each monitor has a separate canvas in a separate window. When `__rdp_h264_data` fires, it receives the surface origin in combined desktop coordinates. `pickRenderTarget(originX, originY)` finds the monitor whose canvas contains that origin and routes `drawImage` to the right context.

### Fallback

If `enable_gfx` is false (toggle off, or the browser fails the `VideoDecoder.isConfigSupported` probe), the GFX flag is never set, the Windows host stays on RemoteFX, and the H.264 path is never activated. xrdp ignores the GFX flag entirely regardless.

---

## What the HUD shows

The "Video" row in the Performance HUD and the codec badge in the toolbar reflect `currentVideoCodec`, which starts as `RFX` and switches to `H.264` when the first AVC420 frame arrives. The badge flips on first frame; it does not fall back if the H.264 decoder stalls.
