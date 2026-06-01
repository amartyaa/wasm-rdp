use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use rust_embed::Embed;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower_http::services::ServeDir;
use tracing::{error, info};

/// Embedded web assets (compiled into the binary).
#[derive(Embed)]
#[folder = "../web/"]
struct WebAssets;

#[derive(Parser, Clone)]
#[command(name = "ironbridge", version = env!("APP_VERSION"), disable_version_flag = true, about = "IronBridge — Browser-Native RDP Client")]
struct Args {
    /// Port to listen on
    #[arg(long, default_value = "8080")]
    port: u16,

    /// Base URL path (e.g. "/rdp")
    #[arg(long, default_value = "/")]
    base_path: String,

    /// RDP target address
    #[arg(long, default_value = "localhost:3389")]
    rdp_target: String,

    /// Path to web static files (overrides embedded assets for development)
    #[arg(long)]
    web_dir: Option<PathBuf>,

    /// Run as a Windows service / Linux daemon
    #[arg(long)]
    service: bool,

    /// Application name shown on the login page and browser tab.
    /// Defaults to "Remote Desktop" when omitted.
    #[arg(long)]
    app_name: Option<String>,

    /// Enable bidirectional text/image clipboard sync between browser and remote.
    /// Default: disabled. Also controlled via IB_TEXT_CLIPBOARD_SYNC env var (CLI takes priority).
    #[arg(long, default_value_t = false, env = "IB_TEXT_CLIPBOARD_SYNC")]
    enable_text_clipboard_sync: bool,

    /// Enable bidirectional file clipboard transfer (CLIPRDR FileGroupDescriptorW).
    /// Default: disabled. Also controlled via IB_FILE_CLIPBOARD_SYNC env var (CLI takes priority).
    #[arg(long, default_value_t = false, env = "IB_FILE_CLIPBOARD_SYNC")]
    enable_file_clipboard_sync: bool,

    /// Print version
    #[arg(short = 'v', short_alias = 'V', long = "version", action = clap::ArgAction::Version)]
    version: (),
}

#[derive(Clone)]
struct AppConfig {
    rdp_target: String,
    /// Injected into index.html as window.__APP_NAME so JS can brand the UI.
    app_name: Option<String>,
    enable_text_clipboard: bool,
    enable_file_clipboard: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    #[cfg(windows)]
    if args.service {
        run_as_windows_service();
        return;
    }

    run_server(args).await;
}

async fn run_server(args: Args) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=debug".into()),
        )
        .init();

    let config = AppConfig {
        rdp_target: args.rdp_target.clone(),
        app_name: args.app_name.clone(),
        enable_text_clipboard: args.enable_text_clipboard_sync,
        enable_file_clipboard: args.enable_file_clipboard_sync,
    };

    if let Some(name) = &config.app_name {
        info!("App name: {name}");
    }
    info!("Text clipboard sync: {}", if config.enable_text_clipboard { "enabled" } else { "disabled" });
    info!("File clipboard sync: {}", if config.enable_file_clipboard { "enabled" } else { "disabled" });

    let base = args.base_path.trim_end_matches('/').to_string();

    let app = if let Some(web_dir) = &args.web_dir {
        info!("Serving web assets from disk: {}", web_dir.display());
        let serve_dir = ServeDir::new(web_dir);

        if base.is_empty() {
            Router::new()
                .route("/ws", axum::routing::get(ws_handler))
                .route("/ping", axum::routing::get(ping_handler))
                .fallback_service(serve_dir)
                .with_state(config)
        } else {
            let nested = Router::new()
                .route("/ws", axum::routing::get(ws_handler))
                .route("/ping", axum::routing::get(ping_handler))
                .fallback_service(serve_dir)
                .with_state(config);
            Router::new().nest(&base, nested)
        }
    } else {
        info!("Serving embedded web assets");

        if base.is_empty() {
            Router::new()
                .route("/ws", axum::routing::get(ws_handler))
                .route("/ping", axum::routing::get(ping_handler))
                .fallback(embedded_handler)
                .with_state(config)
        } else {
            let nested = Router::new()
                .route("/ws", axum::routing::get(ws_handler))
                .route("/ping", axum::routing::get(ping_handler))
                .fallback(embedded_handler)
                .with_state(config);
            Router::new().nest(&base, nested)
        }
    };

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    info!("IronBridge listening on http://{addr}{}/", if base.is_empty() { "" } else { &base });
    info!("RDP target: {}", args.rdp_target);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

