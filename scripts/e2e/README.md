# End-to-end test sweep

Drives the real web client in Chrome against a live RDP target and asserts on
what actually reaches the canvas. Covers everything except audio.

```bash
cd scripts/e2e && npm install        # puppeteer-core only; uses your installed Chrome
```

Start a proxy first, then point the sweep at it:

```bash
server.exe --port 8092 --rdp-target <host>:3389 --enable-text-clipboard-sync

node e2e.js http://localhost:8092 <user> <pass> <label> --clip
node e2e.js http://localhost:8092 <user> <pass> <label>-hidpi --hidpi --quick
node hover.js http://localhost:8092 <user> <pass> <label>
node rail.js  http://localhost:8091 <user> <pass>      # Windows only
node toolbar.js http://localhost:8092 <user> <pass>
node fshint.js   http://localhost:8092 <user> <pass>
```

`CHROME=/path/to/chrome` overrides the browser location.

## What each script is for

| Script | Checks |
|---|---|
| `e2e.js` | connect/NLA, first paint, HUD rows, keyboard (incl. Super), mouse, CLIPRDR, 4 resizes, 3 toolbar fullscreen cycles, 2 browser-fullscreen cycles, hi-dpi ratio, liveness, disconnect |
| `hover.js` | pointer motion specifically — hovers a window close button and watches it change colour |
| `rail.js` | RemoteApp: catalog, published window paints, keyboard and Shift into it |
| `clip.js` | Clipboard ownership — reads the *remote session's* clipboard before and after a real Ctrl+V, in both directions |
| `toolbar.js` | Toolbar drag/pin/hide, the one-time hint, and that dragging sends no mouse input to the session |
| `altcapture.js` | Whether Keyboard Lock really captures a **real** OS-level Alt+Tab (see the caveat below) |
| `fshint.js` | The one-time fullscreen-shortcuts hint — shown once, dismissible, never comes back |

Results land in `e2e-<label>.json` plus a console log dump and a final
screenshot. A non-zero exit means at least one check failed.

## Target notes

- **Windows** needs `fDisabledAllowList=1` under
  `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Terminal Server\TSAppAllowList`
  before `rail.js` can launch anything, or exec fails with
  `RAIL_EXEC_E_NOT_IN_ALLOWLIST`. Note this is *not* the `Policies` key.
- **Windows RemoteApp sessions are reused.** `rail.js` types into Notepad and
  leaves it unsaved, so the session is disconnected, not logged off, and the
  next RAIL run reattaches to it with that Notepad still open. Log off the
  test user between runs, or set `RemoteAppLogoffTimeLimit` (see CLAUDE.md §4.7).
- **grd Remote Login** redirects to a per-session endpoint and lands on the GDM
  greeter, not a desktop. Pass `--login=<PAM password>` or every input check
  silently probes the greeter and proves nothing. The greeter password is the
  host account's, not the credential grd advertises for RDP.
- **xrdp and grd share port 3389** on a single host, so only one runs at a time.
  After switching away from grd, run `loginctl terminate-user <user>`; a
  lingering GNOME session makes `startwm.sh` exit in 0 s and the client shows a
  black screen that looks exactly like a client bug.

## Known probe limitations

- `altcapture.js` **needs an unlocked, interactive desktop.** It injects Alt+Tab
  with `keybd_event`, which feeds the same input queue as real hardware — CDP
  synthetic keys never reach the OS shortcut handler, so nothing else can test
  this. While the workstation is locked, input goes to the secure desktop and
  the result is meaningless: check `FG_BEFORE` in the output, and if it says
  `Windows Default Lock Screen`, the run proved nothing. It also moves real
  focus around, so don't run it while using the machine.

- `F11` cannot be driven: the client maps it to RDP scancode 0x57 and forwards
  it to the remote, and CDP synthetic keys never reach browser chrome. The
  browser-fullscreen path is exercised through CDP `Browser.setWindowBounds`
  with `windowState: fullscreen`, which is the same code path (a `resize` with
  no `fullscreenchange`).
- A single black sample after a resize is not failed immediately — a GPU-less
  host can take seconds to repaint a large desktop, so the check re-probes and
  reports how long recovery took.
