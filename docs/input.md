# Input handling

Keyboard and mouse input is captured in `web/app.js` and encoded into RDP FastPath input PDUs by `wasm/src/session.rs` using IronRDP's `input::Database`.

## Keyboard

### Scancode mapping

RDP uses AT-101 scancodes, not `KeyboardEvent.keyCode` or `charCode`. The `SCANCODE_MAP` in `app.js` maps `KeyboardEvent.code` strings (e.g. `"KeyA"`, `"ArrowLeft"`, `"NumpadEnter"`) to `[scancode, isExtended]` pairs. Extended keys are those that were originally absent from the 84-key AT keyboard and use a two-byte scancode sequence (the set includes right Ctrl/Alt, arrow keys, numpad operators, etc.).

On `keydown`, the JS handler looks up the scancode, calls `session.send_keyboard(scancode, isExtended, true)`. On `keyup`, the same with `false`. If the code is not in the map, the event is ignored — no fallback to keyCode. This means non-standard or exotic keys are silently dropped.

### Modifier key tracking

The JS side tracks which modifier keys are currently pressed. On `window.blur` (the page loses focus), all tracked modifiers are released explicitly by sending a `keyup` for each. Without this, keys like Ctrl and Shift would appear stuck on the remote side if the user alt-tabs away while holding them.

### Special shortcuts

A few key combinations are intercepted in JS before the scancode path:

- **Ctrl+Shift+F** — toggles browser fullscreen. Not forwarded to the remote.
- **Ctrl+Shift+D** — disconnects the session. Not forwarded.
- **Ctrl+Tab** — converted to Alt+Tab on the remote. The JS handler releases Ctrl first (sends a Ctrl keyup), then sends Alt down + Tab down + Tab up + Alt up. This lets you switch windows on the remote without triggering the browser's own tab cycling.
- **Ctrl+C / Ctrl+V** — passed through normally. Paste is also handled via the clipboard pipeline (see `docs/clipboard.md`).

The toolbar "CAD" button sends Ctrl+Alt+Del as three separate FastPath keydown events followed by three keyup events. The "Alt+Tab" button does the same for Alt+Tab.

### Key repeat

Browser `keydown` fires repeatedly when a key is held. These are forwarded to the remote, so the remote's own key-repeat mechanism sees the key as still down. There's no special de-bouncing — this is correct RDP behavior.

---

## Mouse

### Movement

Mouse move events on the canvas are forwarded to `session.send_mouse_move(x, y)`. To avoid flooding the server with 60+ events per second at high refresh rates, moves are throttled: events within `MOUSE_THROTTLE_MS` (16 ms, ~60 fps) of the last sent event are dropped. The last event in any throttled window is always sent on the next tick so the final cursor position is always accurate.

### Buttons

Mouse button down/up events call `session.send_mouse_button(button, x, y, down)`. The button parameter maps browser button indices to RDP button codes. Left (0), right (2), and middle (1) buttons are supported. Browser buttons 3 and 4 (browser back/forward) are not forwarded.

### Scroll wheel

Wheel events call `session.send_mouse_wheel(delta)`. RDP uses a signed integer for wheel delta; positive is "away from user" (scroll up), negative is toward. The browser's `deltaY` value is normalized to a fixed step size.

### Coordinate space

Mouse coordinates are taken directly from the canvas's client rect. In a single-monitor session the canvas covers the session desktop at a 1:1 pixel ratio. In multi-monitor sessions, each popup window has its own canvas covering one monitor, and mouse coordinates from that canvas are offset by the monitor's position in the combined desktop before being forwarded. This offset happens in JS (`window.opener.__rdp_session.send_mouse_move(x + monitorLeft, y + monitorTop)`), so WASM always receives combined desktop coordinates.

---

## Display control / resize

The `session.resize(width, height)` method exists and encodes a DisplayControl `MonitorLayout` PDU (`encode_resize`), but the `setupResizeHandler` in `app.js` is intentionally a no-op. Dynamic resize is not hooked up because xrdp does not support the DisplayControl virtual channel, and sending it to xrdp causes a blank screen. If you're targeting Windows-only and want dynamic resize, you'd need to feature-detect the target OS or gate on user opt-in.
