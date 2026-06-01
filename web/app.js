// ── WASM Module Loading ──────────────────────────────────
let wasm = null;
let session = null;

async function loadWasm() {
    try {
        wasm = await import('./pkg/wasm.js');
        await wasm.default(); // init the WASM module
        console.log('IronRDP WASM loaded');
    } catch (e) {
        console.error('Failed to load WASM module:', e);
        showError('Failed to load RDP module. Check that WASM is built.');
    }

    // Pre-fill cached credentials (domain + username only, not password)
    const cachedDomain = localStorage.getItem('rdp_domain');
    const cachedUsername = localStorage.getItem('rdp_username');
    if (cachedDomain) document.getElementById('domain').value = cachedDomain;
    if (cachedUsername) document.getElementById('username').value = cachedUsername;

    // Restore FPS cap setting
    const fpsSelect = document.getElementById('fps-cap');
    fpsSelect.value = String(fpsCap);
    fpsSelect.addEventListener('change', () => {
        fpsCap = parseInt(fpsSelect.value, 10);
        localStorage.setItem('rdp_fps_cap', String(fpsCap));
    });

    // Settings gear toggle
    const btnSettings = document.getElementById('btn-settings');
    const settingsPanel = document.getElementById('settings-panel');
    btnSettings.addEventListener('click', () => {
        const open = settingsPanel.hidden;
        settingsPanel.hidden = !open;
        btnSettings.classList.toggle('active', open);
    });

    // Multi-monitor toggle. Only selectable on Chromium + secure context; the
    // note explains the requirement when it can't be enabled.
    const multimonToggle = document.getElementById('multimon-toggle');
    const multimonNote = document.getElementById('multimon-note');
    if (multimonSupported()) {
        multimonToggle.checked = multimonEnabled;
        multimonToggle.addEventListener('change', () => {
            multimonEnabled = multimonToggle.checked;
            localStorage.setItem('rdp_multimon', multimonEnabled ? '1' : '0');
        });
    } else {
        multimonEnabled = false;
        multimonToggle.checked = false;
        multimonToggle.disabled = true;
        multimonNote.textContent = window.isSecureContext
            ? 'Unavailable: your browser lacks the Window Management API (use Chrome/Edge).'
            : 'Unavailable: requires a secure context (HTTPS). Windows hosts only.';
    }
}

// ── DOM Elements ─────────────────────────────────────────
const loginScreen = document.getElementById('login-screen');
const loginForm = document.getElementById('login-form');
const connectBtn = document.getElementById('connect-btn');
const btnText = connectBtn.querySelector('.btn-text');
const btnLoader = connectBtn.querySelector('.btn-loader');
const loginError = document.getElementById('login-error');
const canvasContainer = document.getElementById('canvas-container');
const canvas = document.getElementById('rdp-canvas');
const toolbar = document.getElementById('toolbar');
const fpsBadge = document.getElementById('fps-badge');
const resBadge = document.getElementById('resolution-badge');
const codecBadge = document.getElementById('codec-badge');
const btnCad = document.getElementById('btn-cad');
const btnAltTab = document.getElementById('btn-alttab');
const btnFullscreen = document.getElementById('btn-fullscreen');
const btnDisconnect = document.getElementById('btn-disconnect');
const fullscreenCheckbox = document.getElementById('fullscreen-preconnect');
const reconnectOverlay = document.getElementById('reconnect-overlay');
const reconnectStatus = document.getElementById('reconnect-status');
const btnCancelReconnect = document.getElementById('btn-cancel-reconnect');
const perfHud = document.getElementById('perf-hud');
const btnCloseHud = document.getElementById('btn-close-hud');
const hudLatency = document.getElementById('hud-latency');
const hudFps = document.getElementById('hud-fps');
const hudDlSpeed = document.getElementById('hud-dl-speed');
const hudUlSpeed = document.getElementById('hud-ul-speed');
const hudTotalRx = document.getElementById('hud-total-rx');
const hudTotalTx = document.getElementById('hud-total-tx');
const hudResolution = document.getElementById('hud-resolution');
const hudCodec = document.getElementById('hud-codec');
const hudAudioCodec = document.getElementById('hud-audio-codec');

// ── State ────────────────────────────────────────────────
let frameCount = 0;
let lastFpsUpdate = performance.now();
let toolbarTimeout = null;
let lastMouseTime = 0;
let resizeTimeout = null;
let fpsCap = parseInt(localStorage.getItem('rdp_fps_cap') || '30', 10);
const MOUSE_THROTTLE_MS = 16; // ~60fps cap on mouse events
const RESIZE_DEBOUNCE_MS = 250;
let statsInterval = null;
let prevRxBytes = 0;
let prevTxBytes = 0;
let hudVisible = false;

// ── Audio State ──────────────────────────────────────────
let audioContext = null;
let audioGain = null;            // volume control (honors RDPSND Volume PDU)
let audioWorkletNode = null;     // pull-based playback node
let audioWorkletReady = false;   // module loaded
let audioFormat = null;          // { channels, sourceRate } of the active node
// WebCodecs decode state (Opus/AAC -> PCM -> worklet)
let audioDecoder = null;
let audioDecoderCodec = null;    // 'opus' | 'mp4a.40.2'
let audioDecoderRate = 0;
let audioDecoderChannels = 0;
let audioPts = 0;                // monotonic timestamp for EncodedAudioChunk
let currentAudioCodec = '--';    // negotiated codec, shown in the HUD

