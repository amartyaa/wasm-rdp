# Clipboard

Clipboard support is bidirectional — you can paste from the browser into the remote desktop and copy from the remote desktop back to the browser. The protocol is CLIPRDR (MS-RDPECLIP), implemented in `wasm/src/clipboard.rs` as `WasmCliprdrBackend`.

Both text and images are supported. Text transfers cleanly in both directions. Images go through a format conversion because the browser works in PNG/Blob and RDP works in DIB/DIBV5 (Windows Device Independent Bitmap).

---

## Local to remote (browser paste into RDP session)

When you press Ctrl+V inside the RDP canvas, the browser fires a `paste` event. The JS handler reads the clipboard contents (via `ClipboardEvent.clipboardData`) and calls into WASM:

- For plain text: `set_pending_clipboard(text)` stores it in a thread-local.
- For an image: `set_pending_clipboard_image(bytes)` stores the PNG bytes.

WASM then sends a `SendInitiateCopy` request to the server, advertising the available formats:
- `CF_UNICODETEXT` — if text is pending
- `CF_DIBV5` — if an image is pending

The server receives the format list and may later send a `FormatDataRequest` asking for one of the advertised formats. When that request arrives, `on_format_data_request` returns:

- For text: the stored UTF-16LE encoded string (CLIPRDR uses UTF-16LE for `CF_UNICODETEXT`).
- For an image: the PNG bytes converted to a DIB header + raw BGRA pixel data. The `ironrdp-cliprdr-format` crate handles this conversion.

The server then pastes the data into the target application.

---

## Remote to local (copy from RDP session to browser)

When something is copied on the remote desktop, the server sends a format list notification (`on_remote_copy`) listing all available clipboard formats. The handler picks the best one:

- If `CF_UNICODETEXT` is available, request that (plain text is preferred).
- Otherwise, if `CF_DIBV5` is available, request it. `CF_DIB` is also accepted if DIBV5 is not offered.

`SendInitiatePaste` goes back to the server requesting the chosen format. The server responds with `FormatDataResponse` containing the actual data.

`on_format_data_response` decodes the response:

- **Text**: the UTF-16LE bytes are decoded to a Rust string, then forwarded to JS via `write_clipboard_text`. The JS side tries `navigator.clipboard.writeText()` first; if that fails (no permission or no secure context), it falls back to `document.execCommand('copy')` via a temporary textarea.
- **Image**: the DIB/DIBV5 bytes are decoded back to RGBA pixel data using `ironrdp-cliprdr-format`, re-encoded as a PNG Blob, and written to the clipboard via `navigator.clipboard.write()`.

---

## Limitations

**Clipboard permission** — `navigator.clipboard.write()` (needed for writing images and some text cases) requires the page to be focused and the `clipboard-write` permission to be granted. On Chrome/Edge over HTTPS this is typically auto-granted, but on `localhost` or with non-standard policies it may not be.

**Format support** — only text and images are handled. Other Windows clipboard formats (RTF, HTML, custom application formats) are not advertised or requested.

**Timing** — the CLIPRDR handshake (monitor + capabilities PDUs) completes during session setup before the main loop. The format advertisement and data request are asynchronous; there can be a short delay between pressing Ctrl+V in the browser and the paste appearing on the remote.

**Clipboard sync on focus** — a clipboard sync attempt runs on `window.focus` (when the browser window regains focus), so that content copied outside the browser tab can propagate to the remote. This only triggers if an active session exists.
