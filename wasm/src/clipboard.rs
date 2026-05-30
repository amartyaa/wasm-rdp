use ironrdp::cliprdr::backend::{ClipboardMessage, CliprdrBackend};
use ironrdp::cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_cliprdr_format::bitmap::{dib_to_png, dibv5_to_png, png_to_cf_dibv5};
use ironrdp_core::IntoOwned;
use futures_channel::mpsc;
use std::cell::RefCell;
use wasm_bindgen::prelude::*;

use crate::session::InputEvent;

// ── Local clipboard cache ────────────────────────────────
// Cached from JS paste events for retrieval when the remote requests data.
thread_local! {
    static PENDING_CLIPBOARD_TEXT: RefCell<Option<String>> = RefCell::new(None);
    static PENDING_CLIPBOARD_IMAGE: RefCell<Option<Vec<u8>>> = RefCell::new(None);
}

// ── JS interop ───────────────────────────────────────────

/// Write text to the browser clipboard (Remote → Local).
#[wasm_bindgen(inline_js = "
export function write_clipboard_text(text) {
    // Cache for the copy-event fallback (used by app.js onCopy handler)
    window.__rdp_remote_clipboard_text = text;
    window.__rdp_remote_clipboard_image = null;

    // Try the async Clipboard API first (works on HTTPS + user gesture)
    if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).catch(() => {
            // Fallback: execCommand('copy') via hidden textarea
            _fallbackCopyText(text);
        });
    } else {
        _fallbackCopyText(text);
    }
}

function _fallbackCopyText(text) {
    try {
        const ta = document.createElement('textarea');
        ta.value = text;
        ta.style.position = 'fixed';
        ta.style.left = '-9999px';
        document.body.appendChild(ta);
        ta.select();
        document.execCommand('copy');
        document.body.removeChild(ta);
    } catch (e) {
        // Last resort: user must press Ctrl+C manually; data is cached
        console.warn('Clipboard write failed, data cached for Ctrl+C:', e);
    }
}
")]
extern "C" {
    fn write_clipboard_text(text: &str);
}

/// Write a PNG image to the browser clipboard (Remote → Local).
#[wasm_bindgen(inline_js = "
export function write_clipboard_image(pngBytes) {
    // Cache for the copy-event fallback
    window.__rdp_remote_clipboard_image = new Uint8Array(pngBytes);
    window.__rdp_remote_clipboard_text = null;

    try {
        const blob = new Blob([pngBytes], { type: 'image/png' });
        const item = new ClipboardItem({ 'image/png': blob });
        navigator.clipboard.write([item]).catch(e => console.warn('Clipboard image write failed:', e));
    } catch (e) {
        console.warn('ClipboardItem not supported, image cached for Ctrl+C:', e);
    }
}
")]
extern "C" {
    fn write_clipboard_image(png_bytes: &[u8]);
}

// ── Backend ──────────────────────────────────────────────

#[derive(Debug)]
pub struct WasmCliprdrBackend {
    tx: mpsc::UnboundedSender<InputEvent>,
    /// The format we most recently requested from the remote, so we can
    /// interpret the raw bytes in `on_format_data_response`.
    pending_format: Option<ClipboardFormatId>,
}

impl WasmCliprdrBackend {
    pub fn new(tx: mpsc::UnboundedSender<InputEvent>) -> Self {
        Self {
            tx,
            pending_format: None,
        }
    }
}

impl ironrdp_core::AsAny for WasmCliprdrBackend {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

// ── Public WASM API ──────────────────────────────────────

/// Store text from a JS paste event for later retrieval by the CLIPRDR channel.
#[wasm_bindgen]
pub fn set_pending_clipboard(text: String, session: &crate::session::Session) {
    PENDING_CLIPBOARD_IMAGE.with(|cell| *cell.borrow_mut() = None);
    PENDING_CLIPBOARD_TEXT.with(|cell| *cell.borrow_mut() = Some(text));
    let formats = vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
    session.send_cliprdr_message(ClipboardMessage::SendInitiateCopy(formats));
}

/// Store a PNG image from a JS paste event for later retrieval by the CLIPRDR channel.
#[wasm_bindgen]
pub fn set_pending_clipboard_image(png_bytes: &[u8], session: &crate::session::Session) {
    PENDING_CLIPBOARD_TEXT.with(|cell| *cell.borrow_mut() = None);
    PENDING_CLIPBOARD_IMAGE.with(|cell| *cell.borrow_mut() = Some(png_bytes.to_vec()));
    let formats = vec![ClipboardFormat::new(ClipboardFormatId::CF_DIBV5)];
    session.send_cliprdr_message(ClipboardMessage::SendInitiateCopy(formats));
}

// ── Helpers ──────────────────────────────────────────────

fn from_utf16le(data: &[u8]) -> String {
    let u16_iter = data
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]));
    let chars: Vec<u16> = u16_iter.take_while(|&c| c != 0).collect();
    String::from_utf16_lossy(&chars)
}