// ── Multi-Monitor State ──────────────────────────────────
// Multi-monitor uses the Window Management API (Chromium + secure context only)
// and opens one browser window per physical display, all driven by the single
// WASM session. Off by default; the layout is empty unless explicitly enabled.
let multimonEnabled = localStorage.getItem('rdp_multimon') === '1';
let monitorPopups = [];      // secondary displays: [{ win, canvas, monitor }]
let screenDetailsObj = null; // ScreenDetails handle (source of 'screenschange')
let multimonInUse = false;   // true while a multi-monitor session is active

// ── Reconnection State ───────────────────────────────────
let savedCredentials = null;  // { username, password, domain }
let isUserDisconnect = false; // true when user clicks disconnect
let reconnectAttempt = 0;
let reconnectTimer = null;
const MAX_RECONNECT_ATTEMPTS = 5;
const RECONNECT_DELAYS = [2000, 4000, 8000, 16000, 32000]; // exponential backoff

// ── AT-101 Scancode Map ──────────────────────────────────
// Maps KeyboardEvent.code → [scancode, isExtended]
const SCANCODE_MAP = {
    Escape: [0x01, false], Digit1: [0x02, false], Digit2: [0x03, false],
    Digit3: [0x04, false], Digit4: [0x05, false], Digit5: [0x06, false],
    Digit6: [0x07, false], Digit7: [0x08, false], Digit8: [0x09, false],
    Digit9: [0x0A, false], Digit0: [0x0B, false], Minus: [0x0C, false],
    Equal: [0x0D, false], Backspace: [0x0E, false], Tab: [0x0F, false],
    KeyQ: [0x10, false], KeyW: [0x11, false], KeyE: [0x12, false],
    KeyR: [0x13, false], KeyT: [0x14, false], KeyY: [0x15, false],
    KeyU: [0x16, false], KeyI: [0x17, false], KeyO: [0x18, false],
    KeyP: [0x19, false], BracketLeft: [0x1A, false], BracketRight: [0x1B, false],
    Enter: [0x1C, false], ControlLeft: [0x1D, false], KeyA: [0x1E, false],
    KeyS: [0x1F, false], KeyD: [0x20, false], KeyF: [0x21, false],
    KeyG: [0x22, false], KeyH: [0x23, false], KeyJ: [0x24, false],
    KeyK: [0x25, false], KeyL: [0x26, false], Semicolon: [0x27, false],
    Quote: [0x28, false], Backquote: [0x29, false], ShiftLeft: [0x2A, false],
    Backslash: [0x2B, false], KeyZ: [0x2C, false], KeyX: [0x2D, false],
    KeyC: [0x2E, false], KeyV: [0x2F, false], KeyB: [0x30, false],
    KeyN: [0x31, false], KeyM: [0x32, false], Comma: [0x33, false],
    Period: [0x34, false], Slash: [0x35, false], ShiftRight: [0x36, false],
    NumpadMultiply: [0x37, false], AltLeft: [0x38, false], Space: [0x39, false],
    CapsLock: [0x3A, false],
    F1: [0x3B, false], F2: [0x3C, false], F3: [0x3D, false], F4: [0x3E, false],
    F5: [0x3F, false], F6: [0x40, false], F7: [0x41, false], F8: [0x42, false],
    F9: [0x43, false], F10: [0x44, false], F11: [0x57, false], F12: [0x58, false],
    NumLock: [0x45, false], ScrollLock: [0x46, false],
    Numpad7: [0x47, false], Numpad8: [0x48, false], Numpad9: [0x49, false],
    NumpadSubtract: [0x4A, false], Numpad4: [0x4B, false], Numpad5: [0x4C, false],
    Numpad6: [0x4D, false], NumpadAdd: [0x4E, false], Numpad1: [0x4F, false],
    Numpad2: [0x50, false], Numpad3: [0x51, false], Numpad0: [0x52, false],
    NumpadDecimal: [0x53, false],
    // Extended keys
    NumpadEnter: [0x1C, true], ControlRight: [0x1D, true], NumpadDivide: [0x35, true],
    AltRight: [0x38, true], Home: [0x47, true], ArrowUp: [0x48, true],
    PageUp: [0x49, true], ArrowLeft: [0x4B, true], ArrowRight: [0x4D, true],
    End: [0x4F, true], ArrowDown: [0x50, true], PageDown: [0x51, true],
    Insert: [0x52, true], Delete: [0x53, true], MetaLeft: [0x5B, true],
    MetaRight: [0x5C, true], ContextMenu: [0x5D, true],
    PrintScreen: [0x37, true], Pause: [0x45, false],
};

// ── Connect Flow ─────────────────────────────────────────
loginForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    if (!wasm) {
        showError('WASM module not loaded yet');
        return;
    }

    const username = document.getElementById('username').value;
    const password = document.getElementById('password').value;
    const domain = document.getElementById('domain').value || '';

    setConnecting(true);
    hideError();

    try {
        // Pre-connect fullscreen
        if (fullscreenCheckbox.checked) {
            try { await document.documentElement.requestFullscreen(); } catch (_) {}
        }

        await doConnect(username, password, domain);

    } catch (err) {
        showError(String(err));
        setConnecting(false);
        // Close any secondary windows opened before the connect failed.
        teardownMonitorWindows();
        // Exit fullscreen on error
        if (document.fullscreenElement) {
            document.exitFullscreen().catch(() => {});
        }
    }
});