/// Serve files from the embedded `WebAssets`.
/// For `index.html`, injects `window.__APP_NAME` when `--app-name` was given so
/// JS can brand the login page title, heading, and popup titles without a rebuild.
async fn embedded_handler(uri: Uri, State(config): State<AppConfig>) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match WebAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();

            // Config injection: patch index.html with a <script> that sets
            // feature-flag globals so JS can gate clipboard and branding at
            // runtime without a rebuild. All other assets are served verbatim.
            let body = if path == "index.html" {
                if let Ok(html) = std::str::from_utf8(&content.data) {
                    let mut vars = format!(
                        "window.__IB_TEXT_CLIPBOARD={};window.__IB_FILE_CLIPBOARD={};",
                        config.enable_text_clipboard,
                        config.enable_file_clipboard,
                    );
                    if let Some(name) = &config.app_name {
                        let safe = name.replace('\\', "\\\\").replace('"', "\\\"");
                        vars.push_str(&format!(r#"window.__APP_NAME="{safe}";"#));
                    }
                    let script = format!("<script>{vars}</script>");
                    let patched = html.replace("</head>", &format!("{script}</head>"));
                    Body::from(patched.into_bytes())
                } else {
                    Body::from(content.data.to_vec())
                }
            } else {
                Body::from(content.data.to_vec())
            };

            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap())
                .body(body)
                .unwrap()
        }
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("404 Not Found"))
            .unwrap(),
    }
}

async fn ping_handler() -> &'static str {
    "pong"
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(config): State<AppConfig>,
) -> impl IntoResponse {
    info!("WebSocket upgrade request");
    ws.on_upgrade(move |socket| handle_ws(socket, config))
}

