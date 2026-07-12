//! RAIL (RemoteApp) glue: forwards the `ironrdp-rail` crate's decoded window
//! events and exec results to the JS window manager (`app.js`).
//!
//! Window metadata arrives as graphics Orders (see `session.rs`
//! `ActiveStageOutput::Orders`); the exec result arrives on the rail SVC channel
//! via [`WasmRailHandler`]. Both are surfaced to JS through cached-callback
//! bridges, the same pattern as `notify_frame` / `notify_audio_data` in `lib.rs`.

use std::cell::RefCell;

use ironrdp_rail::{RailClientHandler, WindowEvent, ExecResult};
use wasm_bindgen::JsValue;
use wasm_bindgen::prelude::*;

use crate::log;

/// Sink for the server's Execute Result — drives the JS launch-failure toast.
#[derive(Debug)]
pub struct WasmRailHandler;

impl RailClientHandler for WasmRailHandler {
    fn on_exec_result(&mut self, result: ExecResult) {
        notify_rail_exec_result(result.exec_result, result.raw_result);
    }
}

/// Parse a RAIL Orders payload and forward each window event to JS.
pub fn dispatch_window_events(data: &[u8]) {
    let events = ironrdp_rail::parse_window_orders(data);
    // ponytail: milestone-1 diagnostic — confirms orders arrive and parse.
    log(&format!("[RAIL] orders: {} bytes -> {} window event(s)", data.len(), events.len()));
    for ev in events {
        match ev {
            WindowEvent::NewOrUpdate(w) => {
                let vis: Vec<i32> = w
                    .visibility_rects
                    .map(|rects| {
                        rects
                            .iter()
                            .flat_map(|r| [r.left as i32, r.top as i32, r.right as i32, r.bottom as i32])
                            .collect()
                    })
                    .unwrap_or_default();
                notify_rail_window(
                    w.window_id,
                    w.is_new,
                    w.x.is_some(),
                    w.x.unwrap_or(0),
                    w.y.unwrap_or(0),
                    w.width.is_some(),
                    w.width.unwrap_or(0) as i32,
                    w.height.unwrap_or(0) as i32,
                    w.show_state.map(i32::from).unwrap_or(-1),
                    w.title,
                    vis,
                );
            }
            WindowEvent::Deleted(id) => notify_rail_window_deleted(id),
            WindowEvent::ZOrder(ids) => notify_rail_zorder(&ids),
            // Forwarded to JS: the ACTIVE_WND desktop order is the server's
            // "window is fully created and foreground" signal — JS uses it to
            // time window commands (e.g. the first-window auto-maximize, which
            // is ignored host-side if sent at window birth).
            WindowEvent::ActiveWindow(id) => {
                log(&format!("[RAIL] active window 0x{id:08x}"));
                notify_rail_active(id);
            }
        }
    }
}

/// Look up a cached `window.<name>` JS function (mirrors `lib.rs` bridges).
fn cached_fn(cell: &'static std::thread::LocalKey<RefCell<Option<js_sys::Function>>>, name: &str) -> Option<js_sys::Function> {
    cell.with(|c| {
        let mut cached = c.borrow_mut();
        if cached.is_none() {
            if let Some(window) = web_sys::window() {
                if let Ok(func) = js_sys::Reflect::get(&JsValue::from(window), &JsValue::from_str(name)) {
                    *cached = func.dyn_ref::<js_sys::Function>().cloned();
                }
            }
        }
        cached.clone()
    })
}

/// `window.__rdp_rail_window(id, isNew, hasPos, x, y, hasSize, w, h, showState, title, visRects)`.
/// `showState` is -1 when the order didn't carry it; `title` is null when absent;
/// `visRects` is a flat `[l,t,r,b, …]` Int32Array (empty ⇒ no clip change).
#[allow(clippy::too_many_arguments)]
fn notify_rail_window(
    id: u32,
    is_new: bool,
    has_pos: bool,
    x: i32,
    y: i32,
    has_size: bool,
    w: i32,
    h: i32,
    show_state: i32,
    title: Option<String>,
    vis_rects: Vec<i32>,
) {
    thread_local! {
        static CACHED_FN: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
    }
    let Some(func) = cached_fn(&CACHED_FN, "__rdp_rail_window") else { return };
    let args = js_sys::Array::new();
    args.push(&JsValue::from(id));
    args.push(&JsValue::from(is_new));
    args.push(&JsValue::from(has_pos));
    args.push(&JsValue::from(x));
    args.push(&JsValue::from(y));
    args.push(&JsValue::from(has_size));
    args.push(&JsValue::from(w));
    args.push(&JsValue::from(h));
    args.push(&JsValue::from(show_state));
    match title {
        Some(t) => args.push(&JsValue::from_str(&t)),
        None => args.push(&JsValue::NULL),
    };
    args.push(&js_sys::Int32Array::from(vis_rects.as_slice()).into());
    let _ = func.apply(&JsValue::NULL, &args);
}

/// `window.__rdp_rail_active(id)` — server reports this window became foreground.
fn notify_rail_active(id: u32) {
    thread_local! {
        static CACHED_FN: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
    }
    if let Some(func) = cached_fn(&CACHED_FN, "__rdp_rail_active") {
        let _ = func.call1(&JsValue::NULL, &JsValue::from(id));
    }
}

/// `window.__rdp_rail_window_deleted(id)`.
fn notify_rail_window_deleted(id: u32) {
    thread_local! {
        static CACHED_FN: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
    }
    if let Some(func) = cached_fn(&CACHED_FN, "__rdp_rail_window_deleted") {
        let _ = func.call1(&JsValue::NULL, &JsValue::from(id));
    }
}

/// `window.__rdp_rail_zorder(ids)` — top-most first, as a Uint32Array.
fn notify_rail_zorder(ids: &[u32]) {
    thread_local! {
        static CACHED_FN: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
    }
    if let Some(func) = cached_fn(&CACHED_FN, "__rdp_rail_zorder") {
        let _ = func.call1(&JsValue::NULL, &js_sys::Uint32Array::from(ids).into());
    }
}

/// `window.__rdp_rail_exec_result(execResult, rawResult)` — 0 ⇒ success.
fn notify_rail_exec_result(exec_result: u16, raw_result: u32) {
    thread_local! {
        static CACHED_FN: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
    }
    if let Some(func) = cached_fn(&CACHED_FN, "__rdp_rail_exec_result") {
        let _ = func.call2(&JsValue::NULL, &JsValue::from(exec_result), &JsValue::from(raw_result));
    }
}