async function doConnect(username, password, domain) {
    const width = window.innerWidth;
    const height = window.innerHeight;

    // Build WebSocket URL for the proxy
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const basePath = location.pathname.replace(/\/[^/]*$/, '');
    const wsUrl = `${proto}://${location.host}${basePath}/ws`;

    // Expose FPS cap for WASM to read
    window.__rdp_fps_cap = fpsCap;

    // Detect which compressed audio codecs the browser can decode. We only
    // advertise these to the RDP server if WebCodecs can actually handle them;
    // otherwise negotiation stays on PCM.
    const codecs = await detectAudioCodecs();
    console.log('[RDPSND] WebCodecs support — opus:', codecs.opus, 'aac:', codecs.aac);

    // Multi-monitor: build the physical-screen layout when enabled & supported,
    // and open the secondary-display popups now (close to the connect gesture, to
    // avoid popup blocking). Falls back to single-monitor otherwise.
    let monitorBuild = null;
    if (multimonEnabled && multimonSupported()) {
        try {
            monitorBuild = await buildMonitorLayout();
        } catch (e) {
            console.warn('[multimon] getScreenDetails failed, using single monitor:', e);
        }
        if (monitorBuild && monitorBuild.layout.length > 1) {
            openSecondaryPopups(monitorBuild.layout);
        } else {
            monitorBuild = null; // only one screen, or an unusable arrangement
        }
    }

    // Flat [left,top,width,height,primary] per monitor (combined-desktop pixels,
    // primary at 0,0). Empty ⇒ single monitor. A layout can also be injected for
    // single-window testing via window.__rdp_monitors.
    const monitors = monitorBuild
        ? monitorBuild.flat
        : (Array.isArray(window.__rdp_monitors)
            ? Int32Array.from(window.__rdp_monitors.flat())
            : new Int32Array(0));

    session = await wasm.connect(
        wsUrl, username, password, domain, width, height, 'rdp-canvas',
        codecs.opus, codecs.aac, monitors,
    );
    multimonInUse = !!monitorBuild;

    // Store credentials in-memory for reconnection
    savedCredentials = { username, password, domain };
    isUserDisconnect = false;
    reconnectAttempt = 0;

    // Connected — switch to canvas view
    loginScreen.hidden = true;
    canvasContainer.hidden = false;
    reconnectOverlay.hidden = true;
    toolbar.hidden = false;

    // Cache credentials for next session (not password)
    localStorage.setItem('rdp_domain', domain);
    localStorage.setItem('rdp_username', username);

    resBadge.textContent = `${session.width}×${session.height}`;
    setupInputHandlers();
    if (multimonInUse) {
        setupMonitorSurfaces(monitorBuild.layout);
    }
    setupResizeHandler();
    startStatsInterval();
    initAudioContext();
    showToolbar();
}

function setConnecting(loading) {
    connectBtn.disabled = loading;
    btnText.hidden = loading;
    btnLoader.hidden = !loading;
}

function showError(msg) {
    loginError.textContent = msg;
    loginError.hidden = false;
}

function hideError() {
    loginError.hidden = true;
}

// ── Input Handlers ───────────────────────────────────────
function setupInputHandlers() {
    // Keyboard + clipboard on the main document.
    setupDocInput(document, window);
    // Mouse on the main canvas. Single-monitor and the multi-monitor primary
    // both map at offset (0,0); secondary popups get their own offsets.
    attachCanvasMouse(canvas, 0, 0);
}

// Attach keyboard + clipboard handlers to a document (main window or a popup).
function setupDocInput(doc, win) {
    doc.addEventListener('keydown', onKeyDown, true);
    doc.addEventListener('keyup', onKeyUp, true);
    win.addEventListener('blur', releaseAllModifiers);
    doc.addEventListener('paste', onPaste);
    doc.addEventListener('copy', onCopy);
}

// Attach mouse handlers to a canvas, tagging it with the combined-desktop offset
// (the monitor's top-left) so input maps into the shared session coordinate space.
function attachCanvasMouse(canvasEl, offsetX, offsetY) {
    canvasEl.__rdpOffset = { x: offsetX, y: offsetY };
    canvasEl.addEventListener('mousemove', onMouseMove);
    canvasEl.addEventListener('mousedown', onMouseDown);
    canvasEl.addEventListener('mouseup', onMouseUp);
    canvasEl.addEventListener('wheel', onWheel, { passive: false });
    canvasEl.addEventListener('contextmenu', (e) => e.preventDefault());
}

// Release all modifier keys in the RDP session.
// Called on blur to prevent stuck Ctrl/Alt/Shift when focus leaves the browser.
function releaseAllModifiers() {
    if (!session) return;
    session.send_keyboard(0x1D, false, false);  // Ctrl left up
    session.send_keyboard(0x1D, false, true);   // Ctrl right up
    session.send_keyboard(0x2A, false, false);  // Shift left up
    session.send_keyboard(0x36, false, false);  // Shift right up
    session.send_keyboard(0x38, false, false);  // Alt left up
    session.send_keyboard(0x38, false, true);   // Alt right up
    session.send_keyboard(0x5B, false, true);   // Meta left up
    session.send_keyboard(0x5C, false, true);   // Meta right up
}

