# web-rdp

A web-based RDP client where the full protocol stack runs in the browser as WebAssembly. The server is a WebSocket-to-TCP relay and nothing more — it never parses an RDP PDU.

Built on Devolutions' [IronRDP](https://github.com/Devolutions/IronRDP) library.

## Why this instead of Guacamole

Guacamole-style proxies decode RDP on the server and push pixel streams (JPEG or PNG tiles) to the browser. It works, but the server spends meaningful CPU re-encoding frames and the browser receives screenshots rather than the actual RDP output.

Here the browser runs the complete IronRDP state machine — graphics decoder, NLA/CredSSP auth, scancode encoding — in WASM. The backend is a ~200-line relay that moves bytes between the WebSocket and a TCP socket. It does exactly one non-trivial thing: when NLA is required, it does the TLS handshake with the RDP host and sends the server certificate back so the WASM side can complete CredSSP.

## Prerequisites to Build

- Rust stable (any recent version)
- wasm-pack (`cargo install wasm-pack`)

## Build

Windows:
```powershell
.\scripts\build-windows.ps1
```

Linux:
```bash
./scripts/build-ubuntu.sh
```

The script builds the WASM module via wasm-pack, then compiles the server binary. Artefacts land in `target/release/`.

## Running

```
./target/release/server --port 8080 --rdp-target <host>:3389
```

Open `http://localhost:8080` in Chrome or Edge. `--rdp-target` is where the relay forwards connections — `localhost:3389` if the RDP host is on the same machine, or any reachable address otherwise.

## Features

- RemoteFX graphics decoded in the browser
- Multi-monitor support using the Window Management API
- Audio over RDPSND — PCM, Opus, and AAC
- Bidirectional clipboard (text and images)
- NLA authentication via NTLM/CredSSP

## How it works

The WASM module (`wasm/`) runs the IronRDP state machine. RDP PDUs come in over the WebSocket, get parsed, and the decoded framebuffer is painted to an HTML canvas with `putImageData`. Keyboard and mouse events go the other direction — captured in JS, translated to AT-101 scancodes, encoded as FastPath input PDUs in WASM, and sent back over the WebSocket.

The relay (`server/`) is an Axum server. The one non-trivial step is TLS: the WASM client sends `{"cmd":"tls_upgrade"}` over the WebSocket, the server does the TLS handshake with the RDP host, returns the server certificate in a JSON message, and from then on passes ciphertext in both directions unchanged. WASM does CredSSP (NTLM) over that tunnel.

For more detail on individual subsystems, see the `docs/` folder:

- [docs/credssp.md](docs/credssp.md) — TLS upgrade and CredSSP/NTLM sequence
- [docs/graphics.md](docs/graphics.md) — RemoteFX rendering pipeline
- [docs/audio.md](docs/audio.md) — RDPSND negotiation and AudioWorklet playback
- [docs/input.md](docs/input.md) — keyboard scancode mapping and mouse handling
- [docs/clipboard.md](docs/clipboard.md) — bidirectional CLIPRDR flow

## Project layout

| Path | Contents |
|---|---|
| `server/` | Axum WebSocket-to-TCP relay |
| `wasm/` | IronRDP state machine compiled to WASM |
| `web/` | HTML, JS, CSS frontend |
| `crates/ironrdp-rdpsnd/` | Vendored audio crate with Opus and AAC support |
| `scripts/` | Build scripts for Windows and Linux |

## Common build and runtime issues

**Windows Defender locking files during wasm-pack** — wasm-pack writes to `target/` and Defender sometimes holds files open mid-build, causing "access denied" errors. Adding the project folder to Defender's exclusion list resolves it.

**Blank screen after connect** — check that `--rdp-target` is reachable and port 3389 is open on the target.

**CredSSP auth failure** — usually a username format issue. Try `user@domain` or `DOMAIN\user` depending on whether the machine is domain-joined. The domain field on the login form is only needed for domain-joined targets; for local accounts leave it blank.
