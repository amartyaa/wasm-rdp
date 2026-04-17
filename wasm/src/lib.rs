use wasm_bindgen::prelude::*;

mod session;
mod canvas;
mod input;
mod framed;
mod clipboard;

/// Initialize the WASM module. Call this once before anything else.
#[wasm_bindgen(start)]
pub fn init() {
    std::panic::set_hook(Box::new(|info| {
        log_error(&format!("PANIC: {info}"));
    }));
    log("IronRDP WASM module initialized");
}

/// Connect to an RDP server through the WebSocket proxy.
/// Returns a Session handle for the active RDP session.
#[wasm_bindgen]
pub async fn connect(
    ws_url: String,
    username: String,
    password: String,
    domain: String,
    width: u16,
    height: u16,
    canvas_id: String,
) -> Result<session::Session, JsValue> {
    session::Session::connect(ws_url, username, password, domain, width, height, canvas_id)
        .await
        .map_err(|e| JsValue::from_str(&format!("{e:#}")))
}

/// Called from session.rs on each graphics update to notify JS FPS counter
pub(crate) fn notify_frame() {
    if let Ok(func) = js_sys::Reflect::get(
        &wasm_bindgen::JsValue::from(web_sys::window().unwrap()),
        &wasm_bindgen::JsValue::from_str("__rdp_frame"),
    ) {
        if let Some(func) = func.dyn_ref::<js_sys::Function>() {
            let _ = func.call0(&wasm_bindgen::JsValue::NULL);
        }
    }
}

/// Log to browser console
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn log(s: &str);
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn log_error(s: &str);
}