function onKeyDown(e) {
    if (!session) return;

    // Intercept Ctrl+Shift+F → fullscreen toggle
    if (e.ctrlKey && e.shiftKey && e.code === 'KeyF') {
        e.preventDefault();
        releaseAllModifiers();
        toggleFullscreen();
        return;
    }
    // Intercept Ctrl+Shift+D → disconnect
    if (e.ctrlKey && e.shiftKey && e.code === 'KeyD') {
        e.preventDefault();
        releaseAllModifiers();
        disconnect();
        return;
    }
    // Remap Ctrl+Tab → Alt+Tab
    // Release Ctrl first so the remote sees pure Alt+Tab, not Ctrl+Alt+Tab
    if (e.ctrlKey && e.code === 'Tab') {
        e.preventDefault();
        session.send_keyboard(0x1D, false, false); // Ctrl up
        sendAltTab();
        return;
    }

    // Allow Ctrl+V and Ctrl+C to pass through so the browser fires native
    // 'paste' and 'copy' events for clipboard sync between local and remote.
    if (!(e.ctrlKey && (e.code === 'KeyV' || e.code === 'KeyC'))) {
        e.preventDefault();
    }
    const mapping = SCANCODE_MAP[e.code];
    if (mapping) {
        session.send_keyboard(mapping[0], true, mapping[1]);
    }
}

function onKeyUp(e) {
    if (!session) return;
    e.preventDefault();
    const mapping = SCANCODE_MAP[e.code];
    if (mapping) {
        session.send_keyboard(mapping[0], false, mapping[1]);
    }
}

function getCanvasCoords(e) {
    const el = e.currentTarget;
    const rect = el.getBoundingClientRect();
    const scaleX = el.width / rect.width;
    const scaleY = el.height / rect.height;
    const off = el.__rdpOffset || { x: 0, y: 0 };
    return {
        x: Math.round((e.clientX - rect.left) * scaleX) + off.x,
        y: Math.round((e.clientY - rect.top) * scaleY) + off.y,
    };
}

function onMouseMove(e) {
    if (!session) return;
    const now = performance.now();
    if (now - lastMouseTime < MOUSE_THROTTLE_MS) return;
    lastMouseTime = now;
    const { x, y } = getCanvasCoords(e);
    session.send_mouse_move(x, y);
}

function onMouseDown(e) {
    if (!session) return;
    e.preventDefault();
    const { x, y } = getCanvasCoords(e);
    session.send_mouse_button(e.button, true, x, y);
}

function onMouseUp(e) {
    if (!session) return;
    e.preventDefault();
    const { x, y } = getCanvasCoords(e);
    session.send_mouse_button(e.button, false, x, y);
}

function onWheel(e) {
    if (!session) return;
    e.preventDefault();
    const delta = Math.sign(e.deltaY) * -120; // Standard wheel delta
    session.send_mouse_wheel(false, delta);
}

function onPaste(e) {
    if (!session || !wasm) return;

    // Check for image data first (screenshots, copied images)
    const items = e.clipboardData?.items;
    if (items) {
        for (const item of items) {
            if (item.type === 'image/png') {
                const blob = item.getAsFile();
                if (blob) {
                    blob.arrayBuffer().then(buf => {
                        try {
                            const bytes = new Uint8Array(buf);
                            wasm.set_pending_clipboard_image(bytes, session);
                        } catch (err) {
                            console.warn('Clipboard image paste to WASM failed:', err);
                        }
                    });
                    return; // image takes priority
                }
            }
        }
    }

    // Fall back to text
    const text = e.clipboardData?.getData('text/plain');
    if (text) {
        try {
            wasm.set_pending_clipboard(text, session);
        } catch (err) {
            console.warn('Clipboard paste to WASM failed:', err);
        }
    }
}

function onCopy(e) {
    // Remote → Local clipboard sync.
    // When the user presses Ctrl+C in the web client, write cached remote
    // clipboard data to the local clipboard via the copy event's clipboardData.
    // This works on HTTP (no secure context needed).
    const text = window.__rdp_remote_clipboard_text;
    const image = window.__rdp_remote_clipboard_image;

    if (text) {
        e.preventDefault();
        e.clipboardData.setData('text/plain', text);
    } else if (image) {
        // Note: clipboardData.setData only supports text types.
        // Image copy via copy event is not universally supported.
        // The WASM inline JS already attempts navigator.clipboard.write().
    }
}

// ── Resize Handler ───────────────────────────────────────
// Disabled: xrdp does not support the Display Control Virtual Channel,
// so dynamic resize causes a black screen. Canvas stays fixed at the
// negotiated RDP resolution.
function setupResizeHandler() {
    // no-op: keep canvas at the server-negotiated size
}

// ── Multi-Monitor (Window Management API) ────────────────
// One WASM session drives N same-origin windows: the main page shows the primary
// monitor, one popup per secondary display. The session paints each window's
// canvas (same-origin → in-process) and each window forwards input back, offset
// into the combined-desktop coordinate space.

function multimonSupported() {
    return typeof window.getScreenDetails === 'function' && window.isSecureContext;
}

