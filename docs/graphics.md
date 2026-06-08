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

## What the HUD shows

The "Video" row in the Performance HUD and the codec badge in the toolbar are fixed at `RFX` — that is the only graphics codec this client decodes.

## Note on MS-RDPEGFX / H.264

The graphics-pipeline channel (MS-RDPEGFX) is intentionally not negotiated. An H.264/AVC420 passthrough to WebCodecs was prototyped and removed, because Windows only emits H.264 over that channel when the host has a hardware encoder (a GPU) or the "Prioritize H.264/AVC 444 graphics mode" group policy is enabled. On a GPU-less host the channel instead carries RFX-Progressive, which IronRDP cannot decode — so advertising the channel and then receiving RFX-Progressive produced a black screen. Since the legacy RFX path above works on every host (Windows and xrdp), we stay on it. Reviving H.264 would require either a GPU-backed host or implementing an RFX-Progressive decoder as a fallback, which mstsc has but IronRDP does not. `v0.12.0` has H.264 Enabled.
