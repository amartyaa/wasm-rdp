use std::cell::RefCell;
use std::mem;
use std::rc::Rc;

use anyhow::Context as _;
use futures_channel::mpsc;
use futures_util::{SinkExt, StreamExt, select};
use futures_util::FutureExt;
use gloo_net::websocket::futures::WebSocket;
use gloo_net::websocket;
use ironrdp::connector::{self, ClientConnector, ClientConnectorState, ConnectionResult, Credentials, Sequence as _, State as _};
use ironrdp::pdu::gcc;
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::pdu::input::fast_path::FastPathInputEvent;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageOutput};
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp_core::WriteBuf;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::canvas::Canvas;
use crate::framed::WasmFramed;
use crate::{log, log_error};

/// An active RDP session handle exposed to JavaScript.
#[wasm_bindgen]
pub struct Session {
    input_tx: mpsc::UnboundedSender<InputEvent>,
    input_db: ironrdp::input::Database,
    desktop_width: u16,
    desktop_height: u16,
    stats: Rc<RefCell<SessionStats>>,
    /// Render surfaces, one per monitor, shared with the session loop. JS adds
    /// popup-window surfaces here for multi-monitor; the rAF paint iterates them.
    surfaces: Rc<RefCell<Vec<Canvas>>>,
}

/// Bandwidth statistics shared between the session, framed reader, and writer.
#[derive(Default)]
pub(crate) struct SessionStats {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

pub(crate) enum InputEvent {
    FastPath(smallvec::SmallVec<[FastPathInputEvent; 2]>),
    Resize { width: u16, height: u16 },
    /// Apply a new multi-monitor layout to a live session via DisplayControl.
    /// Flat `[left, top, width, height, primary]` per monitor.
    MonitorLayout { monitors: Vec<i32> },
    Cliprdr(ironrdp::cliprdr::backend::ClipboardMessage),
    /// Advertise local files to the remote (Local→Remote file transfer).
    FileCopy(Vec<ironrdp::cliprdr::pdu::FileDescriptor>),
    Terminate,
}

#[wasm_bindgen]
impl Session {
    pub(crate) async fn connect(
        ws_url: String,
        username: String,
        password: String,
        domain: String,
        width: u16,
        height: u16,
        canvas_id: String,
        enable_opus: bool,
        enable_aac: bool,
        monitors: Vec<i32>,
        enable_text_clipboard: bool,
        enable_file_clipboard: bool,
        fps_cap: u32,
        enable_audio: bool,
        enable_font_smoothing: bool,
        disable_cursor_effects: bool,
        allow_wallpaper: bool,
        allow_themes: bool,
        allow_animations: bool,
    ) -> anyhow::Result<Session> {
        log(&format!("Connecting to proxy: {ws_url}"));

        // Multi-monitor: parse the flat layout from JS into GCC monitor rects.
        // Empty ⇒ single-monitor (unchanged legacy behavior). When present, the
        // requested desktop becomes the bounding box of all monitors.
        let monitor_layout = parse_monitor_layout(&monitors);
        let (width, height) = if monitor_layout.is_empty() {
            (width, height)
        } else {
            let (cw, ch) = combined_desktop_size(&monitor_layout);
            log(&format!(
                "Multi-monitor: {} monitors, combined desktop {cw}x{ch}",
                monitor_layout.len()
            ));
            (cw, ch)
        };

        // Rect of the surface backed by the main page canvas. Single-monitor:
        // the whole desktop. Multi-monitor: the primary monitor (JS adds the
        // secondary popup surfaces after connect via `add_surface`).
        let primary_rect = primary_surface_rect(&monitor_layout, width, height);

        // Open WebSocket to the proxy
        let ws = WebSocket::open(&ws_url).context("Failed to open WebSocket")?;

        // Wait for WebSocket to be ready
        loop {
            match ws.state() {
                websocket::State::Closing | websocket::State::Closed => {
                    anyhow::bail!("WebSocket connection failed");
                }
                websocket::State::Connecting => {
                    gloo_timers::future::sleep(std::time::Duration::from_millis(5)).await;
                }
                websocket::State::Open => break,
            }
        }
        log("WebSocket connected to proxy");

        // Build IronRDP connector config
        let config = build_connector_config(
            username.clone(), password.clone(), domain.clone(), width, height,
            monitor_layout, enable_audio, enable_font_smoothing, disable_cursor_effects,
            allow_wallpaper, allow_themes, allow_animations,
        );

        // Split WebSocket for bidirectional I/O
        let (ws_write, ws_read) = ws.split();

        // Create shared stats for bandwidth tracking
        let stats = Rc::new(RefCell::new(SessionStats::default()));

        // Create the framed reader for RDP PDU parsing
        let framed = WasmFramed::new(ws_read, stats.clone());

        // Create connector and perform RDP handshake
        let socket_addr = std::net::SocketAddr::from(([127, 0, 0, 1], 3389));
        
        // Create input channel (must be done before CliprdrBackend to pass tx)
        let (input_tx, input_rx) = mpsc::unbounded();

        let cliprdr = ironrdp::cliprdr::Cliprdr::new(Box::new(
            crate::clipboard::WasmCliprdrBackend::new(input_tx.clone(), enable_text_clipboard, enable_file_clipboard)
        ));

        // Only wire the RDPSND channel when the user enabled audio. With it
        // absent, the server sends no audio PDUs and the main loop is free from
        // audio-processing overhead — critical for smooth video playback.
        let connector = ClientConnector::new(config, socket_addr)
            .with_static_channel(cliprdr);
        let connector = if enable_audio {
            let rdpsnd = ironrdp::rdpsnd::client::Rdpsnd::new(
                Box::new(crate::audio::WasmRdpsndHandler::new(enable_opus, enable_aac))
            );
            connector.with_static_channel(rdpsnd)
        } else {
            connector
        };

        log("Starting RDP connection sequence...");

        let (connection_result, framed, ws_write) = perform_connection(
            connector, framed, ws_write,
            &username, &password, &domain,
        ).await?;

        let desktop_width = connection_result.desktop_size.width;
        let desktop_height = connection_result.desktop_size.height;

        log(&format!(
            "RDP connected! Desktop: {desktop_width}x{desktop_height}"
        ));

        // Set up the initial render surface (the main page canvas). Additional
        // monitor surfaces are added by JS via `add_surface` for multi-monitor.
        let (px, py, pw, ph) = primary_rect;
        let canvas = Canvas::new(&canvas_id, px, py, pw, ph)
            .context("Failed to initialize canvas")?;
        let surfaces: Rc<RefCell<Vec<Canvas>>> = Rc::new(RefCell::new(vec![canvas]));

        // Create the writer channel
        let (writer_tx, mut writer_rx) = mpsc::unbounded::<Vec<u8>>();

        // Spawn writer task
        spawn_local({
            let mut ws_write = ws_write;
            let stats = stats.clone();
            async move {
                while let Some(frame) = writer_rx.next().await {
                    use gloo_net::websocket::Message;
                    stats.borrow_mut().tx_bytes += frame.len() as u64;
                    if ws_write.send(Message::Bytes(frame)).await.is_err() {
                        break;
                    }
                }
            }
        });

        // Spawn the main RDP session loop
        spawn_local({
            let writer_tx = writer_tx.clone();
            let surfaces = surfaces.clone();
            async move {
                let reason = match run_session(
                    connection_result,
                    framed,
                    writer_tx,
                    input_rx,
                    surfaces,
                    desktop_width,
                    desktop_height,
                    fps_cap,
                ).await {
                    Ok(()) => {
                        log("RDP session ended");
                        "user_disconnect"
                    }
                    Err(e) => {
                        log_error(&format!("RDP session error: {e:#}"));
                        "connection_lost"
                    }
                };
                crate::notify_session_ended(reason);
            }
        });

        Ok(Session {
            input_tx,
            input_db: ironrdp::input::Database::new(),
            desktop_width,
            desktop_height,
            stats,
            surfaces,
        })
    }

