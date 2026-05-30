use wasm_bindgen::prelude::*;

mod session;
mod canvas;
mod input;
mod framed;
mod clipboard;
mod audio;

/// Initialize the WASM module. Call this once before anything else.
#[wasm_bindgen(start)]
pub fn init() {
    std::panic::set_hook(Box::new(|info| {
        log_error(&format!("PANIC: {info}"));
    }));
    log(&format!("WASM module version {} loaded", env!("APP_VERSION")));
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

/// Called from session.rs on each graphics update to notify JS FPS counter.
/// Caches the JS function reference to avoid per-frame Reflect::get lookups.
pub(crate) fn notify_frame() {
    use std::cell::RefCell;
    thread_local! {
        static CACHED_FN: RefCell<Option<js_sys::Function>> = RefCell::new(None);
        static LOOKED_UP: RefCell<bool> = RefCell::new(false);
    }
    CACHED_FN.with(|cell| {
        let mut cached = cell.borrow_mut();
        if cached.is_none() {
            LOOKED_UP.with(|looked| {
                if !*looked.borrow() {
                    *looked.borrow_mut() = true;
                    if let Some(window) = web_sys::window() {
                        if let Ok(func) = js_sys::Reflect::get(
                            &wasm_bindgen::JsValue::from(window),
                            &wasm_bindgen::JsValue::from_str("__rdp_frame"),
                        ) {
                            *cached = func.dyn_ref::<js_sys::Function>().cloned();
                        }
                    }
                }
            });
        }
        if let Some(func) = cached.as_ref() {
            let _ = func.call0(&wasm_bindgen::JsValue::NULL);
        }
    });
}

/// Called from session.rs when the RDP session ends (error or clean disconnect).
/// Notifies JS so it can trigger reconnection or show the login screen.
pub(crate) fn notify_session_ended(reason: &str) {
    if let Ok(func) = js_sys::Reflect::get(
        &wasm_bindgen::JsValue::from(web_sys::window().unwrap()),
        &wasm_bindgen::JsValue::from_str("__rdp_session_ended"),
    ) {
        if let Some(func) = func.dyn_ref::<js_sys::Function>() {
            let _ = func.call1(
                &wasm_bindgen::JsValue::NULL,
                &wasm_bindgen::JsValue::from_str(reason),
            );
        }
    }
}

/// Called from audio.rs to forward PCM audio data to JavaScript for playback.
/// Caches the JS function reference to avoid per-call Reflect::get lookups.
pub(crate) fn notify_audio_data(channels: u16, sample_rate: u32, bits_per_sample: u16, data: &[u8]) {
    use std::cell::RefCell;
    thread_local! {
        static CACHED_FN: RefCell<Option<js_sys::Function>> = RefCell::new(None);
        static LOOKED_UP: RefCell<bool> = RefCell::new(false);
    }
    CACHED_FN.with(|cell| {
        let mut cached = cell.borrow_mut();
        if cached.is_none() {
            LOOKED_UP.with(|looked| {
                if !*looked.borrow() {
                    *looked.borrow_mut() = true;
                    if let Some(window) = web_sys::window() {
                        if let Ok(func) = js_sys::Reflect::get(
                            &wasm_bindgen::JsValue::from(window),
                            &wasm_bindgen::JsValue::from_str("__rdp_audio_data"),
                        ) {
                            *cached = func.dyn_ref::<js_sys::Function>().cloned();
                        }
                    }
                }
            });
        }
        if let Some(func) = cached.as_ref() {
            let arr = js_sys::Uint8Array::from(data);
            let _ = func.call4(
                &wasm_bindgen::JsValue::NULL,
                &wasm_bindgen::JsValue::from(channels),
                &wasm_bindgen::JsValue::from(sample_rate),
                &wasm_bindgen::JsValue::from(bits_per_sample),
                &arr.into(),
            );
        }
    });
}

/// Log to browser console
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn log(s: &str);
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn log_error(s: &str);
}
