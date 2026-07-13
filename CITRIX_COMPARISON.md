# IronBridge vs. Citrix — Feature Gap Analysis & Improvement Roadmap

> **Scope.** A blunt engineering gap analysis of **IronBridge** (this repo — a browser-native
> RDP client: the full IronRDP state machine compiled to WASM, fronted by a dumb Rust
> WebSocket↔TCP proxy) against **Citrix Virtual Apps and Desktops / DaaS** (the market
> reference for app/desktop delivery). Purpose: decide what to build next. Every IronBridge
> claim carries a `file:line` citation; every Citrix claim carries a docs.citrix.com source
> (see §8). Citrix state verified against current builds as of **July 2026** (CR 2603,
> LTSR 2507).
>
> **This document supersedes the overlapping (and now partly stale) claims in
> `PERFORMANCE_ANALYSIS.md` and `AUDIO_COMPARISON.md` — see §9.**

---

## 1. Verdict up front

**IronBridge and Citrix are not the same class of product, and the gap analysis only makes
sense once that is said plainly.** Citrix is a *brokered, agent-based platform*: a Virtual
Delivery Agent (VDA) runs inside every session host, a control plane brokers and load-balances
sessions, a gateway terminates a proprietary UDP transport (EDT), and a rich native client
(Workspace app) redirects dozens of local devices. IronBridge is a *zero-agent, zero-install
protocol bridge*: it speaks stock RDP to any unmodified Windows/xrdp host, runs the entire
protocol in the browser tab, and the server component never parses a single PDU
([server/src/main.rs:255-261](server/src/main.rs#L255)).

So the honest framing is **not** "IronBridge vs. the Citrix platform" (control plane,
provisioning, analytics — IronBridge attempts none of it, by charter). It is **"IronBridge vs.
the Citrix session-delivery datapath, and specifically vs. Citrix Workspace app for HTML5"** —
Citrix's own browser client, which is the only apples-to-apples comparator. Against *that*
target, IronBridge is remarkably close on the core datapath (graphics, seamless apps,
clipboard-with-files, audio) and predictably far behind on the peripheral-redirection long tail
and on everything that requires a server-side agent or a native client process.

### 1.1 Domain scorecard

Legend: ✅ parity/competitive · 🟡 partial · 🔴 missing · ⚪ deliberate architectural non-goal.

| Domain | IronBridge | vs. CWA-HTML5 | vs. native Workspace app | Notes |
|---|:--:|:--:|:--:|---|
| Graphics datapath (codecs) | ✅ | ✅ | 🟡 | AVC420/WebCodecs HW decode; no AV1/H.265 (GPU-only anyway) — §4.1 |
| Adaptive display / frame pacing | 🔴 | 🟡 | 🔴 | FPS cap **inert**, no rAF coalescing — §4.1, §7 Tier 0 |
| Multi-monitor | ✅ | ✅ | ✅ | getScreenDetails + popup-per-monitor — §4.1 |
| HiDPI / display scaling | 🔴 | 🟡 | ✅ | `desktop_scale_factor: 0` — §4.1 |
| Dynamic resize | 🔴 | ✅ | ✅ | deliberate no-op (xrdp limitation) — §4.1 |
| Seamless published apps | 🟡 | ✅ | ✅ | RAIL v1 shipped; classic-only, single-app — §4.2 |
| Keyboard | ✅ | ✅ | ✅ | full scancode map + extended keys — §4.3 |
| IME / CJK / unicode input | 🔴 | ✅ | ✅ | no composition events — §4.3, §7 Tier 1 |
| Touch / pen | 🔴 | 🟡 | ✅ | no touch/pointer handlers — §4.3 |
| Audio out | ✅ | ✅ | ✅ | PCM+Opus+AAC, jitter-buffered — §4.4 |
| Microphone in | 🔴 | 🟡 | ✅ | playback-only — §4.4, §7 Tier 1 |
| Webcam / UC offload (Teams) | 🔴 | 🟡 | ✅ | §4.4, mostly ⚪ |
| Clipboard (text+image+**files**) | ✅ | ✅ | ✅ | files both ways; no HTML fmt — §4.6 |
| Drive redirection | 🔴 | 🟡 | ✅ | none; File System Access API feasible — §4.5, §7 Tier 2 |
| Printing | 🔴 | ✅ | ✅ | none — §4.5 |
| Generic USB / smartcard | 🔴 | 🟡 | ✅ | none; browser-limited — §4.5, mostly ⚪ |
| Transport resilience | 🟡 | 🟡 | 🔴 | TCP-only + reconnect; no EDT/session-reliability — §4.7 |
| Web-tier authentication | 🔴 | ✅ | ✅ | **zero auth on `/ws`** — §4.8, §7 Tier 0 |
| App Protection / watermark / recording | 🔴 | 🟡 | ✅ | none — §4.8 |
| Control plane / provisioning / analytics | ⚪ | ⚪ | ⚪ | explicit non-goal — §4.9 |
| Zero-install / zero-agent | ✅✅ | ✅ | 🔴 | IronBridge's core win — §5 |

### 1.2 The three findings that matter most

1. **Zero authentication at the web tier.** `/ws` upgrades and proxies *anyone* who can reach
   it straight to the fixed `--rdp-target`, with no token, cookie, origin, or per-user check
   ([main.rs:255-261](server/src/main.rs#L255)). Credentials are then validated only by the RDP
   host itself. Citrix's entire access story (Gateway, adaptive auth, MFA, SmartAccess) sits in
   front of the datapath. This is IronBridge's single largest gap for any deployment outside a
   trusted network — see §4.8 and §7 Tier 0.
2. **The "60 FPS" setting is a no-op.** The FPS-cap dropdown is plumbed all the way to
   `run_session`, where it lands as the unused `_fps_cap` ([session.rs:1946](wasm/src/session.rs#L1946)),
   and there is no `requestAnimationFrame` coalescing anywhere in the render path (0 grep hits) —
   every dirty region paints synchronously. IronBridge is *faster to first pixel* than an
   rAF-batched client but has *no frame pacing and no overdraw control*, which is exactly the
   opposite trade-off Citrix's Adaptive Display makes. §4.1, §7 Tier 0.
3. **RAIL shipped, and it lands IronBridge in Citrix's core category.** Seamless single-app
   publishing works end-to-end on Windows 11 ([crates/ironrdp-rail/](crates/ironrdp-rail/),
   [wasm/src/rail.rs](wasm/src/rail.rs)). The remaining RAIL gaps (icons, taskbar, Enhanced/HIDEF
   per-window EGFX, multi-app) are incremental, not architectural. §4.2, §7 Tier 1.

---

## 2. What Citrix actually is (so the comparison stays honest)

Citrix Virtual Apps and Desktops (CVAD, on-prem) / Citrix DaaS (cloud-brokered) is a stack of
~10 cooperating components. Most of them have **no IronBridge counterpart by design** — listing
them prevents the rest of this document from silently comparing a protocol bridge to a data
center.

| Citrix layer | Role | IronBridge counterpart |
|---|---|---|
| **Workspace app** (Win/Mac/Linux/ChromeOS/**HTML5**/mobile) | The client; renders HDX, redirects local devices | The browser tab + WASM module — closest to **CWA-HTML5** |
| **VDA** (Virtual Delivery Agent) | Server-side agent in every session host; runs HDX, virtual channels, seamless engine | **None** — IronBridge talks stock RDP to an unmodified host |
| **Delivery Controller / DaaS broker** | Brokers sessions, load-balances, enumerates resources per user | **None** — fixed `--rdp-target`, no brokering ([main.rs:35](server/src/main.rs#L35)) |
| **StoreFront / Workspace** | Resource catalog, SSO, app enumeration | Login form + optional static app catalog (`--app`) ([main.rs:71](server/src/main.rs#L71)) |
| **Citrix Gateway / Gateway Service (CGS)** | TLS/EDT termination, adaptive auth, MFA, SmartAccess | The proxy — but **no auth layer** ([main.rs:255](server/src/main.rs#L255)) |
| **Director / Analytics** | Ops monitoring, session diagnostics, UX telemetry | In-browser performance HUD only ([app.js:1388](web/app.js#L1388)) |
| **Studio** | Admin console, policy authoring | CLI flags + env vars |
| **MCS / PVS** | Machine provisioning (golden image → fleet) | **None** (⚪) |
| **WEM** (Workspace Environment Mgmt) | Logon optimization, resource management | **None** (⚪) |
| **Session Recording / Watermark / App Protection** | Compliance & anti-exfiltration | **None** ([grep: absent]) |

**Takeaway:** §4.9 (management/scale) is almost entirely ⚪ non-goals and is treated briefly.
The substance of this report is the **datapath** (§4.1–4.8), where IronBridge genuinely competes,
and where the fair yardstick is **CWA-HTML5**, itself a deliberately reduced subset of the native
Workspace app.

---

## 3. Architecture comparison

### 3.1 Two fundamentally different shapes

```
CITRIX (brokered, agent-based)
  Browser/Native client ── HDX/ICA over EDT(UDP)/TCP ──► Gateway ──► VDA (agent in host)
        │                                                   │            │
     redirects local devices                        auth/MFA/SmartAccess │ seamless engine,
     (USB, drives, cams…)                                                │ virtual channels,
                                                                         │ policy engine
  Control plane: Broker + StoreFront + Director + Studio + MCS/PVS (separate infrastructure)

IRONBRIDGE (agentless, protocol bridge)
  Browser tab (WASM: full RDP state machine) ── WebSocket/TLS ──► Rust proxy ── TCP/TLS ──► stock RDP host
        │                                                             │                       (Win11 / xrdp / GRD)
     owns the entire protocol:                              dumb pipe + TLS upgrade only
     CredSSP, decode, channels, input                       (never parses a PDU)
```

The defining inversion: **Citrix pushes intelligence into a server-side agent (VDA); IronBridge
pushes it into the client (WASM).** Citrix's model buys deep host integration (any local device
can be projected into the session because the VDA implements the server half); IronBridge's model
buys zero host footprint (it works against any RFC-compliant RDP server, including ones nobody
installed anything on) at the cost of being limited to what stock RDP virtual channels + browser
APIs can express.

### 3.2 Consequence table

| Property | Citrix | IronBridge | Who wins |
|---|---|---|---|
| Host prerequisite | VDA install + control plane | Stock RDP enabled | **IronBridge** (agentless) |
| Client prerequisite | Workspace app (HTML5 = none) | None (any modern browser) | Tie vs HTML5, **IronBridge** vs native |
| Transport | EDT (UDP) w/ TCP fallback, gateway-terminated | WebSocket over TCP/TLS | **Citrix** (loss-resilient) |
| Device redirection breadth | Very high (VDA implements server side) | Limited to RDP static/dynamic VCs + browser APIs | **Citrix** |
| Deployment weight | Data-center stack | Single binary + static assets | **IronBridge** |
| Multi-tenant brokering | Native | None (fixed target) | **Citrix** |
| Attack surface at edge | Hardened gateway + auth | **Unauthenticated proxy** | **Citrix** (§4.8) |
| Trust model | Client trusts gateway/VDA identity | Proxy is a *designed* TLS MITM; WASM does the real endpoint validation… except it doesn't fully (§4.8) | **Citrix** |

### 3.3 Where the datapath actually diverges

Both stacks ultimately move three things: **pixels, input, and virtual-channel data**. Citrix's
HDX is a superset transport with adaptive codecs, EDT, and ~40 virtual channels. IronBridge rides
RDP's own graphics pipeline (EGFX + legacy) and a handful of virtual channels (CLIPRDR, RDPSND,
DRDYNVC, DisplayControl, RAIL). The rest of §4 walks each datapath domain.

---

## 4. Feature-by-feature gap analysis

Each subsection: **Citrix capability → IronBridge status (with evidence) → gap severity for our
charter targets (Win11 headless GPU-less, Ubuntu GNOME Remote Desktop, Ubuntu xrdp) → browser/
protocol feasibility.**

### 4.1 Graphics & display

**Citrix (HDX Thinwire + video codec).** HDX chooses per-region between Thinwire (lossless,
CPU-friendly, text-optimized) and a full-screen video codec (H.264/H.265/AV1). **Adaptive Display
v2** dynamically balances frame rate vs. image quality vs. bandwidth; **Build-to-Lossless**
encodes with a video codec during motion then sharpens to pixel-perfect when motion stops. AV1 and
H.265 are **GPU-only** — AV1 encode requires NVIDIA Ada Lovelace-class GPUs and driver ≥522.25;
neither can be used with CPU encoding ([Citrix HDX Graphics], [Citrix codec blog]). Session
watermark and 3D-Pro GPU acceleration layer on top.

**IronBridge.** A genuinely strong browser graphics pipeline, wired end-to-end:

| Codec | Path | Evidence |
|---|---|---|
| Legacy bitmap (RGBA) | `DecodedImage` → canvas blit | [session.rs:1950](wasm/src/session.rs#L1950), [session.rs:2116](wasm/src/session.rs#L2116) |
| RemoteFX (RFX) fast-path | default, legacy DecodedImage path | [session.rs:935](wasm/src/session.rs#L935) |
| **RFX-Progressive** | EGFX `on_progressive_data` (ported decoder) | [session.rs:287](wasm/src/session.rs#L287) |
| Planar (RDP6), ClearCodec, Uncompressed | EGFX `Codec1Type` | [session.rs:338-341](wasm/src/session.rs#L338) |
| **AVC420 / H.264** | EGFX → **WebCodecs `VideoDecoder`** per surface (HW decode) | [session.rs:1394](wasm/src/session.rs#L1394), [app.js:1922](web/app.js#L1922) |
| AVC444 | **absent** (by design) | not in `VideoCodec` enum ([session.rs:73-90](wasm/src/session.rs#L73)) |
| NSCodec (standalone) | **absent** — exists only as a ClearCodec *subcodec* in the fork decoder, never advertised as a top-level codec | enum ([session.rs:73-90](wasm/src/session.rs#L73)) |

EGFX capability advertisement is conditional: `V8.1 {AVC420_ENABLED|SMALL_CACHE} + V8` when the
browser's H.264 probe passes, else `V8 {SMALL_CACHE}` only ([session.rs:156-186](wasm/src/session.rs#L156)).
This is the **AVC420-over-WebCodecs passthrough** — arguably IronBridge's most Citrix-like graphics
feature: it hands H.264 straight to the GPU's hardware decoder via `VideoDecoder`, exactly the
"video codec for the whole screen" strategy, but on the *client* GPU with no *server* GPU required
for decode.

**The two real graphics gaps:**

1. **Frame pacing is absent, and the FPS cap is a lie.** The 15/30/60/120 dropdown
   ([index.html:45-50](web/index.html#L45), default 60) is threaded through `connect()` into
   `run_session`, where it becomes the **unused** `_fps_cap` ([session.rs:1946](wasm/src/session.rs#L1946)).
   There is **no `requestAnimationFrame` coalescing** anywhere (0 grep hits); `notify_frame()`
   fires synchronously on every graphics update ([session.rs:2139](wasm/src/session.rs#L2139)).
   Consequence vs. Citrix Adaptive Display: IronBridge cannot cap frame rate to save bandwidth,
   cannot coalesce a burst of dirty rects into one composite paint, and has no quality/FPS knob.
   `PERFORMANCE_ANALYSIS.md §3` still flags this as open — it is (§9).
2. **No HiDPI / display scaling.** `desktop_scale_factor: 0` ([session.rs:2407](wasm/src/session.rs#L2407))
   and no `devicePixelRatio` use anywhere. On a 4K/Retina display the remote desktop renders at CSS
   pixels, not device pixels — text is soft. Citrix supports DPI matching and per-monitor scaling.

**Multi-monitor: parity, and genuinely nice.** IronBridge uses the Window Management API
`getScreenDetails()` ([app.js:947](web/app.js#L947)), opens one popup window per secondary display
([app.js:1001-1020](web/app.js#L1001)), maps N one-per-monitor surfaces
([session.rs:1358](wasm/src/session.rs#L1358)), computes the combined desktop as the bounding box
([session.rs:2301](wasm/src/session.rs#L2301)), and relayouts live on `screenschange` via
DisplayControl ([app.js:1069](web/app.js#L1069), [session.rs:2313](wasm/src/session.rs#L2313)).
CWA-HTML5 caps at two external monitors and Chrome/Edge-only; IronBridge has no hard cap (bounded by
u16 desktop dims) but shares the Chromium-only constraint. **Even here IronBridge matches its fair
comparator.**

**Dynamic resize: deliberate no-op.** `setupResizeHandler()` intentionally keeps the canvas at the
server-negotiated size ([app.js:937-939](web/app.js#L937)) because xrdp lacks the Display Control VC
and would black-screen; `encode_resize` exists ([session.rs:1983](wasm/src/session.rs#L1983)) but
nothing drives it on browser resize. This is a charter-driven choice (xrdp is a first-class target),
not an oversight — but it is a real UX gap on Windows hosts, which *do* support RDPEDISP. §7 Tier 1
proposes making it conditional.

**Fullscreen & cursor:** Fullscreen API with pre-connect + Ctrl+Shift+F + per-monitor-popup
fullscreen ([app.js:363,704,1047](web/app.js#L363)); custom hardware cursors via `PointerBitmap` →
CSS data-URL with a 32-entry FNV-1a shape cache ([session.rs:2152](wasm/src/session.rs#L2152),
[canvas.rs:269](wasm/src/canvas.rs#L269)); cursor rendered as an accelerated overlay, not composited
into the framebuffer ([session.rs:2405](wasm/src/session.rs#L2405)). This is competitive.

> **Per-target impact.** Win11 headless GPU-less: AVC420 client-decode works without a server GPU
> (the whole charter) ✅; frame-pacing gap hurts most here (a headless host can flood updates).
> GRD/xrdp: RFX-Progressive/legacy path carries them; HiDPI + resize gaps apply equally; xrdp is the
> reason resize is disabled. **Perf note:** the missing rAF coalescing is a *latency win* (no
> deferral) but an *efficiency loss* (overdraw, no batching) — §7 Tier 0 must preserve the former.

### 4.2 App & desktop delivery (seamless published apps)

**Citrix.** Seamless "published apps" are the flagship: a server-side seamless engine tracks every
top-level window (geometry, z-order, icon, title, focus, taskbar grouping) and streams that metadata
over a virtual channel while pixels flow over Thinwire; the client renders each remote window as a
local window with a taskbar entry and icon. Plus session prelaunch/lingering, app roaming across
devices, anonymous/pre-launched sessions, and workspace control (disconnect/reconnect roaming).

**IronBridge — RAIL v1, shipped.** The RDP-native equivalent (MS-RDPERP RemoteApp / RAIL) is
implemented and works end-to-end on Windows 11:

- Vendored `crates/ironrdp-rail/` — `rail` SVC client (Handshake → ClientStatus → SysParam
  (HighContrast off) → SysParam (WorkArea) → Exec) ([client.rs:61-78](crates/ironrdp-rail/src/client.rs#L61))
  + Window Info Orders parser ([crates/ironrdp-rail/src/window.rs](crates/ironrdp-rail/src/window.rs)).
- Per-window rendering: each remote top-level window is an absolutely-positioned `<canvas>` in
  `#rail-desktop`; move via CSS transform, size, z-order top-first, visibility-rect → CSS `clip-path`
  ([app.js:1458-1523](web/app.js#L1458)).
- UX: opt-in picker/catalog + `?app=` deep link ([app.js:383-417](web/app.js#L383)); auto-maximize the
  first window the server marks foreground ([app.js:1531](web/app.js#L1531)); app-exit → debounced
  session-end ([app.js:1549](web/app.js#L1549)); tab-close guard ([app.js:1634](web/app.js#L1634)).

**RAIL gaps vs. Citrix seamless (all incremental, none architectural):**

| Capability | Citrix | IronBridge RAIL v1 | Path |
|---|:--:|:--:|---|
| Per-window pixels | ✅ | 🟡 classic (full-desktop framebuffer clipped per window; EGFX force-off in rail mode, [session.rs:847-853](wasm/src/session.rs#L847)) | HIDEF/Enhanced RAIL (§7 Tier 1) |
| Window icons | ✅ | 🔴 skipped ([window.rs:208-210](crates/ironrdp-rail/src/window.rs#L208)) | parse ICON orders |
| Taskbar / systray / notify | ✅ | 🔴 skipped ([window.rs:211-213](crates/ironrdp-rail/src/window.rs#L211)) | UI work |
| Multiple apps per session | ✅ | 🔴 single `rail_program` ([session.rs:804](wasm/src/session.rs#L804)) | multi-exec |
| Local move/size | ✅ | 🔴 `ALLOWLOCALMOVESIZE` off ([client.rs:65-66](crates/ironrdp-rail/src/client.rs#L65)) | client info flag + handlers |
| Non-rectangular window regions | ✅ | 🟡 bounding-rect clip only when >1 vis rect ([app.js:1508-1518](web/app.js#L1508)) | multi-rect clip-path |
| Prelaunch / lingering / roaming | ✅ | 🔴 | out of scope v1 |

**Bottom line:** IronBridge is now *in* Citrix's marquee category. The seamless-window architecture
(metadata channel + per-window client compositing) is the same; what remains is polish (icons,
taskbar), fidelity (Enhanced RAIL per-window EGFX to kill overlap ghosting), and breadth (multi-app).
That is a roadmap, not a rewrite. See §7 Tier 1.

> **Per-target impact.** Windows-only feature by nature (xrdp/GRD RAIL is broken/absent — documented
> in memory). GPU-less Win11: classic RAIL works today; Enhanced RAIL needs no *server* GPU (EGFX
> surfaces decode client-side), so it stays on-charter. **Perf:** RAIL is *less* bandwidth than full
> desktop (no wallpaper/shell); the per-window canvas reuses the existing surface list, so the paint
> hot path is unchanged.

### 4.3 Input

**Citrix.** Full keyboard with **dynamic keyboard-layout sync** and **Generic Client IME** for CJK
(sync mode 4), **multi-touch** (gestures, up to the platform limit), **pen/stylus with Windows Ink**
(pressure, eraser, Bluetooth barrel button), relative-mouse mode for games/CAD, and mobile-tuned
touch UX ([Citrix keyboard/IME], [Citrix mobile devices], [Citrix touch]).

**IronBridge.**

- **Keyboard: competitive.** `SCANCODE_MAP` covers AT-101 alphanumerics/F-keys/numpad/modifiers
  ([app.js:289-327](web/app.js#L289)) with a full **extended-key** block (NumpadEnter, arrows,
  Home/End/PgUp/PgDn/Ins/Del, Meta, ContextMenu, PrintScreen, Pause) ([app.js:319-326](web/app.js#L319)).
  Shortcut remaps (Ctrl+Shift+F fullscreen, Ctrl+Shift+D disconnect, Ctrl+Tab→Alt+Tab, toolbar
  Ctrl+Alt+Del) ([app.js:704-724,1081](web/app.js#L704)) and **anti-stuck modifiers** releasing all
  four modifiers on window blur ([app.js:688-698](web/app.js#L688)).
- **Mouse: competitive.** 5 buttons incl. X1/X2 ([session.rs:1256](wasm/src/session.rs#L1256)),
  **both vertical and horizontal wheel** with deltaMode-aware scaling + accumulation
  ([app.js:818-837](web/app.js#L818)), 16 ms (~60 fps) movement throttle ([app.js:220](web/app.js#L220)).

**Input gaps:**

1. **No IME / composition / unicode input.** No `compositionstart`/`beforeinput`/`isComposing`
   handling (0 grep hits); `keyboard_layout: 0`, empty `ime_file_name`
   ([session.rs:2385-2387](wasm/src/session.rs#L2385)). CJK/complex-script users cannot type. This is
   the single biggest input gap for international use. §7 Tier 1.
2. **No touch, no pen, no pointer events.** Only mouse/wheel listeners exist (0 touch/pointer grep
   hits). On a tablet or touchscreen laptop, IronBridge is mouse-emulation-only. Citrix has native
   multitouch + Windows Ink. §7 Tier 1 (touch via RDPEI).
3. **No relative-mouse / pointer-lock mode.** Absolute coordinates only — fine for desktops, poor for
   in-session games/3D.

**Copy/paste interception is clever:** Ctrl+C passes through for the native `copy` event; Ctrl+V is
*deferred* — the V keyup is swallowed, the host clipboard is advertised via CLIPRDR first, then after
100 ms the V keystroke is replayed ([app.js:736-745,775-781](web/app.js#L736)). This solves the
browser-clipboard-async-vs-RDP-sync race elegantly.

> **Per-target impact.** IME gap is host-agnostic (affects Win11/xrdp/GRD equally). Touch gap matters
> most for GRD/Wayland tablets and Win11 convertibles. **Perf:** IME/touch are low-rate input events —
> negligible cost; the risk is correctness (composition state machines), not performance.

### 4.4 Audio & unified communications

**Citrix.** **Adaptive Audio** (bidirectional CTXCAM virtual channel, enabled by default, replaces
the old quality-tier policies), **client microphone redirection**, EDT-lossy mode for real-time audio
on lossy networks, webcam redirection (HDX RealTime video compression), and the big one —
**optimized UC**: the new Microsoft Teams VDI plugin based on Microsoft's **SlimCore** media engine
(GA on Windows + Mac as of mid-2026; the older HDX WebRTC `HdxRtcEngine` is deprecated, end of support
Oct 2026), plus Zoom/Webex VDI plugins. These *offload* the media to the endpoint so audio/video
peer-to-peer never traverses the session ([Citrix audio], [Citrix Teams SlimCore], [MS new-Teams VDI]).

**IronBridge — audio-out is genuinely strong; everything else is absent.**

- **Playback:** PCM always, plus **Opus (0x704F)** and **AAC (0xA106)** advertised compressed-first
  when the browser's WebCodecs probe supports them ([audio.rs:24-35](wasm/src/audio.rs#L24),
  [app.js:1682-1696](web/app.js#L1682)). Registered on **both** the static RDPSND SVC channel
  *and* the DVC (`AUDIO_PLAYBACK_DVC` over DRDYNVC) so the server can pick either
  ([session.rs:896-899,958-961](wasm/src/session.rs#L896)).
- **Quality engineering:** a pull-based **AudioWorklet ring buffer** with 40 ms target / 120 ms max
  latency and drop-to-catch-up, plus a linear-interpolation resampler with persistent phase
  ([audio-worklet.js:37-38,58-135](web/audio-worklet.js#L37)). Volume sync via RDPSND Volume PDU →
  GainNode ([audio.rs:71-74](wasm/src/audio.rs#L71)). This is a well-built, Citrix-Adaptive-Audio-like
  playback stack (see `AUDIO_COMPARISON.md`, though that doc is now stale — §9).
- **Microphone in: absent.** No AUDIO_INPUT / getUserMedia / MediaRecorder (0 grep hits); the handler
  is playback-only, `set_pitch` is a no-op ([audio.rs:76-78](wasm/src/audio.rs#L76)). This is the
  clearest Tier-1 audio gap (RDPEAI + getUserMedia — already on the project roadmap). §7 Tier 1.
- **Webcam / Teams offload: absent** and mostly a **⚪ non-goal** — UC offload fundamentally requires a
  vendor endpoint plugin (SlimCore is a native DLL); there is no browser-only path to Teams-optimized
  media. A generic RDPECAM webcam-redirect *is* browser-feasible (getUserMedia → encode), but at
  meeting quality it competes with a problem Citrix solves with a native process. §7 Tier 3.

> **Per-target impact.** Playback works on all three targets (RDPSND on Win11/xrdp; DVC path lets GRD
> move audio to `AUDIO_PLAYBACK_DVC`, per memory `audio-dvc-architecture`). Mic-in: Win11 & GRD accept
> RDPEAI; xrdp mic support is weak. **Perf:** mic adds one encode + one DVC upstream — modest, and it
> reuses the existing DRDYNVC plumbing; must not disturb the playback jitter buffer (separate channel).

### 4.5 Peripherals & local resources

This is Citrix's widest moat and IronBridge's emptiest column — because every item here needs a
**server-side agent** (the VDA implements the device's server half) or a **native client process**,
and IronBridge has neither.

| Peripheral | Citrix | IronBridge | Browser feasibility |
|---|:--:|:--:|---|
| Client drive / folder mapping (CDM / RDPDR) | ✅ | 🔴 (absent; only a doc-comment mention, [rdpsnd/client.rs:57](crates/ironrdp-rdpsnd/src/client.rs#L57)) | **Feasible** via File System Access API → RDPDR (§7 Tier 2) |
| Printing (Universal Print Driver / UPS) | ✅ | 🔴 | Feasible-ish: RDPDR print queue → browser PDF/print |
| Generic USB redirection | ✅ | 🔴 | **⚪** WebUSB cannot claim arbitrary device classes (no HID/mass-storage/smartcard) |
| Smartcard (SCARD) | ✅ | 🔴 | ⚪ no browser smartcard-in-session API |
| FIDO2 / WebAuthn in session | ✅ (redirection) | 🔴 | Hard: WebAuthn is origin-bound to the *browser*, not projectable into the session |
| Scanners / TWAIN, COM/LPT | ✅ | 🔴 | ⚪ |

**The honest split:** *drives* and *printing* are legitimately browser-feasible and worth doing (§7
Tier 2). *USB/smartcard/scanner/COM* are effectively **⚪ non-goals** — the browser sandbox has no API
to project a raw device class into a remote session, and even Citrix's own HTML5 client leans on the
native app or ChromeOS-specific paths for most of these. Chasing them would fight the platform.

> **Per-target impact.** Drive redirection: Win11 & xrdp both implement the RDPDR server side; GRD's
> RDPDR support is partial. **Perf:** file transfer is bursty and user-initiated; keep it off the
> graphics path (separate DVC), same discipline as the existing file-clipboard chunking.

### 4.6 Clipboard & data movement — **near-parity, an IronBridge strong point**

**Citrix.** Bidirectional clipboard with text, **HTML format** (Chrome/Safari, preserves Office/
browser formatting), images (with a ~2 MB practical limit on HTML5), and file transfer, all with
granular per-direction policy control ([Citrix HTML5 clipboard]).

**IronBridge.** Surprisingly complete ([wasm/src/clipboard.rs](wasm/src/clipboard.rs)):

- **Text** both directions (CF_UNICODETEXT ↔ `navigator.clipboard`) ([clipboard.rs:225,412](wasm/src/clipboard.rs#L225)).
- **Images** both directions (CF_DIBV5/CF_DIB ↔ PNG conversion) ([clipboard.rs:280-293,390-404](wasm/src/clipboard.rs#L280)).
- **Files both directions** — CLIPRDR FileGroupDescriptorW, 4 MB chunks, remote→local gated behind an
  explicit user "Download" click, local→remote incl. drag-drop upload
  ([clipboard.rs:451-538](wasm/src/clipboard.rs#L451), [app.js:663-684](web/app.js#L663)).
- **Server-side policy gates**: `--enable-text-clipboard-sync` / `--enable-file-clipboard-sync`, both
  default-off ([clipboard.rs:129-131](wasm/src/clipboard.rs#L129), [main.rs:53-59](server/src/main.rs#L53)).

**Only gap: no HTML clipboard format** (rich formatting is lost; text+image only). That is a genuine
but narrow gap — and IronBridge's *file* support actually **matches or exceeds CWA-HTML5**, whose file
handling is more constrained. This domain is a **win column**, not a gap column.

> **Per-target impact.** Text/image work everywhere CLIPRDR is present (all three targets). File
> transfer depends on host CLIPRDR file caps (Win11 full; xrdp/GRD vary). **Perf:** chunked + user-
> gated, off the hot path — already designed correctly.

### 4.7 Transport & resilience

**Citrix.** **EDT** (Enlightened Data Transport) — a proprietary reliable-UDP protocol that
outperforms TCP on lossy/high-latency links, with automatic TCP fallback (Adaptive Transport).
**Session Reliability (CGP, port 2598)** freezes the session UI and holds it server-side through
network blips (default 180 s) so reconnect is seamless; **Auto Client Reconnect** re-establishes
dropped sessions transparently. **Rendezvous v2** lets the client connect straight to the VDA
(bypassing gateway proxying); **HDX Direct** establishes direct client↔VDA links for internal users
([Citrix adaptive transport], [Citrix HDX Direct]).

**IronBridge.**

- **Transport: WebSocket over TCP/TLS only.** No UDP path (browsers can't do raw UDP; WebTransport/
  QUIC is the only avenue and isn't used). The proxy sets TCP_NODELAY and uses 64 KB buffers
  ([main.rs:285-287](server/src/main.rs#L285)). On a clean LAN this is fine; on lossy WAN it will lose
  to EDT.
- **Resilience: reconnect, not reliability.** Auto-reconnect with 5 attempts and 2/4/8/16/32 s backoff
  ([app.js:277-278](web/app.js#L277)); **flap detection** (a sub-12 s session counts as a flap; stop
  after 2, [app.js:282-285,1334](web/app.js#L282)); **takeover guard** — a `session_replaced`
  ("Another user connected") disconnect does *not* reconnect, to avoid two clients evicting each other
  on a single-session host ([session.rs:2172](wasm/src/session.rs#L2172), [app.js:1318](web/app.js#L1318),
  memory `reconnect-takeover-war`); and **deferred RDP server redirection** (RDSTLS) handled as a
  first-class reconnect ([app.js:1289](web/app.js#L1289), [session.rs:2179](wasm/src/session.rs#L2179)).
- **The gap:** IronBridge *reconnects* (new session, framebuffer rebuilt) where Citrix *preserves*
  (same session, frozen and resumed). After an IronBridge reconnect the RDP session is re-established
  from scratch; there is no server-side hold. A "session-reliability-lite" (freeze last framebuffer,
  fast-resume) is a Tier-2 idea (§7), but true EDT/CGP parity is architecturally out of reach for a
  TCP/WebSocket bridge.

> **Per-target impact.** All three targets ride TCP today; none get EDT benefits. **Perf:** WebTransport
> (QUIC) is the only browser path to loss-resilience and is experimental (§7 Tier 2) — it would help WAN
> users on all targets but adds a UDP-capable proxy path and a fallback matrix. The reconnect/flap/
> takeover logic is already more careful than a naive client — keep it.

### 4.8 Security & access — **IronBridge's most serious gap**

**Citrix.** Security is a whole product surface: **Citrix Gateway** (TLS termination, adaptive auth,
MFA, nFactor, SmartAccess conditional policies), **FIDO2/WebAuthn** in-session, deep NLA, **App
Protection** (kernel-level anti-keylogging that scrambles keystrokes, anti-screen-capture that blanks
the window against screenshot/recording tools — including blocking Microsoft Recall and AI screen-
capture on Copilot+ PCs — anti-DLL-injection, and policy-tampering detection default-on from CVAD 2511),
**text-based session watermark** (server-drawn, traceable, Thinwire-only), and a separate **Session
Recording** component (with lossy codec + playback justification logging) ([Citrix App Protection],
[Citrix App Protection AI], [Citrix session watermark], [Citrix Session Recording]).

**IronBridge.**

1. **Zero web-tier authentication — the headline risk.** `ws_handler` upgrades and proxies with no
   token, cookie, session, or origin check ([main.rs:255-261](server/src/main.rs#L255)); `/ping` is
   open; credentials pass straight through to the fixed RDP host. **Anyone who can reach the proxy
   gets an RDP pipe to the target.** There is no gateway, no SSO, no per-user target selection. For a
   LAN-only appliance this is arguably acceptable; for anything internet-facing it is the first thing
   an attacker finds. §7 Tier 0 makes this the #1 fix.
2. **Cert validation is effectively absent.** The proxy accepts *any* server cert
   (`danger_accept_invalid_certs`, `use_sni(false)`) ([main.rs:319-327](server/src/main.rs#L319)); the
   WASM client does **not** validate the chain or hostname — it only parses the DER to extract the
   SubjectPublicKeyInfo for CredSSP channel binding ([session.rs:1881-1894](wasm/src/session.rs#L1881)).
   The proxy is therefore a **designed TLS MITM**, and the endpoint-identity check that would make that
   safe (pinning the server public key) is not enforced end-to-end. Even the redirection path's cert-pin
   check is best-effort warn-only ([redirect.rs:114-134](wasm/src/redirect.rs#L114)).
3. **NLA is NTLM-only.** CredSSP runs `ClientMode::Ntlm` ([session.rs:1772](wasm/src/session.rs#L1772));
   Kerberos explicitly bails ([session.rs:1790-1794](wasm/src/session.rs#L1790)); SPN is hardcoded
   `TERMSRV/localhost` ([session.rs:1766](wasm/src/session.rs#L1766)). On Kerberos-only / NTLM-disabled
   hardened domains, IronBridge cannot connect at all. §7 Tier 1.
4. **No App Protection, watermark, or recording** (all absent). App Protection is architecturally
   impossible to fully replicate (it needs a kernel driver on the endpoint); but a **client-side
   session watermark** (CSS/canvas overlay with user/timestamp) is browser-feasible and cheap — a real
   Tier-2 compliance win, even if trivially bypassable vs. Citrix's server-drawn version.

**Net:** on the datapath IronBridge is competitive, but on **access security it is a different
league** — and the zero-auth proxy is the one gap that can block real deployment. It leads the roadmap.

> **Per-target impact.** Auth gap is host-agnostic (the proxy is the same for all three targets). NTLM-
> only hurts hardened Win11/AD domains most; xrdp/GRD typically accept NTLM. **Perf:** adding a web-tier
> auth handshake is per-connection, not per-frame — zero datapath cost.

### 4.9 Management, monitoring & scale — mostly ⚪ non-goals (stated plainly)

Citrix ships Studio (policy/admin), Director (ops monitoring), Autoscale (power management), MCS/PVS
(fleet provisioning), Local Host Cache (broker resiliency), zones, and Service Continuity. **IronBridge
attempts none of this, and shouldn't** — it is a single binary bridging one browser to one host. The
only counterparts are the in-browser **performance HUD** (proxy latency via `/ping`, frame age, FPS,
DL/UL, RX/TX, resolution + monitor count, video codec, audio codec, version — [app.js:1388-1410](web/app.js#L1388))
and byte counters ([session.rs:1301](wasm/src/session.rs#L1301)). These are ⚪ by charter; the only
management-adjacent item worth considering is a **mini-broker** (per-user target selection + catalog
auth), which is really an extension of §4.8's auth work, not a Citrix-control-plane clone. §7 Tier 2.

### 4.10 Client platform coverage

**Citrix.** Workspace app for Windows, Mac, Linux, ChromeOS, iOS, Android, **and HTML5** — the native
apps redirect the full device set; **HTML5 is a deliberately reduced subset** (multi-monitor Chrome/
Edge-only and capped at 2 external monitors; webcam Chrome/Edge-only; mic device-selection added only
in 2511; USB limited) ([Citrix HTML5 multi-monitor], [Citrix HTML5 multimedia]).

**IronBridge.** One client: **any modern Chromium browser** (secure context). Firefox/Safari lose
multi-monitor (`getScreenDetails` is Chromium-only) and some clipboard paths, but core RDP works. No
native apps, no mobile/touch UI, no PWA manifest/service worker (a viewport meta exists but there is no
touch input — [index.html:6](web/index.html#L6)). **The key insight:** IronBridge's fair comparator is
CWA-HTML5, and against that specific target IronBridge is *competitive on the core datapath and even
ahead on file-clipboard*, while sharing the same Chromium-only constraints for the advanced bits. The
native Workspace app's device breadth is out of a browser client's reach — for both of them.

---

## 5. Where IronBridge wins (honest differentiators)

Gap analysis cuts both ways. These are real, and some are things Citrix structurally *cannot* match:

1. **Zero client install.** A URL. No Workspace app, no plugin, no admin rights on the endpoint. CWA-
   HTML5 matches this; the native ecosystem does not.
2. **Zero host agent.** IronBridge speaks stock RDP to **any** unmodified host — Windows, xrdp, GNOME
   Remote Desktop. Citrix *requires a VDA* on every session host, plus the control plane. This is the
   deepest architectural difference: IronBridge has **no server-side software to install, patch, or
   license on the target**.
3. **One small binary vs. a data center.** The server is a single Rust executable that never parses a
   PDU ([main.rs:255-261](server/src/main.rs#L255)) and embeds its own web assets. No broker, no SQL,
   no StoreFront, no gateway appliance. Deployment is "copy one file + run" (or a Windows service,
   [main.rs:448-528](server/src/main.rs#L448)).
4. **Modern web pipeline, client-side GPU.** AVC420 → WebCodecs hands H.264 to the endpoint's hardware
   decoder ([session.rs:1394](wasm/src/session.rs#L1394)) — so a **GPU-less, headless server** can still
   deliver H.264 video, because decode happens on the *client* GPU. That is precisely the charter
   (GPU-less targets) and a place where IronBridge's architecture is genuinely elegant.
5. **Open source & auditable.** The entire stack — protocol state machine included — is readable Rust +
   JS, forkable and patchable (as this repo already does with IronRDP and rdpsnd). Citrix is closed.
6. **Cost & simplicity.** No per-user licensing, no infrastructure tax. For the "publish a couple of
   internal apps to a browser" use case, IronBridge is a fraction of the operational weight.
7. **Careful reconnect ethics.** The takeover/flap logic ([app.js:1318-1349](web/app.js#L1318)) avoids
   the single-session-host eviction war — a subtlety many clients get wrong.

**The positioning that falls out of this:** IronBridge is not trying to be Citrix. It is the
**agentless, zero-install, single-binary way to put a specific Windows/Linux app or desktop in a browser
tab** — strongest exactly where Citrix is heaviest (deployment weight, host prerequisites, cost), and
weakest exactly where Citrix invested a decade (peripheral redirection, access security, WAN transport,
management plane).

---

## 6. Consolidated scorecard

Legend: ✅ parity/competitive · 🟡 partial · 🔴 missing · ⚪ deliberate non-goal. "vs HTML5" = Citrix
Workspace app for HTML5 (the fair comparator). Evidence column = IronBridge.

| # | Capability | Citrix native | Citrix HTML5 | IronBridge | vs HTML5 | Evidence (IronBridge) |
|--:|---|:--:|:--:|:--:|:--:|---|
| 1 | Legacy/RFX graphics | ✅ | ✅ | ✅ | = | session.rs:935,1950 |
| 2 | RFX-Progressive | ✅ | ✅ | ✅ | = | session.rs:287 |
| 3 | H.264/AVC420 (HW decode) | ✅ | ✅ | ✅ | = | session.rs:1394, app.js:1922 |
| 4 | AVC444 | ✅ | 🟡 | 🔴 | − | enum session.rs:73 |
| 5 | H.265 / AV1 | ✅(GPU) | 🟡 | 🔴 | − | GPU-only; ⚪ for GPU-less charter |
| 6 | Adaptive display / frame pacing | ✅ | 🟡 | 🔴 | − | `_fps_cap` unused session.rs:1946 |
| 7 | Multi-monitor | ✅ | ✅(≤2) | ✅ | = / + | app.js:947, session.rs:1358 |
| 8 | HiDPI scaling | ✅ | 🟡 | 🔴 | − | scale_factor 0 session.rs:2407 |
| 9 | Dynamic resize | ✅ | ✅ | 🔴 | − | no-op app.js:937 |
| 10 | Custom cursors | ✅ | ✅ | ✅ | = | session.rs:2152, canvas.rs:269 |
| 11 | Seamless published apps | ✅ | ✅ | 🟡 | ~ | crates/ironrdp-rail, app.js:1458 |
| 12 | App icons / taskbar | ✅ | ✅ | 🔴 | − | window.rs:208-213 |
| 13 | Multi-app per session | ✅ | ✅ | 🔴 | − | session.rs:804 |
| 14 | Keyboard + extended keys | ✅ | ✅ | ✅ | = | app.js:289-327 |
| 15 | IME / CJK / unicode | ✅ | ✅ | 🔴 | − | no composition (0 hits) |
| 16 | Touch / multitouch | ✅ | 🟡 | 🔴 | − | no touch handlers (0 hits) |
| 17 | Pen / Windows Ink | ✅ | 🔴 | 🔴 | = | (0 hits) |
| 18 | Mouse 5-btn + H/V wheel | ✅ | ✅ | ✅ | = | session.rs:1256, app.js:818 |
| 19 | Audio playback (compressed) | ✅ | ✅ | ✅ | = | audio.rs:24, worklet |
| 20 | Microphone input | ✅ | 🟡 | 🔴 | − | absent audio.rs:76 |
| 21 | Webcam redirection | ✅ | 🟡 | 🔴 | − | absent |
| 22 | Teams/Zoom UC offload | ✅ | 🟡 | ⚪ | − | needs native plugin |
| 23 | Clipboard text | ✅ | ✅ | ✅ | = | clipboard.rs:225,412 |
| 24 | Clipboard image | ✅ | 🟡 | ✅ | + | clipboard.rs:280 |
| 25 | Clipboard HTML format | ✅ | ✅ | 🔴 | − | text/image only |
| 26 | File transfer (clipboard) | ✅ | 🟡 | ✅ | + | clipboard.rs:451-538 |
| 27 | Drive/folder redirection | ✅ | 🟡 | 🔴 | − | absent |
| 28 | Printing | ✅ | ✅ | 🔴 | − | absent |
| 29 | Generic USB | ✅ | 🟡 | ⚪ | − | WebUSB can't |
| 30 | Smartcard / FIDO2-in-session | ✅ | 🟡 | 🔴 | − | ⚪-ish |
| 31 | Loss-resilient transport (EDT) | ✅ | 🔴(TCP) | 🔴(TCP) | = | WS/TCP main.rs:285 |
| 32 | Session reliability (hold+resume) | ✅ | ✅ | 🟡 | − | reconnect only app.js:277 |
| 33 | Auto-reconnect + flap/takeover | ✅ | ✅ | ✅ | = | app.js:1318-1349 |
| 34 | Server redirection (RDSTLS) | ✅ | ✅ | ✅ | = | redirect.rs, session.rs:2179 |
| 35 | Web-tier auth / gateway / MFA | ✅ | ✅ | 🔴 | − | none main.rs:255 |
| 36 | End-to-end cert validation | ✅ | ✅ | 🔴 | − | MITM by design session.rs:1881 |
| 37 | NLA depth (Kerberos) | ✅ | ✅ | 🔴 | − | NTLM-only session.rs:1772 |
| 38 | App Protection (anti-capture) | ✅ | 🟡 | 🔴 | − | none |
| 39 | Session watermark | ✅ | ✅ | 🔴 | − | none |
| 40 | Session recording | ✅ | ✅ | 🔴 | − | none |
| 41 | Control plane / broker / provisioning | ✅ | ✅ | ⚪ | − | non-goal |
| 42 | Zero host agent | 🔴 | 🔴 | ✅✅ | + | stock RDP |
| 43 | Zero client install | 🔴 | ✅ | ✅ | = | browser only |
| 44 | Single-binary deploy | 🔴 | 🔴 | ✅✅ | + | main.rs |
| 45 | Open source | 🔴 | 🔴 | ✅ | + | this repo |

**Reading the scorecard:** of the 45 rows, IronBridge is at parity-or-better vs. **CWA-HTML5** on ~20
(the entire core datapath + its own architectural wins), partial on ~4, and behind on ~21 — and those
21 cluster hard into three buckets: **peripheral redirection** (needs agent/native process), **access
security** (needs a gateway/auth layer — the one truly urgent bucket), and **advanced display/UC** (frame
pacing, HiDPI, mic/webcam/Teams). The roadmap targets the buckets that are both browser-feasible and
high-value.

---

## 7. Improvement roadmap ("how best we can improve")

Tiered by value×feasibility. **Per project rule (CLAUDE.md), every Tier 0/1/2 item carries a
compatibility line (Win11 headless GPU-less · Ubuntu GNOME Remote Desktop · Ubuntu xrdp), a performance
note, and a feature-interplay note.** Tier 3 items are named with the reason they're deferred/declined.

### Tier 0 — Fix what IronBridge already *claims* to do (correctness debt, do first)

These aren't Citrix-parity features; they're places the product is currently misleading or exposed.

**T0.1 — Web-tier authentication on `/ws`.** *The #1 fix.* Add an auth gate before the proxy upgrades:
minimum a shared bearer token / signed cookie; better, a small login → short-lived signed token → `/ws`
checks it (and optionally selects the target, folding in the mini-broker idea). Origin-check the WS
handshake too.
- *Compat:* host-agnostic — pure proxy/web-tier change; identical for Win11/GRD/xrdp.
- *Perf:* per-connection only, **zero datapath cost**.
- *Interplay:* must not break the RAIL `?app=` deep link or auto-reconnect (token must survive reconnect
  — stash with `savedCredentials`); pairs naturally with per-user target selection (T2.4).
- *Evidence of gap:* [main.rs:255-261](server/src/main.rs#L255).

**T0.2 — Make the FPS cap real (or delete the UI).** Either implement `requestAnimationFrame` dirty-
region coalescing driven by the cap (one composite paint per frame, honoring 15/30/60/120) or remove the
inert dropdown. Recommended: implement it — it's also the long-open `PERFORMANCE_ANALYSIS.md §3` item.
- *Compat:* host-agnostic (client render path).
- *Perf:* **the central perf trade** — rAF batching cuts overdraw/CPU on update-heavy scenes and enables
  bandwidth-saving frame caps, at the cost of up to one frame of added latency. Keep an "uncapped/
  immediate" mode to preserve today's low-latency behavior; make coalescing the default.
- *Interplay:* touches every codec paint path (legacy + EGFX blit + AVC420 decoded frames,
  [session.rs:2116,1382](wasm/src/session.rs#L2116)); must coexist with multi-monitor surfaces (one rAF
  loop, all surfaces) and not stall the audio worklet (separate clock).
- *Evidence:* `_fps_cap` [session.rs:1946](wasm/src/session.rs#L1946); no rAF (0 hits).

**T0.3 — End-to-end server cert pinning.** Enforce the WASM-side public-key pin that the architecture
*intends* (the proxy is a deliberate MITM, so the WASM must verify the real host key). Promote the
redirection path's warn-only pin check ([redirect.rs:114](wasm/src/redirect.rs#L114)) to enforced, and
surface a trust-on-first-use / configured-pin flow.
- *Compat:* host-agnostic; needs a way to convey the expected pin (config or TOFU).
- *Perf:* handshake-only, zero datapath cost.
- *Interplay:* interacts with server redirection (re-pin on redirect target) and CredSSP channel binding
  (already extracts SPKI, [session.rs:1881](wasm/src/session.rs#L1881)).

### Tier 1 — High value, browser-feasible, on-charter

**T1.1 — Microphone input (RDPEAI + getUserMedia).** Already roadmapped. `getUserMedia` → encode (Opus
via WebCodecs, or PCM) → AUDIO_INPUT DVC upstream. Reuses the DRDYNVC plumbing that playback already
uses ([session.rs:958](wasm/src/session.rs#L958)) and the vendored rdpsnd crate pattern.
- *Compat:* Win11 ✅ & GRD ✅ (accept RDPEAI); **xrdp weak/absent** (gate it, like other host-specific
  features).
- *Perf:* one encode + a low-bitrate upstream DVC; **must not perturb the playback jitter buffer** (it's
  a separate channel/clock — keep them isolated).
- *Interplay:* pairs with the audio HUD row; needs a mic permission UX; respects the existing audio
  enable flag.

**T1.2 — Dynamic resize on capable hosts (RDPEDISP).** Make `setupResizeHandler` conditional instead of a
blanket no-op: drive `encode_resize`/DisplayControl ([session.rs:1983,2313](wasm/src/session.rs#L1983))
on browser resize/fullscreen **when the host advertises the Display Control VC**, falling back to the
current fixed-canvas behavior when it doesn't.
- *Compat:* **Win11 ✅** (supports RDPEDISP); **xrdp 🔴** (the exact reason it's disabled today — keep the
  fallback); GRD 🟡 (verify).
- *Perf:* resize is a rare event → a re-negotiation + framebuffer realloc; negligible steady-state cost.
- *Interplay:* **directly interacts with multi-monitor** (both drive DisplayControl — unify the layout
  path, [app.js:1069](web/app.js#L1069)) and with HiDPI (T1.4); reconnect must re-apply the last size.

**T1.3 — IME / unicode input.** Add `compositionstart/update/end` + `beforeinput` handling and send
composed text via Unicode keyboard events; enable client keyboard-layout signaling
([session.rs:2385](wasm/src/session.rs#L2385)).
- *Compat:* host-agnostic (Win11/GRD/xrdp all accept Unicode input events).
- *Perf:* low-rate input — negligible; risk is correctness (dead keys, composition cancel), so leave a
  test.
- *Interplay:* must not fight the Ctrl+V deferred-paste replay ([app.js:736](web/app.js#L736)) or the
  scancode fast path (composition bypasses scancodes).

**T1.4 — HiDPI / display scaling.** Honor `devicePixelRatio`: negotiate `desktop_scale_factor` and/or
render the canvas at device pixels ([session.rs:2407](wasm/src/session.rs#L2407)).
- *Compat:* Win11 ✅ (DPI-aware); GRD/xrdp 🟡 (scale client-side if the host won't).
- *Perf:* larger framebuffer = more decode/paint/bandwidth — make it opt-in or DPI-capped; interacts with
  the T0.2 frame-pacing work (more pixels per frame).
- *Interplay:* multi-monitor (per-monitor DPI), dynamic resize (T1.2).

**T1.5 — RAIL polish → Enhanced RAIL.** Sequence: window **icons** + **taskbar** UI
([window.rs:208-213](crates/ironrdp-rail/src/window.rs#L208)) → **Enhanced/HIDEF RAIL** (per-window EGFX
via MapSurfaceToWindow, re-enabling EGFX in rail mode, [session.rs:847-853](wasm/src/session.rs#L847)) →
**multi-app** ([session.rs:804](wasm/src/session.rs#L804)).
- *Compat:* **Windows-only** (RAIL charter); GPU-less-safe (EGFX decodes client-side).
- *Perf:* Enhanced RAIL is *less* bandwidth (only window pixels) and kills overlap ghosting; icons/taskbar
  are low-rate metadata.
- *Interplay:* Enhanced RAIL flips the `enable_gfx = !rail_mode` decision — the biggest change; reuses the
  AVC420 per-surface decoder path.

**T1.6 — Kerberos NLA + correct SPN.** Add `ClientMode` Kerberos support and derive the SPN from the real
target hostname instead of `TERMSRV/localhost` ([session.rs:1766,1790](wasm/src/session.rs#L1766)).
- *Compat:* unblocks **hardened Win11/AD** (NTLM-disabled domains); no effect on xrdp/GRD (NTLM fine).
- *Perf:* handshake-only.
- *Interplay:* SPN must track server redirection targets; ties into T0.1 (per-user target → correct SPN).

**T1.7 — Touch input (RDPEI).** Add pointer/touch event handlers → RDP MS-RDPEI multitouch (or at minimum
touch→mouse emulation for tablets).
- *Compat:* Win11 ✅ (RDPEI); GRD/xrdp 🟡 (fall back to mouse emulation).
- *Perf:* low-rate; negligible.
- *Interplay:* new DVC; coexists with existing mouse path (choose per pointerType).

### Tier 2 — Valuable, more work / more design

**T2.1 — Drive redirection (RDPDR + File System Access API).** Map a user-picked local folder into the
session via RDPDR, backed by the browser File System Access API.
- *Compat:* Win11 ✅ & xrdp ✅ (RDPDR server side); GRD 🟡.
- *Perf:* bursty, user-initiated — **keep on a separate DVC, off the graphics path** (reuse the file-
  clipboard chunking discipline, [clipboard.rs:451](wasm/src/clipboard.rs#L451)).
- *Interplay:* complements file-clipboard; needs a clear permission/consent UX (File System Access
  prompts).

**T2.2 — Printing (RDPDR print channel → browser).** Redirect a session printer to a browser print/PDF
flow.
- *Compat:* Win11 ✅; xrdp 🟡; GRD 🟡.
- *Perf:* per-job, off hot path.
- *Interplay:* builds on the T2.1 RDPDR channel work.

**T2.3 — Client-side session watermark.** CSS/canvas overlay with username/timestamp/host over the
session (and RAIL windows). Not Citrix's tamper-resistant server-drawn version, but a real deterrent and
compliance checkbox.
- *Compat:* host-agnostic (pure client overlay).
- *Perf:* one static overlay element — negligible; must sit above RAIL windows (reuse the z-index 5000
  chrome layer from the RAIL work).
- *Interplay:* interacts with fullscreen and multi-monitor popups (render per surface).

**T2.4 — Per-user target selection / mini-broker.** Let the authenticated user (T0.1) choose among
allowed targets instead of a single fixed `--rdp-target` ([main.rs:35](server/src/main.rs#L35)); a static
per-user catalog on the proxy.
- *Compat:* host-agnostic.
- *Perf:* connection-time only.
- *Interplay:* extends T0.1 auth; pairs with the RAIL app catalog (`--app`) into one "what can this user
  launch, where" model.

**T2.5 — WebTransport (QUIC) transport experiment.** Offer a WebTransport datagram path as an alternative
to WebSocket/TCP for loss resilience, with TCP fallback (mirrors Citrix Adaptive Transport's shape).
- *Compat:* host-agnostic on the RDP side; needs a UDP/QUIC-capable proxy endpoint.
- *Perf:* **the one transport lever that helps lossy WAN** — but adds a fallback matrix and a second proxy
  path; measure before committing.
- *Interplay:* reconnect/redirection logic must handle two transports; keep TCP/WS as default.

**T2.6 — Session-reliability-lite.** On disconnect, freeze and keep displaying the last framebuffer with a
"reconnecting" affordance, and fast-resume; approximates CGP's UX without server-side hold.
- *Compat:* host-agnostic (client-side).
- *Perf:* holds one framebuffer in memory — trivial.
- *Interplay:* layers on the existing reconnect/flap/takeover logic ([app.js:1318](web/app.js#L1318)) —
  don't regress the takeover guard.

### Tier 3 — Explicit non-goals (named, with reasons)

- **Generic USB redirection** — WebUSB cannot claim HID/mass-storage/smartcard classes; the browser
  sandbox structurally blocks arbitrary device projection. ⚪
- **Teams/Zoom UC offload** — requires the vendor's native endpoint media engine (SlimCore is a DLL);
  no browser-only path exists. ⚪
- **Webcam at meeting quality** — a generic RDPECAM redirect is *possible* (getUserMedia→encode) but
  competes with a problem Citrix solves with a native process; low ROI vs. mic-in. Defer.
- **Smartcard / scanner / COM-LPT** — no browser API to project these device classes into a session. ⚪
- **Control plane / MCS-PVS / Director / Autoscale** — IronBridge is a bridge, not a platform; building a
  data center is off-charter. ⚪
- **AV1 / H.265 server encode** — GPU-only on the encode side; the charter targets are GPU-less and
  headless, so there's no server GPU to encode with. AVC420 client-decode already covers the video case. ⚪
- **App Protection (kernel anti-capture)** — needs an endpoint kernel driver; a browser tab cannot block
  OS-level screen capture. ⚪ (client watermark T2.3 is the feasible partial.)
- **Browser Content Redirection** — offloading remote browser content to the local browser is a deep
  per-site integration with poor ROI here. ⚪

### 7.1 Recommended execution order

1. **T0.1 web-tier auth** (unblocks real deployment; largest risk retired).
2. **T0.2 frame pacing / real FPS cap** (fixes a claimed-but-fake feature + the open perf item).
3. **T1.1 microphone** and **T1.3 IME** (highest user-visible datapath gaps; both browser-native).
4. **T1.2 dynamic resize** + **T1.4 HiDPI** (unify with multi-monitor's DisplayControl path).
5. **T1.5 RAIL polish → Enhanced RAIL** (deepens the category IronBridge just entered).
6. **T0.3 cert pinning**, **T1.6 Kerberos** (security hardening, handshake-only).
7. **T2.x** as demand dictates (drives/printing/watermark/mini-broker/WebTransport).

---

## 8. Sources

**Citrix (verified July 2026):**
- HDX graphics, codecs, Build-to-Lossless, AV1/H.265 GPU requirements — community.citrix.com/tech-zone/design/design-decisions/hdx-graphics/ ; citrix.com/blogs (advanced video codec support in HDX) ; docs.citrix.com/en-us/citrix-virtual-apps-desktops/graphics/thinwire
- Adaptive Transport / EDT / Rendezvous / HDX Direct — docs.citrix.com/en-us/citrix-virtual-apps-desktops/hdx-transport/adaptive-transport.html ; …/hdx-transport/hdx-direct.html ; docs.citrix.com/en-us/citrix-gateway-service/hdx-edt-support-for-gateway-service.html
- App Protection (anti-keylogging/anti-screen-capture, anti-DLL-injection, policy-tampering default-on 2511, Recall/AI blocking) — docs.citrix.com/en-us/citrix-workspace-app/app-protection/features.html ; …/app-protection/app-protection-blocking-unauthorized-ai.html
- Microsoft Teams optimization (SlimCore VDI plugin, GA Win/Mac, HdxRtcEngine deprecation) — docs.citrix.com/en-us/citrix-virtual-apps-desktops/multimedia/opt-ms-teams-new/ms-slimcore-optimization.html ; support.citrix.com/external/article/CTX691425 ; learn.microsoft.com/en-us/microsoftteams/vdi-2
- Audio (Adaptive Audio, bidirectional CTXCAM, mic redirection, EDT-lossy) — docs.citrix.com/en-us/citrix-virtual-apps-desktops/multimedia/audio.html
- Session watermark (Thinwire-only, not in recordings) — docs.citrix.com/en-us/citrix-virtual-apps-desktops/graphics/introduction/session-watermark-introduction.html
- Session Recording (lossy codec, playback justification) — docs.citrix.com/en-us/session-recording/current-release/configure/settings-on-session-recording-agent/enable-or-disable-lossy-screen-recording.html
- CWA for HTML5 (multi-monitor ≤2 Chrome/Edge, webcam Chrome/Edge, mic device-selection 2511, clipboard native/HTML format, printing) — docs.citrix.com/en-us/citrix-workspace-app-for-html5/ ; help-docs.citrix.com/en-us/citrix-workspace-app/html5/multi-monitor.html ; …/html5/clipboard ; …/html5/printing.html
- Peripherals (CDM, generic USB, composite device splitting, FIDO2/WebAuthn redirection) — docs.citrix.com/en-us/citrix-virtual-apps-desktops/devices.html ; …/devices/usb-devices/composite-devices-and-device-splitting.html ; …/secure/fido2.html
- Input (keyboard layout dynamic sync, Generic Client IME sync-mode-4, multitouch, pen/Windows Ink) — docs.citrix.com/en-us/citrix-workspace-app-for-windows/keyboard.html ; docs.citrix.com/en-us/citrix-virtual-apps-desktops/devices/mobile-devices.html
- Release cadence (CR 2503/2511/2603, LTSR 2507; no "2506") — docs.citrix.com/en-us/citrix-virtual-apps-desktops/whats-new.html ; …/2507-ltsr/whats-new.html

**MS-RDP* specifications (feasibility basis for the roadmap):**
- MS-RDPBCGR (base graphics/connection), MS-RDPERP (RAIL/RemoteApp incl. HIDEF), MS-RDPEGFX (EGFX
  graphics pipeline), MS-RDPEA (RDPSND audio out), **MS-RDPEAI** (audio input/mic — T1.1), **MS-RDPEDISP**
  (dynamic display/resize — T1.2), **MS-RDPEI** (multitouch — T1.7), **MS-RDPEFS/RDPDR** (drive & print
  redirection — T2.1/T2.2), MS-RDPECAM (webcam), MS-RDPEUSB (USB), MS-RDPEPC (port/print).

**Browser API basis:** WebCodecs (`VideoDecoder`/`AudioDecoder` — used; `AudioEncoder`/`VideoEncoder` for
T1.1/webcam), `getUserMedia` (mic/cam), Window Management API `getScreenDetails` (multi-monitor — used),
File System Access API (T2.1 drives), WebTransport (T2.5), `devicePixelRatio` (T1.4), composition/
`beforeinput` events (T1.3), Pointer/Touch events (T1.7).

**IronBridge:** this repository — `server/src/main.rs`, `wasm/src/{session,clipboard,audio,rail,redirect}.rs`,
`web/{app.js,index.html,audio-worklet.js}`, `crates/ironrdp-rail/`, `crates/ironrdp-rdpsnd/`; design docs
`CLAUDE.md`, `PERFORMANCE_ANALYSIS.md`, `AUDIO_COMPARISON.md`, `MULTIMONITOR_DESIGN.md`, `docs/*.md`.

---

## 9. Appendix — stale claims in existing docs superseded here

Both existing comparison docs predate current code and should be read with these corrections (this
report is authoritative where they conflict):

**`PERFORMANCE_ANALYSIS.md`:**
- Claims TCP_NODELAY / hoisted read buffer / zero-copy `.freeze()` as *open* — all **since fixed**
  ([main.rs:285](server/src/main.rs#L285) NODELAY + 64 KB buffer; framing uses `.freeze()`).
- The **rAF dirty-region batching** item it flags **remains genuinely open** (§4.1, §7 T0.2) — the FPS
  badge still counts dirty-region paints, not frames.

**`AUDIO_COMPARISON.md`:**
- Claims "PCM only", "Volume PDU ignored / no GainNode", and a push-based BufferSource scheduler — all
  **outdated**. Shipped code has **Opus/AAC** ([audio.rs:24](wasm/src/audio.rs#L24)), **GainNode volume
  sync** ([audio.rs:71](wasm/src/audio.rs#L71)), and a **pull-based AudioWorklet ring buffer**
  ([audio-worklet.js:37](web/audio-worklet.js#L37)). The audio stack it recommends building **has been
  built**.

**Net:** treat §4 + §6 of this document as the current-state source of truth; the older docs remain useful
for their reasoning/history but not for their status claims.