// Enumerate physical screens and build a normalized layout. Primary is placed at
// (0,0) and the others translated relative to it. v1 supports only non-negative
// arrangements (secondaries to the right/below); negative offsets (a screen left
// of / above the primary) fall back to single-monitor — RDP needs the primary at
// the origin and our framebuffer is 0-based. Uses CSS pixels (scale factor 100).
async function buildMonitorLayout() {
    if (!screenDetailsObj) {
        screenDetailsObj = await window.getScreenDetails();
        screenDetailsObj.addEventListener('screenschange', onScreensChange);
    }
    const screens = screenDetailsObj.screens;
    const primary = screens.find((s) => s.isPrimary) || screenDetailsObj.currentScreen || screens[0];

    const layout = screens.map((s) => ({
        screen: s,
        left: s.left - primary.left,
        top: s.top - primary.top,
        width: s.width,
        height: s.height,
        primary: s === primary,
    }));

    if (layout.some((m) => m.left < 0 || m.top < 0)) {
        console.warn('[multimon] arrangement has a screen left of / above the primary; ' +
            'this v1 supports right/below only — falling back to single monitor.');
        return null;
    }

    const flat = [];
    for (const m of layout) flat.push(m.left, m.top, m.width, m.height, m.primary ? 1 : 0);
    console.log('[multimon] layout', layout.map((m) => `${m.width}x${m.height}@(${m.left},${m.top})${m.primary ? '*' : ''}`).join('  '));
    return { flat: Int32Array.from(flat), layout };
}

// Open one popup window per secondary monitor, placed over that screen.
function openSecondaryPopups(layout) {
    teardownMonitorWindows();
    for (const m of layout) {
        if (m.primary) continue;
        const s = m.screen;
        const features = `popup,left=${s.availLeft},top=${s.availTop},width=${s.availWidth},height=${s.availHeight}`;
        const win = window.open('', `rdp-monitor-${m.left}-${m.top}`, features);
        if (!win) {
            console.warn('[multimon] popup blocked for a secondary display; allow popups for this site.');
            continue;
        }
        win.document.title = 'Remote Display';
        const style = win.document.createElement('style');
        style.textContent = 'html,body{margin:0;height:100%;background:#000;overflow:hidden;cursor:none}canvas{display:block;width:100vw;height:100vh}';
        win.document.head.appendChild(style);
        const c = win.document.createElement('canvas');
        win.document.body.appendChild(c);
        monitorPopups.push({ win, canvas: c, monitor: m });
    }
}

// Declare the full surface set to the session (primary = main canvas, secondaries
// = popup canvases) and wire each window's input + fullscreen.
function setupMonitorSurfaces(layout) {
    if (!session) return;
    const primary = layout.find((m) => m.primary) || layout[0];

    session.clear_surfaces();
    // Primary → main page canvas. Mouse already attached by setupInputHandlers.
    canvas.__rdpOffset = { x: 0, y: 0 };
    session.add_surface(canvas, 0, 0, primary.width, primary.height);

    for (const p of monitorPopups) {
        const m = p.monitor;
        session.add_surface(p.canvas, m.left, m.top, m.width, m.height);
        attachCanvasMouse(p.canvas, m.left, m.top);
        setupDocInput(p.win.document, p.win);
        enableClickFullscreen(p.win, m.screen);
    }
}

// Fullscreen a popup on its screen on first interaction. requestFullscreen needs
// a user gesture, which we don't have after the async connect — so defer it to
// the first click inside the popup. Until then the window is just sized to the
// screen's work area (with browser chrome).
function enableClickFullscreen(win, screen) {
    const go = () => {
        try {
            const el = win.document.documentElement;
            const p = el.requestFullscreen ? el.requestFullscreen({ screen }) : null;
            if (p && p.catch) p.catch(() => {});
        } catch (_) {}
    };
    win.document.addEventListener('mousedown', go, { once: true });
}

// Close all secondary popups (e.g. on disconnect or before a relayout).
function teardownMonitorWindows() {
    for (const p of monitorPopups) {
        try { p.win.close(); } catch (_) {}
    }
    monitorPopups = [];
}

// React to a physical-arrangement change on a live multi-monitor session:
// rebuild the layout, re-open popups, push the new layout over DisplayControl,
// and re-declare surfaces.
async function onScreensChange() {
    if (!multimonInUse || !session) return;
    let build;
    try { build = await buildMonitorLayout(); } catch (_) { return; }
    if (!build || build.layout.length < 2) return;
    console.log('[multimon] screens changed — applying new layout');
    openSecondaryPopups(build.layout);
    session.apply_monitor_layout(build.flat);
    setupMonitorSurfaces(build.layout);
}

// ── Special Keys ─────────────────────────────────────────
function sendCtrlAltDel() {
    if (!session) return;
    // Press Ctrl, Alt, Del
    session.send_keyboard(0x1D, true, false);  // Ctrl down
    session.send_keyboard(0x38, true, false);  // Alt down
    session.send_keyboard(0x53, true, true);   // Del down (extended)
    // Release in reverse
    session.send_keyboard(0x53, false, true);  // Del up
    session.send_keyboard(0x38, false, false); // Alt up
    session.send_keyboard(0x1D, false, false); // Ctrl up
}

function sendAltTab() {
    if (!session) return;
    session.send_keyboard(0x38, true, false);  // Alt down
    session.send_keyboard(0x0F, true, false);  // Tab down
    session.send_keyboard(0x0F, false, false); // Tab up
    session.send_keyboard(0x38, false, false); // Alt up
}

// ── Toolbar ──────────────────────────────────────────────
btnCad.addEventListener('click', sendCtrlAltDel);
btnAltTab.addEventListener('click', sendAltTab);
btnFullscreen.addEventListener('click', toggleFullscreen);
btnDisconnect.addEventListener('click', disconnect);