    /// Send a keyboard scancode event
    #[wasm_bindgen]
    pub fn send_keyboard(&mut self, scancode: u8, is_pressed: bool, is_extended: bool) {
        let sc = ironrdp::input::Scancode::from_u8(is_extended, scancode);
        let op = if is_pressed {
            ironrdp::input::Operation::KeyPressed(sc)
        } else {
            ironrdp::input::Operation::KeyReleased(sc)
        };

        let events = self.input_db.apply(std::iter::once(op));
        let _ = self.input_tx.unbounded_send(InputEvent::FastPath(events));
    }

    /// Send a mouse move event
    #[wasm_bindgen]
    pub fn send_mouse_move(&mut self, x: u16, y: u16) {
        let op = ironrdp::input::Operation::MouseMove(ironrdp::input::MousePosition { x, y });
        let events = self.input_db.apply(std::iter::once(op));
        let _ = self.input_tx.unbounded_send(InputEvent::FastPath(events));
    }

    /// Send a mouse button event
    #[wasm_bindgen]
    pub fn send_mouse_button(&mut self, button: u8, is_pressed: bool, x: u16, y: u16) {
        let btn = match button {
            0 => ironrdp::input::MouseButton::Left,
            1 => ironrdp::input::MouseButton::Middle,
            2 => ironrdp::input::MouseButton::Right,
            3 => ironrdp::input::MouseButton::X1,
            4 => ironrdp::input::MouseButton::X2,
            _ => return,
        };
        let op = if is_pressed {
            ironrdp::input::Operation::MouseButtonPressed(btn)
        } else {
            ironrdp::input::Operation::MouseButtonReleased(btn)
        };
        let move_op = ironrdp::input::Operation::MouseMove(ironrdp::input::MousePosition { x, y });

        let events = self.input_db.apply([move_op, op].into_iter());
        let _ = self.input_tx.unbounded_send(InputEvent::FastPath(events));
    }

    /// Send a mouse wheel event
    #[wasm_bindgen]
    pub fn send_mouse_wheel(&mut self, horizontal: bool, delta: i16) {
        let rotations = ironrdp::input::WheelRotations {
            is_vertical: !horizontal,
            rotation_units: delta,
        };
        let op = ironrdp::input::Operation::WheelRotations(rotations);
        let events = self.input_db.apply(std::iter::once(op));
        let _ = self.input_tx.unbounded_send(InputEvent::FastPath(events));
    }

    /// Get the desktop width
    #[wasm_bindgen(getter)]
    pub fn width(&self) -> u16 {
        self.desktop_width
    }

    /// Get the desktop height
    #[wasm_bindgen(getter)]
    pub fn height(&self) -> u16 {
        self.desktop_height
    }

    /// Get total bytes received from the RDP server
    #[wasm_bindgen(getter)]
    pub fn rx_bytes(&self) -> f64 {
        self.stats.borrow().rx_bytes as f64
    }