/// Pick the best image format from those advertised by the remote.
/// Preference: CF_DIBV5 > CF_DIB (both are universally supported).
fn pick_image_format(formats: &[ClipboardFormat]) -> Option<ClipboardFormatId> {
    let mut has_dibv5 = false;
    let mut has_dib = false;

    for f in formats {
        match f.id() {
            ClipboardFormatId::CF_DIBV5 => has_dibv5 = true,
            ClipboardFormatId::CF_DIB => has_dib = true,
            _ => {}
        }
    }

    if has_dibv5 {
        Some(ClipboardFormatId::CF_DIBV5)
    } else if has_dib {
        Some(ClipboardFormatId::CF_DIB)
    } else {
        None
    }
}

// ── CliprdrBackend impl ──────────────────────────────────

impl CliprdrBackend for WasmCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        ".clipboard"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {
        crate::log("Clipboard: channel ready");
    }

    fn on_request_format_list(&mut self) {
        let has_text = PENDING_CLIPBOARD_TEXT.with(|cell| cell.borrow().is_some());
        let has_image = PENDING_CLIPBOARD_IMAGE.with(|cell| cell.borrow().is_some());

        let mut formats = Vec::new();
        if has_text {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
        }
        if has_image {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIBV5));
        }
        let _ = self.tx.unbounded_send(InputEvent::Cliprdr(
            ClipboardMessage::SendInitiateCopy(formats),
        ));
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        _capabilities: ClipboardGeneralCapabilityFlags,
    ) {
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        // Prefer text; if no text, try image.
        let has_text = available_formats
            .iter()
            .any(|f| f.id() == ClipboardFormatId::CF_UNICODETEXT);

        if has_text {
            self.pending_format = Some(ClipboardFormatId::CF_UNICODETEXT);
            let _ = self.tx.unbounded_send(InputEvent::Cliprdr(
                ClipboardMessage::SendInitiatePaste(ClipboardFormatId::CF_UNICODETEXT),
            ));
            return;
        }

        if let Some(img_fmt) = pick_image_format(available_formats) {
            self.pending_format = Some(img_fmt);
            let _ = self.tx.unbounded_send(InputEvent::Cliprdr(
                ClipboardMessage::SendInitiatePaste(img_fmt),
            ));
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        if request.format == ClipboardFormatId::CF_UNICODETEXT {
            let text = PENDING_CLIPBOARD_TEXT
                .with(|cell| cell.borrow().clone())
                .unwrap_or_default();
            let response =
                FormatDataResponse::new_unicode_string(&text).into_owned();
            let _ = self.tx.unbounded_send(InputEvent::Cliprdr(
                ClipboardMessage::SendFormatData(response),
            ));
        } else if request.format == ClipboardFormatId::CF_DIBV5 {
            let png_data = PENDING_CLIPBOARD_IMAGE.with(|cell| cell.borrow().clone());
            let response = match png_data {
                Some(png) => match png_to_cf_dibv5(&png) {
                    Ok(dib_data) => {
                        FormatDataResponse::new_data(dib_data).into_owned()
                    }
                    Err(e) => {
                        crate::log_error(&format!("Clipboard: PNG→DIBV5 conversion failed: {e}"));
                        FormatDataResponse::new_error()
                    }
                },
                None => FormatDataResponse::new_error(),
            };
            let _ = self.tx.unbounded_send(InputEvent::Cliprdr(
                ClipboardMessage::SendFormatData(response),
            ));
        } else {
            let _ = self.tx.unbounded_send(InputEvent::Cliprdr(
                ClipboardMessage::SendFormatData(FormatDataResponse::new_error()),
            ));
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() {
            crate::log_error("Clipboard: server sent error response");
            self.pending_format = None;
            return;
        }

        let format = self.pending_format.take();
        match format {
            Some(ClipboardFormatId::CF_UNICODETEXT) => {
                let text = from_utf16le(response.data());
                if !text.is_empty() {
                    write_clipboard_text(&text);
                }
            }
            Some(ClipboardFormatId::CF_DIBV5) => {
                match dibv5_to_png(response.data()) {
                    Ok(png) => write_clipboard_image(&png),
                    Err(e) => crate::log_error(&format!(
                        "Clipboard: DIBV5→PNG conversion failed: {e}"
                    )),
                }
            }
            Some(ClipboardFormatId::CF_DIB) => {
                match dib_to_png(response.data()) {
                    Ok(png) => write_clipboard_image(&png),
                    Err(e) => crate::log_error(&format!(
                        "Clipboard: DIB→PNG conversion failed: {e}"
                    )),
                }
            }
            _ => {
                crate::log_error("Clipboard: unexpected format data response (no pending format)");
            }
        }
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}
    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}
    fn on_lock(&mut self, _data_id: LockDataId) {}
    fn on_unlock(&mut self, _data_id: LockDataId) {}

    fn now_ms(&self) -> u64 {
        js_sys::Date::now() as u64
    }
}
