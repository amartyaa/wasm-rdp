use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower_http::services::ServeDir;
use tracing::{error, info};

#[derive(Parser, Clone)]
#[command(name = "web-rdp-server", about = "WebSocket-to-TCP proxy for IronRDP WASM client")]
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

    /// Path to web static files
    #[arg(long, default_value = "web")]
    web_dir: PathBuf,
}

#[derive(Clone)]
struct AppConfig {
    rdp_target: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=debug".into()),
        )
        .init();

    let args = Args::parse();
    let config = AppConfig {
        rdp_target: args.rdp_target.clone(),
    };

    let base = args.base_path.trim_end_matches('/').to_string();

    // Static file serving for the web directory
    let serve_dir = ServeDir::new(&args.web_dir);

    let app = if base.is_empty() {
        // Base path is root "/"
        Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .fallback_service(serve_dir)
            .with_state(config)
    } else {
        // Base path is e.g. "/rdp"
        let nested = Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .fallback_service(serve_dir)
            .with_state(config);
        Router::new().nest(&base, nested)
    };

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    info!("Server listening on http://{addr}{base}/");
    info!("RDP target: {}", args.rdp_target);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
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