function showToolbar() {
    toolbar.classList.add('visible');
    resetToolbarHide();
}

function resetToolbarHide() {
    clearTimeout(toolbarTimeout);
    toolbarTimeout = setTimeout(() => {
        if (document.fullscreenElement) {
            toolbar.classList.remove('visible');
        }
    }, 3000);
}

// Show toolbar when mouse is near top edge
document.addEventListener('mousemove', (e) => {
    if (!canvasContainer.hidden && e.clientY < 10) {
        showToolbar();
    }
});

function toggleFullscreen() {
    if (document.fullscreenElement) {
        document.exitFullscreen();
    } else {
        document.documentElement.requestFullscreen();
    }
}

function disconnect() {
    isUserDisconnect = true;
    cancelReconnect();
    if (session) {
        session.shutdown();
        session = null;
    }
    savedCredentials = null;
    cleanupSession();
}

function cleanupSession() {
    canvasContainer.hidden = true;
    reconnectOverlay.hidden = true;
    perfHud.hidden = true;
    hudVisible = false;
    toolbar.hidden = true;
    toolbar.classList.remove('visible');
    loginScreen.hidden = false;
    setConnecting(false);
    if (document.fullscreenElement) {
        document.exitFullscreen().catch(() => {});
    }
    // Multi-monitor: close secondary windows and stop tracking screen changes.
    teardownMonitorWindows();
    multimonInUse = false;
    if (screenDetailsObj) {
        try { screenDetailsObj.removeEventListener('screenschange', onScreensChange); } catch (_) {}
        screenDetailsObj = null;
    }
    // Remove input handlers
    document.removeEventListener('keydown', onKeyDown, true);
    document.removeEventListener('keyup', onKeyUp, true);
    window.removeEventListener('blur', releaseAllModifiers);
    document.removeEventListener('paste', onPaste);
    document.removeEventListener('copy', onCopy);
    // Stop stats interval
    if (statsInterval) { clearInterval(statsInterval); statsInterval = null; }
    prevRxBytes = 0;
    prevTxBytes = 0;
    // Close audio context and tear down the worklet graph
    if (audioDecoder) {
        try { audioDecoder.close(); } catch (_) {}
        audioDecoder = null;
        audioDecoderCodec = null;
        audioDecoderRate = 0;
        audioDecoderChannels = 0;
        audioPts = 0;
    }
    if (audioWorkletNode) {
        try { audioWorkletNode.disconnect(); } catch (_) {}
        audioWorkletNode = null;
    }
    currentAudioCodec = '--';
    if (hudAudioCodec) hudAudioCodec.textContent = '--';
    if (audioContext) {
        audioContext.close().catch(() => {});
        audioContext = null;
        audioGain = null;
        audioWorkletReady = false;
        audioFormat = null;
    }
}

// ── Auto-Reconnection ────────────────────────────────────

function cancelReconnect() {
    if (reconnectTimer) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
    }
    reconnectAttempt = 0;
}

async function attemptReconnect() {
    if (!savedCredentials || isUserDisconnect) return;
    if (reconnectAttempt >= MAX_RECONNECT_ATTEMPTS) {
        reconnectStatus.textContent = 'Could not reconnect. Please log in again.';
        setTimeout(() => {
            cancelReconnect();
            savedCredentials = null;
            cleanupSession();
        }, 3000);
        return;
    }

    const delay = RECONNECT_DELAYS[reconnectAttempt] || 32000;
    reconnectAttempt++;
    reconnectStatus.textContent = `Attempt ${reconnectAttempt}/${MAX_RECONNECT_ATTEMPTS} — retrying in ${delay / 1000}s...`;

    reconnectTimer = setTimeout(async () => {
        reconnectTimer = null;
        if (!savedCredentials || isUserDisconnect) return;

        reconnectStatus.textContent = `Attempt ${reconnectAttempt}/${MAX_RECONNECT_ATTEMPTS} — connecting...`;
        try {
            // Clean up old session
            session = null;
            document.removeEventListener('keydown', onKeyDown, true);
            document.removeEventListener('keyup', onKeyUp, true);
            window.removeEventListener('blur', releaseAllModifiers);
            document.removeEventListener('paste', onPaste);
            document.removeEventListener('copy', onCopy);

            const { username, password, domain } = savedCredentials;
            await doConnect(username, password, domain);
            // Success — overlay hidden by doConnect
        } catch (err) {
            console.warn(`Reconnect attempt ${reconnectAttempt} failed:`, err);
            attemptReconnect();
        }
    }, delay);
}

btnCancelReconnect.addEventListener('click', () => {
    cancelReconnect();
    savedCredentials = null;
    cleanupSession();
});

// Called by WASM when the RDP session ends
window.__rdp_session_ended = function(reason) {
    console.log('Session ended, reason:', reason);
    if (reason === 'connection_lost' && savedCredentials && !isUserDisconnect) {
        // Unexpected disconnect — try to reconnect
        session = null;
        // Close frozen secondary windows during the backoff; they're re-opened
        // by the reconnect's doConnect if multi-monitor is still in use.
        teardownMonitorWindows();
        reconnectOverlay.hidden = false;
        toolbar.hidden = true;
        attemptReconnect();
    } else {
        // User-initiated or unrecoverable — go to login
        session = null;
        cleanupSession();
    }
};

