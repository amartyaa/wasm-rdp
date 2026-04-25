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
/// At build time, the contents of `../web/` are baked in.
#[derive(Embed)]
#[folder = "../web/"]
struct WebAssets;

#[derive(Parser, Clone)]
#[command(name = "ironbridge", about = "IronBridge — Browser-Native RDP Client")]
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
}

#[derive(Clone)]
struct AppConfig {
    rdp_target: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Windows service mode
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
    };

    let base = args.base_path.trim_end_matches('/').to_string();

    let app = if let Some(web_dir) = &args.web_dir {
        // Dev mode: serve from disk
        info!("Serving web assets from disk: {}", web_dir.display());
        let serve_dir = ServeDir::new(web_dir);

        if base.is_empty() {
            Router::new()
                .route("/ws", axum::routing::get(ws_handler))
                .fallback_service(serve_dir)
                .with_state(config)
        } else {
            let nested = Router::new()
                .route("/ws", axum::routing::get(ws_handler))
                .fallback_service(serve_dir)
                .with_state(config);
            Router::new().nest(&base, nested)
        }
    } else {
        // Production mode: serve embedded assets
        info!("Serving embedded web assets");

        if base.is_empty() {
            Router::new()
                .route("/ws", axum::routing::get(ws_handler))
                .fallback(embedded_handler)
                .with_state(config)
        } else {
            let nested = Router::new()
                .route("/ws", axum::routing::get(ws_handler))
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

/// Serve files from the embedded `WebAssets` at compile time.
async fn embedded_handler(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');

    // Default to index.html for root
    let path = if path.is_empty() { "index.html" } else { path };

    match WebAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();

            Response::builder()
                .status(StatusCode::OK)
                .header(
                    header::CONTENT_TYPE,
                    HeaderValue::from_str(&mime).unwrap(),
                )
                .body(Body::from(content.data.to_vec()))
                .unwrap()
        }
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("404 Not Found"))
            .unwrap(),
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(config): State<AppConfig>,
) -> impl IntoResponse {
    info!("WebSocket upgrade request");
    ws.on_upgrade(move |socket| handle_ws(socket, config))
}

async fn handle_ws(ws: WebSocket, config: AppConfig) {
    info!("Connecting to RDP target: {}", config.rdp_target);

    let tcp = match TcpStream::connect(&config.rdp_target).await {
        Ok(stream) => stream,
        Err(e) => {
            error!("Failed to connect to RDP target: {e}");
            return;
        }
    };

    let (tcp_read, tcp_write) = tcp.into_split();
    let (ws_write, ws_read) = ws.split();

    // WS → TCP: forward binary messages to the RDP server
    let ws_to_tcp = {
        let mut ws_read = ws_read;
        let mut tcp_write = tcp_write;
        async move {
            while let Some(msg) = ws_read.next().await {
                match msg {
                    Ok(Message::Binary(data)) => {
                        if tcp_write.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {} // ignore text/ping/pong
                }
            }
        }
    };

    // TCP → WS: forward RDP responses back to the browser
    let tcp_to_ws = {
        let mut tcp_read = tcp_read;
        let mut ws_write = ws_write;
        async move {
            let mut buf = vec![0u8; 16384];
            loop {
                match tcp_read.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if ws_write
                            .send(Message::Binary(buf[..n].to_vec().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    };

    tokio::select! {
        _ = ws_to_tcp => info!("WS→TCP stream ended"),
        _ = tcp_to_ws => info!("TCP→WS stream ended"),
    }

    info!("WebSocket session closed");
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

        // Build a runtime and run the server
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