    /// Get total bytes sent to the RDP server
    #[wasm_bindgen(getter)]
    pub fn tx_bytes(&self) -> f64 {
        self.stats.borrow().tx_bytes as f64
    }
    /// Request desktop resize (e.g. on fullscreen change)
    #[wasm_bindgen]
    pub fn resize(&mut self, width: u16, height: u16) {
        self.desktop_width = width;
        self.desktop_height = height;
        let _ = self.input_tx.unbounded_send(InputEvent::Resize { width, height });
    }

    /// Apply a new multi-monitor layout to the live session over DisplayControl
    /// (Windows hosts only). `monitors` is the same flat
    /// `[left, top, width, height, primary]` layout passed to `connect`. After
    /// calling this, JS should rebuild the render surfaces (`clear_surfaces` +
    /// `add_surface`) to match the new combined desktop.
    #[wasm_bindgen]
    pub fn apply_monitor_layout(&mut self, monitors: Vec<i32>) {
        if monitors.is_empty() {
            return;
        }
        let (cw, ch) = combined_desktop_size(&parse_monitor_layout(&monitors));
        self.desktop_width = cw;
        self.desktop_height = ch;
        let _ = self.input_tx.unbounded_send(InputEvent::MonitorLayout { monitors });
    }

    /// Terminate the session
    #[wasm_bindgen]
    pub fn shutdown(&self) {
        let _ = self.input_tx.unbounded_send(InputEvent::Terminate);
    }

    /// Remove all render surfaces. Used before re-declaring the full set on a
    /// multi-monitor (re)layout. The next painted frame is skipped until at
    /// least one surface is added again.
    #[wasm_bindgen]
    pub fn clear_surfaces(&self) {
        self.surfaces.borrow_mut().clear();
    }