// ── Stats Interval (FPS + HUD) ───────────────────────────
function startStatsInterval() {
    if (statsInterval) clearInterval(statsInterval);
    prevRxBytes = session ? session.rx_bytes : 0;
    prevTxBytes = session ? session.tx_bytes : 0;

    statsInterval = setInterval(() => {
        // Update FPS badge (always visible in toolbar)
        fpsBadge.textContent = `${frameCount} FPS`;
        const currentFps = frameCount;
        frameCount = 0;

        // Update HUD if visible
        if (!hudVisible || !session) return;

        const rxBytes = session.rx_bytes;
        const txBytes = session.tx_bytes;
        const dlSpeed = rxBytes - prevRxBytes;
        const ulSpeed = txBytes - prevTxBytes;
        prevRxBytes = rxBytes;
        prevTxBytes = txBytes;

        hudFps.textContent = currentFps;
        hudDlSpeed.textContent = formatSpeed(dlSpeed);
        hudUlSpeed.textContent = formatSpeed(ulSpeed);
        hudTotalRx.textContent = formatSize(rxBytes);
        hudTotalTx.textContent = formatSize(txBytes);
        hudResolution.textContent = multimonInUse
            ? `${session.width}×${session.height} (${monitorPopups.length + 1} mon)`
            : `${session.width}×${session.height}`;
        hudCodec.textContent = 'RFX';

        // Latency ping
        const pingStart = performance.now();
        fetch('/ping').then(() => {
            hudLatency.textContent = `${Math.round(performance.now() - pingStart)} ms`;
        }).catch(() => {
            hudLatency.textContent = '-- ms';
        });
    }, 1000);
}

function formatSpeed(bytesPerSec) {
    if (bytesPerSec >= 1048576) return `${(bytesPerSec / 1048576).toFixed(1)} MB/s`;
    if (bytesPerSec >= 1024) return `${(bytesPerSec / 1024).toFixed(0)} KB/s`;
    return `${bytesPerSec} B/s`;
}

function formatSize(bytes) {
    if (bytes >= 1073741824) return `${(bytes / 1073741824).toFixed(2)} GB`;
    if (bytes >= 1048576) return `${(bytes / 1048576).toFixed(1)} MB`;
    if (bytes >= 1024) return `${(bytes / 1024).toFixed(0)} KB`;
    return `${bytes} B`;
}

// Called from WASM on each graphics update
window.__rdp_frame = function() {
    frameCount++;
};

// ── Performance HUD Toggle ───────────────────────────────
fpsBadge.addEventListener('click', () => {
    hudVisible = !hudVisible;
    perfHud.hidden = !hudVisible;
});
btnCloseHud.addEventListener('click', () => {
    hudVisible = false;
    perfHud.hidden = true;
});

// ── Audio Playback (RDPSND) ──────────────────────────────
// Pull-based playback via an AudioWorklet ring buffer (see audio-worklet.js).
// The audio device clock drives consumption, so there is no playback cursor to
// drift — fixing the lag-and-never-recover behavior of the old scheduler.
function initAudioContext() {
    if (audioContext) return;
    try {
        audioContext = new AudioContext();
        audioGain = audioContext.createGain();
        audioGain.gain.value = 1.0;
        audioGain.connect(audioContext.destination);
        console.log('[RDPSND] AudioContext created, sampleRate:', audioContext.sampleRate);

        audioContext.audioWorklet.addModule('./audio-worklet.js')
            .then(() => {
                audioWorkletReady = true;
                console.log('[RDPSND] Audio worklet loaded');
            })
            .catch((e) => console.warn('[RDPSND] Failed to load audio worklet:', e));
    } catch (e) {
        console.warn('[RDPSND] Failed to create AudioContext:', e);
    }
}

// Probe WebCodecs for Opus/AAC decode support. Returns { opus, aac }.
async function detectAudioCodecs() {
    const result = { opus: false, aac: false };
    if (typeof AudioDecoder === 'undefined' || !AudioDecoder.isConfigSupported) {
        return result;
    }
    try {
        const o = await AudioDecoder.isConfigSupported({ codec: 'opus', sampleRate: 48000, numberOfChannels: 2 });
        result.opus = !!o.supported;
    } catch (_) {}
    try {
        const a = await AudioDecoder.isConfigSupported({ codec: 'mp4a.40.2', sampleRate: 48000, numberOfChannels: 2 });
        result.aac = !!a.supported;
    } catch (_) {}
    return result;
}

// Ensure the worklet node matches the given format, then hand it per-channel PCM.
function postPcmToWorklet(channelData, channels, sourceRate) {
    if (!audioWorkletNode || audioFormat.channels !== channels || audioFormat.sourceRate !== sourceRate) {
        if (audioWorkletNode) {
            try { audioWorkletNode.disconnect(); } catch (_) {}
        }
        audioFormat = { channels, sourceRate };
        audioWorkletNode = new AudioWorkletNode(audioContext, 'rdp-audio', {
            numberOfInputs: 0,
            numberOfOutputs: 1,
            outputChannelCount: [Math.max(1, channels)],
            processorOptions: { channels, sourceRate },
        });
        audioWorkletNode.connect(audioGain);
    }
    audioWorkletNode.port.postMessage(
        { type: 'pcm', channelData },
        channelData.map((a) => a.buffer),
    );
}

