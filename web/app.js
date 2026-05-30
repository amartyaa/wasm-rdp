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
let nextPlayTime = 0;

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
    session = await wasm.connect(wsUrl, username, password, domain, width, height, 'rdp-canvas');

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
    // Keyboard
    document.addEventListener('keydown', onKeyDown, true);
    document.addEventListener('keyup', onKeyUp, true);
    window.addEventListener('blur', releaseAllModifiers);

    // Mouse
    canvas.addEventListener('mousemove', onMouseMove);
    canvas.addEventListener('mousedown', onMouseDown);
    canvas.addEventListener('mouseup', onMouseUp);
    canvas.addEventListener('wheel', onWheel, { passive: false });
    canvas.addEventListener('contextmenu', (e) => e.preventDefault());

    // Clipboard paste (local → remote)
    document.addEventListener('paste', onPaste);
    // Clipboard copy (remote → local fallback for HTTP)
    document.addEventListener('copy', onCopy);
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
    // Close audio context
    if (audioContext) {
        audioContext.close().catch(() => {});
        audioContext = null;
        nextPlayTime = 0;
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
        hudResolution.textContent = `${session.width}×${session.height}`;
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
function initAudioContext() {
    if (!audioContext) {
        try {
            audioContext = new AudioContext();
            nextPlayTime = 0;
            console.log('[RDPSND] AudioContext created, sampleRate:', audioContext.sampleRate);
        } catch (e) {
            console.warn('[RDPSND] Failed to create AudioContext:', e);
        }
    }
}

window.__rdp_audio_data = function(channels, sampleRate, bitsPerSample, uint8Array) {
    if (!audioContext) return;

    // Resume if suspended (autoplay policy)
    if (audioContext.state === 'suspended') {
        audioContext.resume();
    }

    const bytesPerSample = bitsPerSample / 8;
    const totalSamples = Math.floor(uint8Array.length / bytesPerSample);
    const numFrames = Math.floor(totalSamples / channels);
    if (numFrames === 0) return;

    // Create audio buffer
    const audioBuffer = audioContext.createBuffer(channels, numFrames, sampleRate);

    // Use Int16Array view for bulk access (avoids per-sample DataView.getInt16 overhead)
    const samples = new Int16Array(uint8Array.buffer, uint8Array.byteOffset, totalSamples);
    const scale = 1.0 / 32768.0;

    for (let ch = 0; ch < channels; ch++) {
        const channelData = audioBuffer.getChannelData(ch);
        for (let i = 0; i < numFrames; i++) {
            channelData[i] = samples[i * channels + ch] * scale;
        }
    }

    // Schedule back-to-back playback
    const source = audioContext.createBufferSource();
    source.buffer = audioBuffer;
    source.connect(audioContext.destination);

    const now = audioContext.currentTime;
    if (nextPlayTime < now) {
        nextPlayTime = now + 0.05; // 50ms pre-buffer to absorb jitter
    }
    source.start(nextPlayTime);
    nextPlayTime += audioBuffer.duration;
};

// ── Init ─────────────────────────────────────────────────
loadWasm();