    /// Add a render surface backed by `canvas`, covering the combined-desktop
    /// rectangle `[origin_x, origin_x+width) × [origin_y, origin_y+height)`.
    /// For multi-monitor, JS calls this once per monitor (main + popups).
    #[wasm_bindgen]
    pub fn add_surface(
        &self,
        canvas: web_sys::HtmlCanvasElement,
        origin_x: u16,
        origin_y: u16,
        width: u16,
        height: u16,
    ) {
        match crate::canvas::Canvas::from_element(canvas, origin_x, origin_y, width, height) {
            Ok(surface) => self.surfaces.borrow_mut().push(surface),
            Err(e) => log_error(&format!("add_surface failed: {e:#}")),
        }
    }
}

/// Perform the full RDP connection sequence using the ClientConnector state machine.
///
/// The connector drives through states:
/// ConnectionInitiationSendRequest → ConnectionInitiationWaitConfirm → ...
/// → SecurityUpgrade → CredSSP → ... → Connected
///
/// At the SecurityUpgrade state:
///   1. We send a `{"cmd":"tls_upgrade"}` text message to the proxy.
///   2. The proxy upgrades its TCP connection to TLS and returns the server
///      certificate as `{"cmd":"tls_ready","server_cert":"<hex>"}`.
///   3. We store the certificate for CredSSP channel binding.
///
/// At the CredSSP state:
///   1. We use `sspi::credssp::CredSspClient` with NTLM mode.
///   2. Exchange TSRequest PDUs with the RDP server through the proxy's TLS tunnel.
///   3. On success, mark CredSSP as done and continue with BasicSettingsExchange.
async fn perform_connection(
    mut connector: ClientConnector,
    mut framed: WasmFramed,
    ws_write: futures_util::stream::SplitSink<WebSocket, gloo_net::websocket::Message>,
    username: &str,
    password: &str,
    domain: &str,
) -> anyhow::Result<(ConnectionResult, WasmFramed, futures_util::stream::SplitSink<WebSocket, gloo_net::websocket::Message>)> {
    let mut ws_write = ws_write;
    let mut buf = WriteBuf::new();
    let mut server_public_key: Vec<u8> = Vec::new();

    loop {
        // Check if we've reached the Connected state
        if let ClientConnectorState::Connected { .. } = &connector.state {
            let state = mem::replace(&mut connector.state, ClientConnectorState::Consumed);
            if let ClientConnectorState::Connected { result } = state {
                return Ok((result, framed, ws_write));
            }
            unreachable!();
        }

        // Handle TLS security upgrade
        if connector.should_perform_security_upgrade() {
            log("Security upgrade — requesting TLS from proxy...");

            // Tell the proxy to upgrade its TCP connection to TLS
            use gloo_net::websocket::Message as WsMsg;
            let cmd = r#"{"cmd":"tls_upgrade"}"#;
            ws_write.send(WsMsg::Text(cmd.to_string()))
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send tls_upgrade: {e}"))?;

            // Wait for the proxy's tls_ready response (arrives as a text WS message)
            let cert_hex = framed.read_text_message().await
                .context("Failed to read tls_ready response")?;

            // Parse the JSON response and extract SubjectPublicKeyInfo
            if let Some(cert_str) = parse_tls_ready(&cert_hex) {
                let cert_der = hex_decode(&cert_str)?;
                log(&format!(
                    "TLS upgrade complete — cert DER: {} bytes",
                    cert_der.len()
                ));

                // Extract the SubjectPublicKeyInfo from the X.509 certificate.
                // CredSSP uses the SPKI (not the full cert) for channel binding.
                server_public_key = extract_public_key(&cert_der)?;
                log(&format!(
                    "Extracted SubjectPublicKeyInfo: {} bytes",
                    server_public_key.len()
                ));
            } else {
                log_error(&format!("Unexpected TLS response: {cert_hex}"));
                anyhow::bail!("Invalid tls_ready response from proxy");
            }

            connector.mark_security_upgrade_as_done();
            continue;
        }

        // Handle CredSSP authentication
        if connector.should_perform_credssp() {
            if server_public_key.is_empty() {
                log("CredSSP: no server cert available, skipping");
                connector.mark_credssp_as_done();
                continue;
            }

            log("CredSSP: starting NTLM authentication...");

            let hybrid_ex = format!("{:?}", connector.state).contains("HYBRID_EX");
            if hybrid_ex {
                log("CredSSP: HYBRID_EX negotiated");
            }

            match perform_credssp(
                &server_public_key,
                username,
                password,
                domain,
                &mut framed,
                &mut ws_write,
                hybrid_ex,
            ).await {
                Ok(()) => {
                    log("CredSSP: authentication successful!");
                }
                Err(e) => {
                    // The server required NLA (we're in should_perform_credssp),
                    // so a CredSSP failure is fatal — continuing only produces a
                    // confusing downstream "connection closed" error. Propagate the
                    // real cause (it carries STATUS_LOGON_FAILURE / 0xc000006d for
                    // bad credentials) so the UI can show "check username/password".
                    log_error(&format!("CredSSP failed: {e:#}"));
                    return Err(e.context("CredSSP authentication failed"));
                }
            }

            connector.mark_credssp_as_done();
            continue;
        }

        let state_name = connector.state.name();
        log(&format!("Connector state: {state_name}"));

        match connector.next_pdu_hint() {
            Some(hint) => {
                let pdu = framed.read_by_hint(hint).await
                    .context("Failed to read PDU from server")?;

                log(&format!("  Received {} bytes from server", pdu.len()));

                let written = connector.step(&pdu, &mut buf)
                    .context("Connector step failed")?;

                if !written.is_nothing() {
                    let data = buf.filled().to_vec();
                    if !data.is_empty() {
                        log(&format!("  Sending {} bytes to server", data.len()));
                        use gloo_net::websocket::Message;
                        ws_write.send(Message::Bytes(data)).await
                            .map_err(|e| anyhow::anyhow!("WebSocket send: {e}"))?;
                    }
                    buf.clear();
                }
            }
            None => {
                let written = connector.step_no_input(&mut buf)
                    .context("Connector step_no_input failed")?;

                if !written.is_nothing() {
                    let data = buf.filled().to_vec();
                    if !data.is_empty() {
                        log(&format!("  Sending {} bytes to server", data.len()));
                        use gloo_net::websocket::Message;
                        ws_write.send(Message::Bytes(data)).await
                            .map_err(|e| anyhow::anyhow!("WebSocket send: {e}"))?;
                    }
                    buf.clear();
                }
            }
        }
    }
}

async fn perform_credssp(
    server_public_key: &[u8],
    username: &str,
    password: &str,
    domain: &str,
    framed: &mut WasmFramed,
    ws_write: &mut futures_util::stream::SplitSink<WebSocket, gloo_net::websocket::Message>,
    hybrid_ex: bool,
) -> anyhow::Result<()> {
    use sspi::credssp::{CredSspClient, CredSspMode, ClientState, ClientMode};
    use sspi::credssp::TsRequest;
    use sspi::ntlm::NtlmConfig;
    use sspi::generator::GeneratorState;

    // Build NTLM credentials using sspi types
    let sspi_username = sspi::Username::new(
        username,
        if domain.is_empty() { None } else { Some(domain) },
    ).map_err(|e| anyhow::anyhow!("Invalid username: {e}"))?;

    let credentials = sspi::Credentials::AuthIdentity(sspi::AuthIdentity {
        username: sspi_username,
        password: password.to_string().into(),
    });

    let spn = format!("TERMSRV/{}", "localhost");

    let mut credssp_client = CredSspClient::new(
        server_public_key.to_vec(),
        credentials,
        CredSspMode::WithCredentials,
        ClientMode::Ntlm(NtlmConfig::default()),
        spn,
    ).map_err(|e| anyhow::anyhow!("CredSSP init failed: {e}"))?;

    // First round: start with an empty TsRequest
    let mut ts_request = TsRequest::default();
    let mut round = 0;

    loop {
        round += 1;
        log(&format!("CredSSP round {round}"));

        // Process current ts_request through the CredSSP state machine
        let mut generator = credssp_client.process(ts_request);
        let client_state = match generator.start() {
            GeneratorState::Completed(result) => {
                result.map_err(|e| anyhow::anyhow!("CredSSP process error: {e}"))?
            }
            GeneratorState::Suspended(_network_request) => {
                // NTLM doesn't need network requests (only Kerberos does).
                // If we get here, it's unexpected for our NTLM-only flow.
                anyhow::bail!("CredSSP: unexpected network request (Kerberos not supported)");
            }
        };

        match client_state {
            ClientState::ReplyNeeded(reply_ts_request) => {
                // Encode and send the TSRequest to the RDP server
                let mut encoded = Vec::new();
                reply_ts_request.encode_ts_request(&mut encoded)
                    .map_err(|e| anyhow::anyhow!("TSRequest encode failed: {e}"))?;

                log(&format!("CredSSP: sending {} bytes", encoded.len()));
                use gloo_net::websocket::Message;
                ws_write.send(Message::Bytes(encoded)).await
                    .map_err(|e| anyhow::anyhow!("WebSocket send: {e}"))?;

                // Read the server's response TSRequest
                let response_data = framed.read_credssp_response().await
                    .context("Failed to read CredSSP response")?;

                log(&format!("CredSSP: received {} bytes", response_data.len()));

                ts_request = TsRequest::from_buffer(&response_data)
                    .map_err(|e| anyhow::anyhow!("TSRequest decode failed: {e}"))?;
            }
            ClientState::FinalMessage(final_ts_request) => {
                // Encode and send the final TSRequest (with auth_info)
                let mut encoded = Vec::new();
                final_ts_request.encode_ts_request(&mut encoded)
                    .map_err(|e| anyhow::anyhow!("Final TSRequest encode failed: {e}"))?;

                log(&format!("CredSSP: sending final message ({} bytes)", encoded.len()));
                use gloo_net::websocket::Message;
                ws_write.send(Message::Bytes(encoded)).await
                    .map_err(|e| anyhow::anyhow!("WebSocket send: {e}"))?;

                if hybrid_ex {
                    log("CredSSP: waiting for EarlyUserAuthResult (HYBRID_EX)");
                    let auth_result = framed.read_exact(4).await?;
                    log(&format!("CredSSP EarlyUserAuthResult: {:02X?}", auth_result));
                    if auth_result != [0, 0, 0, 0] {
                        anyhow::bail!("CredSSP EarlyUserAuthResult denied/invalid: {:?}", auth_result);
                    }
                }

                log("CredSSP: handshake complete");
                return Ok(());
            }
        }

        if round > 10 {
            anyhow::bail!("CredSSP: too many rounds, aborting");
        }
    }
}

/// Parse `{"cmd":"tls_ready","server_cert":"<hex>"}` and return the hex string.
fn parse_tls_ready(json_str: &str) -> Option<String> {
    // Simple JSON parsing without serde (avoid adding serde to WASM)
    if !json_str.contains("tls_ready") {
        return None;
    }
    // Find "server_cert":"..."
    let marker = "\"server_cert\":\"";
    let start = json_str.find(marker)? + marker.len();
    let end = json_str[start..].find('"')? + start;
    Some(json_str[start..end].to_string())
}

/// Decode a hex string to bytes.
fn hex_decode(hex: &str) -> anyhow::Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        anyhow::bail!("Odd-length hex string");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| anyhow::anyhow!("Invalid hex: {e}"))
        })
        .collect()
}