// Synthesize an OpusHead identification header for raw Opus packets (RDP does
// not Ogg-encapsulate, and WebCodecs' Opus decoder needs it as `description`).
function makeOpusHead(channels, sampleRate) {
    const h = new Uint8Array(19);
    h.set([0x4f, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64], 0); // "OpusHead"
    h[8] = 1;            // version
    h[9] = channels;     // channel count
    // preSkip (LE16) = 0 at [10..12]
    h[12] = sampleRate & 0xff;          // inputSampleRate LE32
    h[13] = (sampleRate >>> 8) & 0xff;
    h[14] = (sampleRate >>> 16) & 0xff;
    h[15] = (sampleRate >>> 24) & 0xff;
    // outputGain (LE16)=0 at [16..18], mapping family=0 at [18]
    return h;
}

function onDecodedAudio(audioData) {
    try {
        const channels = audioData.numberOfChannels;
        const frames = audioData.numberOfFrames;
        const rate = audioData.sampleRate;
        const channelData = [];
        for (let ch = 0; ch < channels; ch++) {
            const arr = new Float32Array(frames);
            audioData.copyTo(arr, { planeIndex: ch, format: 'f32-planar' });
            channelData.push(arr);
        }
        postPcmToWorklet(channelData, channels, rate);
    } catch (e) {
        console.warn('[RDPSND] decoded frame handling failed:', e);
    } finally {
        audioData.close();
    }
}

function ensureDecoder(codecStr, sampleRate, channels, description) {
    if (audioDecoder && audioDecoderCodec === codecStr &&
        audioDecoderRate === sampleRate && audioDecoderChannels === channels) {
        return audioDecoder;
    }
    if (audioDecoder) {
        try { audioDecoder.close(); } catch (_) {}
        audioDecoder = null;
    }
    try {
        audioDecoder = new AudioDecoder({
            output: onDecodedAudio,
            error: (e) => console.warn('[RDPSND] AudioDecoder error:', e),
        });
        const config = { codec: codecStr, sampleRate, numberOfChannels: channels };
        if (description && description.length) config.description = description;
        audioDecoder.configure(config);
        audioDecoderCodec = codecStr;
        audioDecoderRate = sampleRate;
        audioDecoderChannels = channels;
    } catch (e) {
        console.warn('[RDPSND] AudioDecoder configure failed:', e);
        audioDecoder = null;
    }
    return audioDecoder;
}

// codec: 0x0001 PCM, 0x704F Opus, 0xA106 AAC. extradata = codec config (AAC ASC).
window.__rdp_audio_data = function(codec, channels, sampleRate, bitsPerSample, uint8Array, extradata) {
    if (!audioContext || !audioWorkletReady) return; // drop packets before the worklet is ready

    if (audioContext.state === 'suspended') {
        audioContext.resume();
    }

    // Track the negotiated codec for the HUD.
    const codecName = codec === 0x704F ? 'Opus' : codec === 0xA106 ? 'AAC' : codec === 0x0001 ? 'PCM' : '?';
    if (codecName !== currentAudioCodec) {
        currentAudioCodec = codecName;
        if (hudAudioCodec) hudAudioCodec.textContent = codecName;
    }

    if (codec === 0x0001) {
        // PCM: deinterleave to per-channel Float32, then hand to the worklet.
        const bytesPerSample = bitsPerSample / 8;
        const totalSamples = Math.floor(uint8Array.length / bytesPerSample);
        const numFrames = Math.floor(totalSamples / channels);
        if (numFrames === 0) return;

        const channelData = [];
        if (bitsPerSample === 16) {
            const samples = new Int16Array(uint8Array.buffer, uint8Array.byteOffset, totalSamples);
            const scale = 1.0 / 32768.0;
            for (let ch = 0; ch < channels; ch++) {
                const arr = new Float32Array(numFrames);
                for (let i = 0; i < numFrames; i++) arr[i] = samples[i * channels + ch] * scale;
                channelData.push(arr);
            }
        } else if (bitsPerSample === 8) {
            for (let ch = 0; ch < channels; ch++) {
                const arr = new Float32Array(numFrames);
                for (let i = 0; i < numFrames; i++) arr[i] = (uint8Array[i * channels + ch] - 128) / 128.0;
                channelData.push(arr);
            }
        } else {
            return;
        }
        postPcmToWorklet(channelData, channels, sampleRate);
        return;
    }

    // Compressed codecs: decode via WebCodecs, output handler feeds the worklet.
    let codecStr, description;
    if (codec === 0x704F) {
        codecStr = 'opus';
        description = (extradata && extradata.length) ? extradata : makeOpusHead(channels, sampleRate);
    } else if (codec === 0xA106) {
        codecStr = 'mp4a.40.2';
        description = (extradata && extradata.length) ? extradata : null;
    } else {
        return;
    }

    const dec = ensureDecoder(codecStr, sampleRate, channels, description);
    if (!dec) return;
    try {
        dec.decode(new EncodedAudioChunk({
            type: 'key',
            timestamp: audioPts,
            duration: 0,
            data: uint8Array,
        }));
        audioPts += 20000; // ~20 ms; only needs to be monotonic
    } catch (e) {
        console.warn('[RDPSND] decode() failed:', e);
    }
};

// Called from WASM on a RDPSND Volume PDU. left/right are 0..0xFFFF per channel.
window.__rdp_audio_volume = function(left, right) {
    if (!audioGain) return;
    const g = Math.max(left, right) / 0xFFFF;
    audioGain.gain.value = Math.max(0, Math.min(1, g));
};

// ── Init ─────────────────────────────────────────────────
loadWasm();