/// WebSocket ↔ TCP proxy with TLS upgrade support.
///
/// Protocol:
///   1. Initially relays binary WS messages to raw TCP (pre-TLS X.224 negotiation).
///   2. When WASM client sends a text message `{"cmd":"tls_upgrade"}`, the proxy:
///      a. Upgrades the TCP connection to TLS (accepting any certificate).
///      b. Extracts the server's TLS certificate as DER bytes.
///      c. Sends back `{"cmd":"tls_ready","server_cert":"<hex>"}`.
///   3. After TLS upgrade, continues relaying binary WS ↔ TLS-TCP.
async fn handle_ws(ws: WebSocket, config: AppConfig) {
    info!("Connecting to RDP target: {}", config.rdp_target);

    let tcp = match TcpStream::connect(&config.rdp_target).await {
        Ok(stream) => stream,
        Err(e) => {
            error!("Failed to connect to RDP target: {e}");
            return;
        }
    };

    // Disable Nagle's algorithm: RDP is latency-sensitive and interactive,
    // so small input/graphics PDUs must not be delayed waiting to coalesce.
    if let Err(e) = tcp.set_nodelay(true) {
        error!("Failed to set TCP_NODELAY: {e}");
    }

    let (mut ws_write, mut ws_read) = ws.split();

    // Phase 1: Raw TCP relay until TLS upgrade command
    let (mut tcp_read, mut tcp_write) = tcp.into_split();

    // Reusable read buffer — hoisted out of the loop to avoid a 64 KB
    // allocation + zero-fill on every select! iteration.
    let mut buf = vec![0u8; 65536];

    loop {
        tokio::select! {
            // WS → TCP
            msg = ws_read.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        if tcp_write.write_all(&data).await.is_err() {
                            info!("TCP write failed during pre-TLS phase");
                            return;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        // Check for TLS upgrade command
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                            if json.get("cmd").and_then(|v| v.as_str()) == Some("tls_upgrade") {
                                info!("TLS upgrade requested by client");

                                // Reunite the TCP halves for TLS upgrade
                                let tcp_stream = tcp_read.reunite(tcp_write).unwrap();

                                // Perform TLS handshake (accept any cert — the WASM client validates)
                                let tls_connector = native_tls::TlsConnector::builder()
                                    .danger_accept_invalid_certs(true)
                                    .danger_accept_invalid_hostnames(true)
                                    .use_sni(false)
                                    .build()
                                    .unwrap();
                                let tls_connector = tokio_native_tls::TlsConnector::from(tls_connector);

                                let tls_stream = match tls_connector.connect("rdp", tcp_stream).await {
                                    Ok(s) => s,
                                    Err(e) => {
                                        error!("TLS handshake failed: {e}");
                                        let err_msg = serde_json::json!({
                                            "cmd": "tls_error",
                                            "error": format!("{e}")
                                        });
                                        let _ = ws_write.send(Message::Text(err_msg.to_string().into())).await;
                                        return;
                                    }
                                };

                                // Extract server certificate DER bytes
                                let server_cert_der = tls_stream.get_ref()
                                    .peer_certificate()
                                    .ok()
                                    .flatten()
                                    .map(|cert| cert.to_der().unwrap_or_default())
                                    .unwrap_or_default();

                                let cert_hex = hex_encode(&server_cert_der);

                                info!("TLS upgrade complete, cert {} bytes", server_cert_der.len());

                                // Send cert to WASM client
                                let ready_msg = serde_json::json!({
                                    "cmd": "tls_ready",
                                    "server_cert": cert_hex
                                });
                                if ws_write.send(Message::Text(ready_msg.to_string().into())).await.is_err() {
                                    error!("Failed to send tls_ready");
                                    return;
                                }

                                // Phase 2: TLS relay
                                run_tls_relay(tls_stream, ws_read, ws_write).await;
                                return;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                        info!("WebSocket closed during pre-TLS phase");
                        return;
                    }
                    _ => {}
                }
            }
            // TCP → WS
            n = tcp_read.read(&mut buf) => {
                match n {
                    Ok(0) | Err(_) => {
                        info!("TCP closed during pre-TLS phase");
                        return;
                    }
                    Ok(n) => {
                        if ws_write.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                            info!("WS write failed during pre-TLS phase");
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Relay between WebSocket and a TLS-wrapped TCP stream.
async fn run_tls_relay(
    tls_stream: tokio_native_tls::TlsStream<TcpStream>,
    mut ws_read: futures_util::stream::SplitStream<WebSocket>,
    mut ws_write: futures_util::stream::SplitSink<WebSocket, Message>,
) {
    let (mut tls_read, mut tls_write) = tokio::io::split(tls_stream);

    let ws_to_tls = async {
        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    if tls_write.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    };

    let tls_to_ws = async {
        let mut buf = vec![0u8; 65536];
        loop {
            match tls_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_write
                        .send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    };

    tokio::select! {
        _ = ws_to_tls => info!("WS→TLS stream ended"),
        _ = tls_to_ws => info!("TLS→WS stream ended"),
    }

    info!("TLS WebSocket session closed");
}

/// Simple hex encoder (avoids adding the `hex` crate dependency).
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ── Windows Service Support ──────────────────────────────────
#[cfg(windows)]
fn run_as_windows_service() {
    use std::ffi::OsString;
    use windows_service::{
        define_windows_service,
        service_dispatcher,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode,
            ServiceState, ServiceStatus, ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
    };

    const SERVICE_NAME: &str = "IronBridgeRDP";

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(e) = run_service() {
            eprintln!("Service error: {e}");
        }
    }

    fn run_service() -> Result<(), Box<dyn std::error::Error>> {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown_tx = std::sync::Mutex::new(Some(shutdown_tx));

        let status_handle = service_control_handler::register(
            SERVICE_NAME,
            move |control_event| -> ServiceControlHandlerResult {
                match control_event {
                    ServiceControl::Stop => {
                        if let Some(tx) = shutdown_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        ServiceControlHandlerResult::NoError
                    }
                    ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                    _ => ServiceControlHandlerResult::NotImplemented,
                }
            },
        )?;

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: std::time::Duration::default(),
            process_id: None,
        })?;

        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let args = Args::parse();
            let server_future = run_server(args);
            tokio::select! {
                _ = server_future => {},
                _ = shutdown_rx => {
                    tracing::info!("Service stop requested");
                },
            }
        });

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: std::time::Duration::default(),
            process_id: None,
        })?;

        Ok(())
    }

    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .expect("Failed to start service dispatcher");
}
