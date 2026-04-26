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

// ── State ────────────────────────────────────────────────
let frameCount = 0;
let lastFpsUpdate = performance.now();
let toolbarTimeout = null;
let lastMouseTime = 0;
let resizeTimeout = null;
const MOUSE_THROTTLE_MS = 16; // ~60fps cap on mouse events
const RESIZE_DEBOUNCE_MS = 250;

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

        const width = window.innerWidth;
        const height = window.innerHeight;

        // Build WebSocket URL for the proxy
        const proto = location.protocol === 'https:' ? 'wss' : 'ws';
        const basePath = location.pathname.replace(/\/[^/]*$/, '');
        const wsUrl = `${proto}://${location.host}${basePath}/ws`;

        session = await wasm.connect(wsUrl, username, password, domain, width, height, 'rdp-canvas');

        // Connected — switch to canvas view
        loginScreen.hidden = true;
        canvasContainer.hidden = false;
        toolbar.hidden = false;

        resBadge.textContent = `${session.width}×${session.height}`;
        setupInputHandlers();
        setupResizeHandler();
        startFpsCounter();
        showToolbar();

    } catch (err) {
        showError(String(err));
        setConnecting(false);
        // Exit fullscreen on error
        if (document.fullscreenElement) {
            document.exitFullscreen().catch(() => {});
        }
    }
});

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
    // Keyboard
    document.addEventListener('keydown', onKeyDown, true);
    document.addEventListener('keyup', onKeyUp, true);

    // Mouse
    canvas.addEventListener('mousemove', onMouseMove);
    canvas.addEventListener('mousedown', onMouseDown);
    canvas.addEventListener('mouseup', onMouseUp);
    canvas.addEventListener('wheel', onWheel, { passive: false });
    canvas.addEventListener('contextmenu', (e) => e.preventDefault());

    // Clipboard paste (local → remote)
    document.addEventListener('paste', onPaste);
}

function onKeyDown(e) {
    if (!session) return;

    // Intercept Ctrl+Shift+F → fullscreen toggle
    if (e.ctrlKey && e.shiftKey && e.code === 'KeyF') {
        e.preventDefault();
        toggleFullscreen();
        return;
    }
    // Intercept Ctrl+Shift+D → disconnect
    if (e.ctrlKey && e.shiftKey && e.code === 'KeyD') {
        e.preventDefault();
        disconnect();
        return;
    }
    // Remap Ctrl+Tab → Alt+Tab
    if (e.ctrlKey && e.code === 'Tab') {
        e.preventDefault();
        sendAltTab();
        return;
    }

    e.preventDefault();
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
    const rect = canvas.getBoundingClientRect();
    const scaleX = canvas.width / rect.width;
    const scaleY = canvas.height / rect.height;
    return {
        x: Math.round((e.clientX - rect.left) * scaleX),
        y: Math.round((e.clientY - rect.top) * scaleY),
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
    const text = e.clipboardData?.getData('text/plain');
    if (text) {
        try {
            wasm.clipboard_paste(text);
        } catch (err) {
            console.warn('Clipboard paste to WASM failed:', err);
        }
    }
}

// ── Resize Handler ───────────────────────────────────────
// Disabled: xrdp does not support the Display Control Virtual Channel,
// so dynamic resize causes a black screen. Canvas stays fixed at the
// negotiated RDP resolution.
function setupResizeHandler() {
    // no-op: keep canvas at the server-negotiated size
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
    if (session) {
        session.shutdown();
        session = null;
    }
    canvasContainer.hidden = true;
    toolbar.hidden = true;
    toolbar.classList.remove('visible');
    loginScreen.hidden = false;
    setConnecting(false);
    if (document.fullscreenElement) {
        document.exitFullscreen().catch(() => {});
    }
    // Remove input handlers
    document.removeEventListener('keydown', onKeyDown, true);
    document.removeEventListener('keyup', onKeyUp, true);
    document.removeEventListener('paste', onPaste);
}

// ── FPS Counter ──────────────────────────────────────────
function startFpsCounter() {
    // The WASM module calls back on each graphics update; we count those
    // For now, use a simple timer-based approach
    setInterval(() => {
        fpsBadge.textContent = `${frameCount} FPS`;
        frameCount = 0;
    }, 1000);
}

// Called from WASM on each graphics update
window.__rdp_frame = function() {
    frameCount++;
};

// ── Init ─────────────────────────────────────────────────
loadWasm();
