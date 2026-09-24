# Graphics pipeline

Graphics arrive as RemoteFX (RFX), the tile-based codec. The host encodes the desktop as RFX and the browser decodes it in WASM. (Windows can also drive graphics over the MS-RDPEGFX channel — H.264 or RFX-Progressive — but that path is not implemented here; see the note at the end.)

## Common entry point — WebSocket to IronRDP

All graphics PDUs come in over the WebSocket as binary messages. The relay server passes them through unchanged; it never parses graphics content.

On the WASM side, `framed.rs` (`WasmFramed`) buffers incoming WebSocket data and extracts whole PDUs before handing them to IronRDP. The length parsing differs by PDU type:

- **TPKT / X224 PDUs** — the 4-byte TPKT header at bytes 2–3 (big-endian) carries the total length.
- **FastPath PDUs** — identified by the low 2 bits of the first byte. Length is encoded in 1 byte if the high bit is clear, or 2 bytes (the second byte's high bit is set) otherwise. FastPath is the path graphics PDUs take during an active session.

Once a complete PDU is in the buffer, `split_to(len).freeze()` returns a zero-copy `Bytes` handle — no allocation, just a refcount on the underlying buffer — which goes straight into `active_stage.process()`.

`active_stage.process()` returns a list of `ActiveStageOutput` variants. The rendering loop in `run_session` handles two of them: `GraphicsUpdate` and `ResponseFrame`. Everything else (pointer changes, terminate) is handled inline.

## RemoteFX

RemoteFX is a tile-based codec. The server divides the changed area into 64×64 tiles and sends them progressively in transmission order (left-to-right, top-to-bottom within the dirty region). IronRDP decodes each tile into the `DecodedImage` framebuffer (a single RGBA32 buffer for the full session desktop).

When `active_stage.process()` returns `GraphicsUpdate(region)`, the region is a dirty rectangle in framebuffer coordinates, and it is painted **immediately** — there is no batching layer:

```
FastPath PDU arrives
  → active_stage.process()
  → GraphicsUpdate(dirty_rect)
  → (skip entirely if egfx_active — the legacy framebuffer is black once
     EGFX owns rendering, so blitting it would stamp black over GFX output)
  → surface.draw(dirty_rect) for every monitor surface
  → notify_frame() (increments the FPS counter in JS)
```

> **There is no frame pacing, and the FPS-cap dropdown does nothing.** An earlier
> version of this document described a `pending` rectangle flushed by a
> `requestAnimationFrame` callback with an FPS-cap check. That was never built:
> there is no `request_animation_frame` call anywhere in `wasm/src/`, and the
> cap threaded in from the UI lands on the unused `_fps_cap` in `run_session`.
>
> The trade-off this leaves: IronBridge is *faster to first pixel* than an
> rAF-batched client, but has no overdraw control and cannot trade frame rate
> for bandwidth. Adding pacing must preserve the latency win — see
> `CITRIX_COMPARISON.md` §7 T0.2.

`canvas.draw()` in `canvas.rs` writes the dirty region from `DecodedImage` to the HTML canvas. There's a fast path for regions that span the full canvas width — those can be sliced directly from the framebuffer as a contiguous byte range without a row-by-row copy. Partial-width regions go through a persistent `rgba_buf` scratch buffer that is reused across frames to avoid per-frame allocation.

The final step is `putImageData`, which copies from WASM linear memory into the browser's canvas backing store. This copy is unavoidable with the 2D canvas API.

### Why tiles march top-to-bottom

Because tiles arrive in raster order and each dirty region paints as it lands, some display frames show only the first N rows of a screen change. This is a fundamental property of RFX — the server does not buffer the full frame before sending. With bounded frame-marker gating (BEGIN/END markers that IronRDP parses for the ack but which can be surfaced to gate presentation) it is possible to hold rendering until a complete frame arrives, at the cost of slightly higher latency for large screen changes.

## What the HUD shows

The "Video" row in the Performance HUD and the codec badge in the toolbar report the **live** negotiated codec, not a fixed string. In practice you will see `RFX-Progressive` against xrdp and GNOME Remote Desktop, and `ClearCodec` against Windows Server 2025.

## MS-RDPEGFX / H.264

EGFX **is** negotiated now. This section previously said the opposite; that description predates the RFX-Progressive decoder.

The original problem was real: Windows only emits H.264 over EGFX when the host has a hardware encoder or the "Prioritize H.264/AVC 444 graphics mode" policy is set, and on a GPU-less host the channel carries RFX-Progressive instead — which IronRDP could not decode, so advertising EGFX produced a black screen. That was fixed by **porting a full RFX-Progressive decoder from FreeRDP** (`ironrdp-graphics`, driven from `on_progressive_data`), which is what makes EGFX safe on the GPU-less targets this project exists for.

On top of that, capability advertisement is conditional: `V8.1 {AVC420_ENABLED|SMALL_CACHE} + V8` when the browser's H.264 probe passes, else `V8 {SMALL_CACHE}` only. When a host does send AVC420, frames are handed to a WebCodecs `VideoDecoder` per surface, so H.264 is decoded by the *client's* GPU with no server GPU required. AVC444 is deliberately not implemented.

Two EGFX behaviours worth knowing when debugging a black screen:

- `egfx_active` gates the legacy path. Once an EGFX surface exists the legacy `DecodedImage` stays black, so legacy `GraphicsUpdate`s are suppressed. It is cleared when the last surface is deleted and on `ResetGraphics` — a latched `egfx_active` was one of three bugs that used to freeze xrdp sessions after a resize.
- Windows composes into **offscreen** surfaces and maps them later; blitting an unmapped surface paints garbage, so surfaces only present once `MapSurfaceToOutput` has been seen.
