use ironrdp::cliprdr::backend::{ClipboardMessage, CliprdrBackend};
use ironrdp::cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::IntoOwned;
use futures_channel::mpsc;
use std::cell::RefCell;
use wasm_bindgen::prelude::*;

use crate::session::InputEvent;

/// Clipboard text cached from local paste events (JS → WASM).
thread_local! {
    static PENDING_CLIPBOARD_TEXT: RefCell<Option<String>> = RefCell::new(None);
}

/// Write text to the browser's clipboard (Remote → Local).
#[wasm_bindgen(inline_js = "
export function write_clipboard_text(text) {
    if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).catch(e => console.warn('Clipboard write failed:', e));
    }
}
")]
extern "C" {
    fn write_clipboard_text(text: &str);
}

#[derive(Debug)]
pub struct WasmCliprdrBackend {
    tx: mpsc::UnboundedSender<InputEvent>,
    remote_formats_to_read: Vec<ClipboardFormatId>,
}

impl WasmCliprdrBackend {
    pub fn new(tx: mpsc::UnboundedSender<InputEvent>) -> Self {
        Self {
            tx,
            remote_formats_to_read: Vec::new(),
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

/// Store text from a JS paste event for later retrieval by the CLIPRDR channel.
/// Called from JS when user pastes in the browser.
#[wasm_bindgen]
pub fn set_pending_clipboard(text: String, session: &crate::session::Session) {
    PENDING_CLIPBOARD_TEXT.with(|cell| {
        *cell.borrow_mut() = Some(text);
    });
    // Trigger the copy sequence
    let formats = vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
    session.send_cliprdr_message(ClipboardMessage::SendInitiateCopy(formats));
}

/// Decode UTF-16LE bytes to a Rust string.
fn from_utf16le(data: &[u8]) -> String {
    let u16_iter = data
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]));
    // Strip null terminator
    let chars: Vec<u16> = u16_iter.take_while(|&c| c != 0).collect();
    String::from_utf16_lossy(&chars)
}

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
        crate::log("Clipboard: format list requested (local → remote)");
        let text = PENDING_CLIPBOARD_TEXT.with(|cell| cell.borrow().clone());
        let formats = if text.is_some() {
            vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]
        } else {
            vec![]
        };
        let _ = self.tx.unbounded_send(InputEvent::Cliprdr(ClipboardMessage::SendInitiateCopy(formats)));
    }

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        self.remote_formats_to_read.clear();

        let mut has_text = false;
        for format in available_formats {
            if format.id() == ClipboardFormatId::CF_UNICODETEXT {
                has_text = true;
                self.remote_formats_to_read.push(format.id());
                break;
            }
        }

        if has_text {
            let format = self.remote_formats_to_read.pop().unwrap();
            let _ = self.tx.unbounded_send(InputEvent::Cliprdr(ClipboardMessage::SendInitiatePaste(format)));
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        if request.format == ClipboardFormatId::CF_UNICODETEXT {
            let text = PENDING_CLIPBOARD_TEXT.with(|cell| cell.borrow().clone()).unwrap_or_default();
            let response = ironrdp::cliprdr::pdu::FormatDataResponse::new_unicode_string(&text).into_owned();
            let _ = self.tx.unbounded_send(InputEvent::Cliprdr(ClipboardMessage::SendFormatData(response)));
        } else {
            let response = ironrdp::cliprdr::pdu::FormatDataResponse::new_error();
            let _ = self.tx.unbounded_send(InputEvent::Cliprdr(ClipboardMessage::SendFormatData(response)));
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() {
            crate::log_error("Clipboard: server sent error response");
            return;
        }

        let text = from_utf16le(response.data());
        if !text.is_empty() {
            write_clipboard_text(&text);
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
