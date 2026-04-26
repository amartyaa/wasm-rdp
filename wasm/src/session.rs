use std::mem;

use anyhow::Context as _;
use futures_channel::mpsc;
use futures_util::{SinkExt, StreamExt, select};
use futures_util::FutureExt;
use gloo_net::websocket::futures::WebSocket;
use gloo_net::websocket;
use ironrdp::connector::{self, ClientConnector, ClientConnectorState, ConnectionResult, Credentials, Sequence as _, State as _};
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
    desktop_width: u16,
    desktop_height: u16,
}

pub(crate) enum InputEvent {
    FastPath(smallvec::SmallVec<[FastPathInputEvent; 2]>),
    Resize { width: u16, height: u16 },
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
    ) -> anyhow::Result<Session> {
        log(&format!("Connecting to proxy: {ws_url}"));

        // Open WebSocket to the proxy
        let ws = WebSocket::open(&ws_url).context("Failed to open WebSocket")?;

        // Wait for WebSocket to be ready
        loop {
            match ws.state() {
                websocket::State::Closing | websocket::State::Closed => {
                    anyhow::bail!("WebSocket connection failed");
                }
                websocket::State::Connecting => {
                    gloo_timers::future::sleep(std::time::Duration::from_millis(50)).await;
                }
                websocket::State::Open => break,
            }
        }
        log("WebSocket connected to proxy");

        // Build IronRDP connector config
        let config = build_connector_config(
            username.clone(), password.clone(), domain.clone(), width, height,
        );

        // Split WebSocket for bidirectional I/O
        let (ws_write, ws_read) = ws.split();

        // Create the framed reader for RDP PDU parsing
        let framed = WasmFramed::new(ws_read);

        // Create connector and perform RDP handshake
        let socket_addr = std::net::SocketAddr::from(([127, 0, 0, 1], 3389));
        
        let cliprdr = ironrdp::cliprdr::Cliprdr::new(Box::new(crate::clipboard::WasmCliprdrBackend::new()));

        let connector = ClientConnector::new(config, socket_addr)
            .with_static_channel(cliprdr);

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

        // Set up canvas for rendering
        let canvas = Canvas::new(&canvas_id, desktop_width, desktop_height)
            .context("Failed to initialize canvas")?;

        // Create input channel
        let (input_tx, input_rx) = mpsc::unbounded();

        // Create the writer channel
        let (writer_tx, mut writer_rx) = mpsc::unbounded::<Vec<u8>>();

        // Spawn writer task
        spawn_local({
            let mut ws_write = ws_write;
            async move {
                while let Some(frame) = writer_rx.next().await {
                    use gloo_net::websocket::Message;
                    if ws_write.send(Message::Bytes(frame)).await.is_err() {
                        break;
                    }
                }
            }
        });

        // Spawn the main RDP session loop
        spawn_local({
            let writer_tx = writer_tx.clone();
            async move {
                if let Err(e) = run_session(
                    connection_result,
                    framed,
                    writer_tx,
                    input_rx,
                    canvas,
                    desktop_width,
                    desktop_height,
                ).await {
                    log_error(&format!("RDP session error: {e:#}"));
                }
                log("RDP session ended");
            }
        });

        Ok(Session {
            input_tx,
            desktop_width,
            desktop_height,
        })
    }

    /// Send a keyboard scancode event
    #[wasm_bindgen]
    pub fn send_keyboard(&self, scancode: u8, is_pressed: bool, is_extended: bool) {
        let sc = ironrdp::input::Scancode::from_u8(is_extended, scancode);
        let op = if is_pressed {
            ironrdp::input::Operation::KeyPressed(sc)
        } else {
            ironrdp::input::Operation::KeyReleased(sc)
        };

        let mut db = ironrdp::input::Database::new();
        let events = db.apply(std::iter::once(op));
        let _ = self.input_tx.unbounded_send(InputEvent::FastPath(events));
    }

    /// Send a mouse move event
    #[wasm_bindgen]
    pub fn send_mouse_move(&self, x: u16, y: u16) {
        let op = ironrdp::input::Operation::MouseMove(ironrdp::input::MousePosition { x, y });
        let mut db = ironrdp::input::Database::new();
        let events = db.apply(std::iter::once(op));
        let _ = self.input_tx.unbounded_send(InputEvent::FastPath(events));
    }

    /// Send a mouse button event
    #[wasm_bindgen]
    pub fn send_mouse_button(&self, button: u8, is_pressed: bool, x: u16, y: u16) {
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

        let mut db = ironrdp::input::Database::new();
        let events = db.apply([move_op, op].into_iter());
        let _ = self.input_tx.unbounded_send(InputEvent::FastPath(events));
    }

    /// Send a mouse wheel event
    #[wasm_bindgen]
    pub fn send_mouse_wheel(&self, horizontal: bool, delta: i16) {
        let rotations = ironrdp::input::WheelRotations {
            is_vertical: !horizontal,
            rotation_units: delta,
        };
        let op = ironrdp::input::Operation::WheelRotations(rotations);
        let mut db = ironrdp::input::Database::new();
        let events = db.apply(std::iter::once(op));
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

    /// Request desktop resize (e.g. on fullscreen change)
    #[wasm_bindgen]
    pub fn resize(&mut self, width: u16, height: u16) {
        self.desktop_width = width;
        self.desktop_height = height;
        let _ = self.input_tx.unbounded_send(InputEvent::Resize { width, height });
    }

    /// Terminate the session
    #[wasm_bindgen]
    pub fn shutdown(&self) {
        let _ = self.input_tx.unbounded_send(InputEvent::Terminate);
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
                    log_error(&format!("CredSSP failed: {e:#}"));
                    log("Attempting to continue without CredSSP...");
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

async fn run_session(
    connection_result: ConnectionResult,
    mut framed: WasmFramed,
    writer_tx: mpsc::UnboundedSender<Vec<u8>>,
    mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
    mut canvas: Canvas,
    width: u16,
    height: u16,
) -> anyhow::Result<()> {
    let mut image = DecodedImage::new(PixelFormat::RgbA32, width, height);
    let mut active_stage = ActiveStage::new(connection_result);

    loop {
        let outputs = select! {
            frame = framed.read_pdu().fuse() => {
                let (action, payload) = frame.context("read RDP PDU")?;
                active_stage.process(&mut image, action, &payload)?
            }
            event = input_rx.next().fuse() => {
                match event {
                    Some(InputEvent::FastPath(events)) => {
                        active_stage.process_fastpath_input(&mut image, &events)
                            .context("process input")?
                    }
                    Some(InputEvent::Resize { width: w, height: h }) => {
                        log(&format!("Resize requested: {w}x{h}"));
                        match active_stage.encode_resize(u32::from(w), u32::from(h), None, None) {
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
                    canvas.draw(&image, region)?;
                    crate::notify_frame();
                }
                ActiveStageOutput::PointerDefault => {
                    canvas.set_cursor("default");
                }
                ActiveStageOutput::PointerHidden => {
                    canvas.set_cursor("none");
                }
                ActiveStageOutput::PointerBitmap(pointer) => {
                    canvas.set_custom_cursor(
                        &pointer.bitmap_data,
                        pointer.width as u32,
                        pointer.height as u32,
                        pointer.hotspot_x as u32,
                        pointer.hotspot_y as u32,
                    );
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

fn build_connector_config(
    username: String,
    password: String,
    domain: String,
    width: u16,
    height: u16,
) -> connector::Config {
    let domain = if domain.is_empty() { None } else { Some(domain) };

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
        autologon: false,
        enable_audio_playback: false,
        pointer_software_rendering: true,
        performance_flags: PerformanceFlags::default()
            | PerformanceFlags::DISABLE_WALLPAPER
            | PerformanceFlags::DISABLE_FULLWINDOWDRAG
            | PerformanceFlags::DISABLE_MENUANIMATIONS
            | PerformanceFlags::DISABLE_THEMING,
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
    }
}