/// Extract the raw public key bytes from an X.509 certificate.
///
/// CredSSP channel binding requires the raw key (content of the BIT STRING
/// inside SubjectPublicKeyInfo), matching FreeRDP's `i2d_PublicKey()`.
/// For RSA this is `SEQUENCE { modulus INTEGER, exponent INTEGER }`.
fn extract_public_key(cert_der: &[u8]) -> anyhow::Result<Vec<u8>> {
    use x509_cert::Certificate;
    use x509_cert::der::Decode;

    let cert = Certificate::from_der(cert_der)
        .map_err(|e| anyhow::anyhow!("Failed to parse X.509 certificate: {e}"))?;

    let spki = &cert.tbs_certificate.subject_public_key_info;
    let raw_key = spki.subject_public_key
        .as_bytes()
        .ok_or_else(|| anyhow::anyhow!("SubjectPublicKey has unused bits"))?;

    Ok(raw_key.to_vec())
}

/// Union two inclusive rectangles into their bounding box.
fn union_rect(
    a: ironrdp::pdu::geometry::InclusiveRectangle,
    b: ironrdp::pdu::geometry::InclusiveRectangle,
) -> ironrdp::pdu::geometry::InclusiveRectangle {
    ironrdp::pdu::geometry::InclusiveRectangle {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

thread_local! {
    // Monotonic paint timestamp (rAF DOMHighResTimeStamp) used for fps cap.
    // Thread-local avoids capturing an Rc into the rAF closure and the Rc
    // cycle that would prevent the framebuffer from being freed on session end.
    static LAST_PAINT_MS: RefCell<f64> = RefCell::new(0.0);
}

async fn run_session(
    connection_result: ConnectionResult,
    mut framed: WasmFramed,
    writer_tx: mpsc::UnboundedSender<Vec<u8>>,
    mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
    surfaces: Rc<RefCell<Vec<Canvas>>>,
    width: u16,
    height: u16,
    fps_cap: u32,
) -> anyhow::Result<()> {
    // Minimum ms between canvas paints (0 = uncapped / display-rate).
    // Reset the timer so a fresh session never incorrectly skips the first frame.
    let frame_min_ms: f64 = if fps_cap > 0 && fps_cap < 300 {
        1000.0 / fps_cap as f64
    } else {
        0.0
    };
    LAST_PAINT_MS.with(|t| *t.borrow_mut() = 0.0);

    let image = Rc::new(RefCell::new(DecodedImage::new(PixelFormat::RgbA32, width, height)));
    let mut active_stage = ActiveStage::new(connection_result);

    // Dirty-region coalescing. Instead of painting synchronously on every
    // GraphicsUpdate (which causes many putImageData boundary crossings and
    // overdraw the monitor never shows), we accumulate the dirty region and
    // flush exactly once per animation frame, aligned to the display refresh.
    let pending: Rc<RefCell<Option<ironrdp::pdu::geometry::InclusiveRectangle>>> =
        Rc::new(RefCell::new(None));
    let raf_scheduled = Rc::new(RefCell::new(false));
    let raf_closure: Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>> = Rc::new(RefCell::new(None));

    // Build the rAF callback once. It presents the accumulated `pending` region
    // at most once per display frame, subject to one bound:
    //   * FPS cap — never paint more often than `frame_min_ms` (0 = uncapped).
    // When the cap blocks this tick it re-arms itself (via a Weak to avoid an Rc
    // cycle) so the held region still flushes even if no further GraphicsUpdate
    // arrives. The loop self-terminates once `pending` is taken (nothing left → no
    // re-arm); new updates restart it via schedule_repaint.
    {
        let pending = pending.clone();
        let raf_scheduled = raf_scheduled.clone();
        let image = image.clone();
        let surfaces = surfaces.clone();
        let raf_weak = Rc::downgrade(&raf_closure);
        // frame_min_ms is f64 (Copy) — captured by value, no Rc needed.
        let cb = Closure::wrap(Box::new(move |now: f64| {
            *raf_scheduled.borrow_mut() = false;

            // Nothing to paint → stop the rAF loop.
            if pending.borrow().is_none() {
                return;
            }

            // FPS cap: if we painted too recently, re-arm and wait. The pending
            // region stays accumulated until we're allowed to paint.
            if frame_min_ms > 0.0 {
                let elapsed = LAST_PAINT_MS.with(|t| now - *t.borrow());
                if elapsed < frame_min_ms {
                    if let (Some(window), Some(rc)) = (web_sys::window(), raf_weak.upgrade()) {
                        if !*raf_scheduled.borrow() {
                            if let Some(cb) = rc.borrow().as_ref() {
                                *raf_scheduled.borrow_mut() = true;
                                let _ = window.request_animation_frame(cb.as_ref().unchecked_ref());
                            }
                        }
                    }
                    return;
                }
            }

            // Present the coalesced dirty region.
            if let Some(region) = pending.borrow_mut().take() {
                LAST_PAINT_MS.with(|t| *t.borrow_mut() = now);
                let img = image.borrow();
                let mut painted = false;
                for surface in surfaces.borrow_mut().iter_mut() {
                    if surface.draw(&img, region.clone()).is_ok() {
                        painted = true;
                    }
                }
                if painted {
                    crate::notify_frame();
                }
            }
        }) as Box<dyn FnMut(f64)>);
        *raf_closure.borrow_mut() = Some(cb);
    }

    let schedule_repaint = {
        let raf_scheduled = raf_scheduled.clone();
        let raf_closure = raf_closure.clone();
        move || {
            if *raf_scheduled.borrow() {
                return;
            }
            if let Some(window) = web_sys::window() {
                if let Some(cb) = raf_closure.borrow().as_ref() {
                    *raf_scheduled.borrow_mut() = true;
                    let _ = window.request_animation_frame(cb.as_ref().unchecked_ref());
                }
            }
        }
    };

    loop {
        let outputs = select! {
            frame = framed.read_pdu().fuse() => {
                let (action, payload) = frame.context("read RDP PDU")?;
                match active_stage.process(&mut image.borrow_mut(), action, payload.as_ref()) {
                    Ok(outputs) => outputs,
                    Err(e) => {
                        log_error(&format!("Ignoring PDU processing error: {e:#}"));
                        Vec::new()
                    }
                }
            }
            event = input_rx.next().fuse() => {
                match event {
                    Some(InputEvent::FastPath(events)) => {
                        active_stage.process_fastpath_input(&mut image.borrow_mut(), &events)
                            .context("process input")?
                    }
                    Some(InputEvent::Resize { width: new_w, height: new_h }) => {
                        log(&format!("Resize requested: {new_w}x{new_h}"));
                        match active_stage.encode_resize(u32::from(new_w), u32::from(new_h), None, None) {
                            Some(Ok(resize_frame)) => {
                                vec![ActiveStageOutput::ResponseFrame(resize_frame)]
                            }
                            Some(Err(e)) => {
                                log_error(&format!("Resize failed: {e}"));
                                Vec::new()
                            }
                            None => {
                                log("Resize: displaycontrol not available");
                                Vec::new()
                            }
                        }
                    }
                    Some(InputEvent::MonitorLayout { monitors }) => {
                        let entries = parse_dc_monitor_layout(&monitors);
                        if entries.is_empty() {
                            log_error("MonitorLayout: no valid monitors after validation");
                            Vec::new()
                        } else {
                            // Resize our framebuffer to the new combined desktop
                            // before the server starts sending updates for it.
                            let (nw, nh) = combined_desktop_size(&parse_monitor_layout(&monitors));
                            log(&format!("MonitorLayout: {} monitors, combined {nw}x{nh}", entries.len()));
                            *image.borrow_mut() = DecodedImage::new(PixelFormat::RgbA32, nw, nh);
                            match active_stage.encode_monitor_layout(&entries) {
                                Some(Ok(frame)) => vec![ActiveStageOutput::ResponseFrame(frame)],
                                Some(Err(e)) => {
                                    log_error(&format!("MonitorLayout encode failed: {e}"));
                                    Vec::new()
                                }
                                None => {
                                    log("MonitorLayout: displaycontrol not available");
                                    Vec::new()
                                }
                            }
                        }
                    }
                    Some(InputEvent::Cliprdr(message)) => {
                        if let Some(cliprdr) = active_stage.get_svc_processor_mut::<ironrdp::cliprdr::CliprdrClient>() {
                            if let Some(svc_messages) = match message {
                                ironrdp::cliprdr::backend::ClipboardMessage::SendInitiateCopy(formats) => {
                                    match cliprdr.initiate_copy(&formats) {
                                        Ok(msgs) => Some(msgs),
                                        Err(e) => { log_error(&format!("Cliprdr copy error: {e}")); None }
                                    }
                                }
                                ironrdp::cliprdr::backend::ClipboardMessage::SendFormatData(response) => {
                                    match cliprdr.submit_format_data(response) {
                                        Ok(msgs) => Some(msgs),
                                        Err(e) => { log_error(&format!("Cliprdr format data error: {e}")); None }
                                    }
                                }
                                ironrdp::cliprdr::backend::ClipboardMessage::SendInitiatePaste(format) => {
                                    match cliprdr.initiate_paste(format) {
                                        Ok(msgs) => Some(msgs),
                                        Err(e) => { log_error(&format!("Cliprdr paste error: {e}")); None }
                                    }
                                }
                                ironrdp::cliprdr::backend::ClipboardMessage::SendFileContentsRequest(req) => {
                                    match cliprdr.request_file_contents(req) {
                                        Ok(msgs) => Some(msgs),
                                        Err(e) => { log_error(&format!("Cliprdr file contents request error: {e}")); None }
                                    }
                                }
                                ironrdp::cliprdr::backend::ClipboardMessage::SendFileContentsResponse(resp) => {
                                    match cliprdr.submit_file_contents(resp) {
                                        Ok(msgs) => Some(msgs),
                                        Err(e) => { log_error(&format!("Cliprdr file contents response error: {e}")); None }
                                    }
                                }
                                ironrdp::cliprdr::backend::ClipboardMessage::Error(e) => {
                                    log_error(&format!("Clipboard backend error: {e}"));
                                    None
                                }
                            } {
                                let frame = active_stage.process_svc_processor_messages(svc_messages)
                                    .context("encode cliprdr SVC messages")?;
                                vec![ActiveStageOutput::ResponseFrame(frame)]
                            } else {
                                Vec::new()
                            }
                        } else {
                            log_error("Clipboard event received but Cliprdr is not available");
                            Vec::new()
                        }
                    }
                    Some(InputEvent::FileCopy(files)) => {
                        if let Some(cliprdr) = active_stage.get_svc_processor_mut::<ironrdp::cliprdr::CliprdrClient>() {
                            match cliprdr.initiate_file_copy(files) {
                                Ok(msgs) => {
                                    let frame = active_stage.process_svc_processor_messages(msgs)
                                        .context("encode cliprdr file copy messages")?;
                                    vec![ActiveStageOutput::ResponseFrame(frame)]
                                }
                                Err(e) => {
                                    log_error(&format!("Cliprdr initiate_file_copy error: {e}"));
                                    Vec::new()
                                }
                            }
                        } else {
                            log_error("FileCopy event received but Cliprdr is not available");
                            Vec::new()
                        }
                    }
                    Some(InputEvent::Terminate) => {
                        active_stage.graceful_shutdown()
                            .context("graceful shutdown")?
                    }
                    None => break,
                }
            }
        };

        for out in outputs {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => {
                    writer_tx.unbounded_send(frame)
                        .context("send response frame")?;
                }
                ActiveStageOutput::GraphicsUpdate(region) => {
                    // Coalesce into the pending dirty region; the rAF callback
                    // paints it once per display frame (subject to the FPS cap).
                    let mut p = pending.borrow_mut();
                    *p = Some(match p.take() {
                        Some(prev) => union_rect(prev, region),
                        None => region,
                    });
                    drop(p);
                    schedule_repaint();
                }
                ActiveStageOutput::PointerDefault => {
                    for surface in surfaces.borrow().iter() {
                        surface.set_cursor("default");
                    }
                }
                ActiveStageOutput::PointerHidden => {
                    for surface in surfaces.borrow().iter() {
                        surface.set_cursor("none");
                    }
                }
                ActiveStageOutput::PointerBitmap(pointer) => {
                    for surface in surfaces.borrow_mut().iter_mut() {
                        surface.set_custom_cursor(
                            &pointer.bitmap_data,
                            pointer.width as u32,
                            pointer.height as u32,
                            pointer.hotspot_x as u32,
                            pointer.hotspot_y as u32,
                        );
                    }
                }
                ActiveStageOutput::Terminate(_reason) => {
                    log("RDP session terminated by server");
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    Ok(())
}

/// Parse the flat monitor array from JS (`[left, top, width, height, primary]`
/// per monitor, in combined-desktop pixels, primary at the origin) into GCC
/// monitor rects. RDP monitor edges are **inclusive**, so a 1920-wide monitor at
/// x=0 spans `left=0..=right=1919`. An empty input yields an empty layout
/// (single-monitor / legacy path).
fn parse_monitor_layout(flat: &[i32]) -> Vec<gcc::Monitor> {
    flat.chunks_exact(5)
        .map(|c| {
            let (left, top, w, h, primary) = (c[0], c[1], c[2], c[3], c[4]);
            gcc::Monitor {
                left,
                top,
                right: left + w - 1,
                bottom: top + h - 1,
                flags: if primary != 0 {
                    gcc::MonitorFlags::PRIMARY
                } else {
                    gcc::MonitorFlags::empty()
                },
            }
        })
        .collect()
}

/// Combined desktop size = bounding box of all monitors. Edges are inclusive, so
/// the size is `max(right) + 1` by `max(bottom) + 1`. Assumes a normalized,
/// non-negative layout (origin at 0,0) as produced by the JS side.
fn combined_desktop_size(monitors: &[gcc::Monitor]) -> (u16, u16) {
    let right = monitors.iter().map(|m| m.right).max().unwrap_or(0);
    let bottom = monitors.iter().map(|m| m.bottom).max().unwrap_or(0);
    let w = (right + 1).clamp(1, u16::MAX as i32) as u16;
    let h = (bottom + 1).clamp(1, u16::MAX as i32) as u16;
    (w, h)
}

/// Parse the flat monitor array into DisplayControl `MonitorLayoutEntry` values
/// for a live (re)layout. Widths are adjusted to the protocol's constraints
/// (even, 200..=8192); the primary must be at (0,0) (the JS side normalizes it
/// so). Invalid monitors are skipped.
fn parse_dc_monitor_layout(flat: &[i32]) -> Vec<ironrdp::displaycontrol::pdu::MonitorLayoutEntry> {
    use ironrdp::displaycontrol::pdu::MonitorLayoutEntry;
    flat.chunks_exact(5)
        .filter_map(|c| {
            let (left, top, w, h, primary) = (c[0], c[1], c[2], c[3], c[4]);
            let (w, h) = MonitorLayoutEntry::adjust_display_size(w.max(0) as u32, h.max(0) as u32);
            let entry = if primary != 0 {
                MonitorLayoutEntry::new_primary(w, h)
            } else {
                MonitorLayoutEntry::new_secondary(w, h)
            }
            .ok()?;
            entry.with_position(left, top).ok()
        })
        .collect()
}

/// Rect `(origin_x, origin_y, width, height)` of the surface backed by the main
/// page canvas. Single-monitor (empty layout): the whole desktop. Multi-monitor:
/// the primary monitor (the one flagged PRIMARY, else the first).
fn primary_surface_rect(monitors: &[gcc::Monitor], combined_w: u16, combined_h: u16) -> (u16, u16, u16, u16) {
    let Some(m) = monitors
        .iter()
        .find(|m| m.flags.contains(gcc::MonitorFlags::PRIMARY))
        .or_else(|| monitors.first())
    else {
        return (0, 0, combined_w, combined_h);
    };
    let ox = m.left.max(0).min(i32::from(u16::MAX)) as u16;
    let oy = m.top.max(0).min(i32::from(u16::MAX)) as u16;
    let w = (m.right - m.left + 1).clamp(1, i32::from(u16::MAX)) as u16;
    let h = (m.bottom - m.top + 1).clamp(1, i32::from(u16::MAX)) as u16;
    (ox, oy, w, h)
}

fn build_connector_config(
    username: String,
    password: String,
    domain: String,
    width: u16,
    height: u16,
    monitors: Vec<gcc::Monitor>,
    enable_audio: bool,
    enable_font_smoothing: bool,
    disable_cursor_effects: bool,
    allow_wallpaper: bool,
    allow_themes: bool,
    allow_animations: bool,
) -> connector::Config {
    let domain = if domain.is_empty() { None } else { Some(domain) };

    // Full-window-drag always off (minor bandwidth, no UX value in a web client).
    // Wallpaper/themes/animations are user-controlled; each adds significant bandwidth.
    // ENABLE_FONT_SMOOTHING: opt-in ClearType.
    // DISABLE_CURSORSETTINGS: removes cursor shadow/blink, minor bandwidth win.
    let mut perf = PerformanceFlags::default()
        | PerformanceFlags::DISABLE_FULLWINDOWDRAG;
    if !allow_wallpaper  { perf |= PerformanceFlags::DISABLE_WALLPAPER; }
    if !allow_themes     { perf |= PerformanceFlags::DISABLE_THEMING; }
    if !allow_animations { perf |= PerformanceFlags::DISABLE_MENUANIMATIONS; }
    if enable_font_smoothing  { perf |= PerformanceFlags::ENABLE_FONT_SMOOTHING; }
    if disable_cursor_effects { perf |= PerformanceFlags::DISABLE_CURSORSETTINGS; }

    connector::Config {
        credentials: Credentials::UsernamePassword { username, password },
        domain,
        enable_tls: true,
        enable_credssp: true,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: connector::DesktopSize { width, height },
        bitmap: None,
        client_build: 0,
        client_name: "web-rdp-rust".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: MajorPlatformType::UNSPECIFIED,
        enable_server_pointer: true,
        request_data: None,
        autologon: true,
        enable_audio_playback: enable_audio,
        pointer_software_rendering: true,
        performance_flags: perf,
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        compression_type: None,
        multitransport_flags: None,
        monitors,
        monitors_extended: Vec::new(),
    }
}

impl Session {
    /// Expose a way for the clipboard module to send cliprdr messages to the event loop.
    pub(crate) fn send_cliprdr_message(&self, message: ironrdp::cliprdr::backend::ClipboardMessage) {
        let _ = self.input_tx.unbounded_send(InputEvent::Cliprdr(message));
    }

    /// Advertise local files to the remote via CLIPRDR (Local→Remote file transfer).
    pub(crate) fn send_file_copy(&self, files: Vec<ironrdp::cliprdr::pdu::FileDescriptor>) {
        let _ = self.input_tx.unbounded_send(InputEvent::FileCopy(files));
    }
}

