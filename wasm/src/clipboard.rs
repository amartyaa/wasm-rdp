use ironrdp::cliprdr::backend::{ClipboardMessage, CliprdrBackend};
use ironrdp::cliprdr::pdu::{
    ClipboardFileAttributes, ClipboardFormat, ClipboardFormatId,
    ClipboardGeneralCapabilityFlags, FileContentsFlags,
    FileContentsRequest, FileContentsResponse, FileDescriptor, FormatDataRequest,
    FormatDataResponse, LockDataId, FORMAT_NAME_FILE_LIST,
};
use ironrdp_cliprdr_format::bitmap::{dib_to_png, dibv5_to_png, png_to_cf_dibv5};
use ironrdp_core::IntoOwned;
use futures_channel::mpsc;
use std::cell::RefCell;
use std::collections::HashMap;
use wasm_bindgen::prelude::*;

use crate::session::InputEvent;

// ── Download chunk size ──────────────────────────────────
// 4 MB per RANGE request — covers typical clipboard files in one round-trip.
const CHUNK_SIZE: u32 = 4 * 1024 * 1024;

// ── Per-file download tracking ───────────────────────────
enum FileTransferState {
    AwaitingSize { file_index: i32, name: String, data_id: Option<u32> },
    AwaitingData { file_index: i32, name: String, total: u64, data: Vec<u8>, data_id: Option<u32> },
}

// ── Thread-local clipboard caches ────────────────────────
thread_local! {
    static PENDING_CLIPBOARD_TEXT: RefCell<Option<String>> = RefCell::new(None);
    static PENDING_CLIPBOARD_IMAGE: RefCell<Option<Vec<u8>>> = RefCell::new(None);
    // Local→remote file cache: (filename, file_bytes) for each pending file.
    static PENDING_FILES: RefCell<Vec<(String, Vec<u8>)>> = RefCell::new(Vec::new());
    // Remote file format ID cached until the user explicitly clicks "Download".
    static PENDING_REMOTE_FILE_FORMAT: RefCell<Option<ClipboardFormatId>> = RefCell::new(None);
}

// ── JS interop ───────────────────────────────────────────

#[wasm_bindgen(inline_js = "
export function write_clipboard_text(text) {
    window.__rdp_remote_clipboard_text = text;
    window.__rdp_remote_clipboard_image = null;
    if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).catch(() => { _fallbackCopyText(text); });
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
        console.warn('Clipboard write failed, data cached for Ctrl+C:', e);
    }
}
")]
extern "C" {
    fn write_clipboard_text(text: &str);
}

#[wasm_bindgen(inline_js = "
export function write_clipboard_image(pngBytes) {
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

/// Notify JS that the remote clipboard contains files (without downloading yet).
#[wasm_bindgen(inline_js = "
export function notify_remote_clipboard_has_files() {
    window.__rdp_remote_has_files && window.__rdp_remote_has_files();
}
")]
extern "C" {
    fn notify_remote_clipboard_has_files();
}

/// Trigger a browser file download for a file received from the remote.
#[wasm_bindgen(inline_js = "
export function download_file(name, bytes) {
    try {
        const blob = new Blob([bytes], { type: 'application/octet-stream' });
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = name;
        document.body.appendChild(a);
        a.click();
        document.body.removeChild(a);
        setTimeout(() => URL.revokeObjectURL(url), 5000);
    } catch (e) {
        console.warn('File download failed:', e);
    }
}
")]
extern "C" {
    fn download_file(name: &str, bytes: &[u8]);
}

// ── Backend ──────────────────────────────────────────────

#[derive(Debug)]
pub struct WasmCliprdrBackend {
    tx: mpsc::UnboundedSender<InputEvent>,
    /// Format we most recently requested from the remote (to interpret the response).
    pending_format: Option<ClipboardFormatId>,
    /// Monotonically increasing stream ID for FileContentsRequests we issue.
    next_stream_id: u32,
    /// In-flight file download states keyed by stream_id.
    file_transfers: HashMap<u32, FileTransferState>,
    enable_text: bool,
    enable_file: bool,
}

impl std::fmt::Debug for FileTransferState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileTransferState::AwaitingSize { file_index, name, .. } =>
                write!(f, "AwaitingSize(index={file_index}, name={name})"),
            FileTransferState::AwaitingData { file_index, name, total, data, .. } =>
                write!(f, "AwaitingData(index={file_index}, name={name}, {}/{total})", data.len()),
        }
    }
}

impl WasmCliprdrBackend {
    pub fn new(tx: mpsc::UnboundedSender<InputEvent>, enable_text: bool, enable_file: bool) -> Self {
        Self {
            tx,
            pending_format: None,
            next_stream_id: 1,
            file_transfers: HashMap::new(),
            enable_text,
            enable_file,
        }
    }

    fn alloc_stream_id(&mut self) -> u32 {
        let id = self.next_stream_id;
        self.next_stream_id = self.next_stream_id.wrapping_add(1).max(1);
        id
    }

    fn request_file_size(&mut self, file_index: i32, name: String, data_id: Option<u32>) {
        let stream_id = self.alloc_stream_id();
        self.file_transfers.insert(stream_id, FileTransferState::AwaitingSize {
            file_index,
            name,
            data_id,
        });
        let request = FileContentsRequest {
            stream_id,
            index: file_index,
            flags: FileContentsFlags::SIZE,
            position: 0,
            requested_size: 8,
            data_id,
        };
        let _ = self.tx.unbounded_send(InputEvent::Cliprdr(
            ClipboardMessage::SendFileContentsRequest(request),
        ));
    }

    fn request_file_range(&mut self, file_index: i32, name: String, total: u64,
                          data: Vec<u8>, data_id: Option<u32>) {
        let position = data.len() as u64;
        let remaining = total.saturating_sub(position);
        let chunk = remaining.min(CHUNK_SIZE as u64) as u32;

        if chunk == 0 {
            // Empty file or already complete — download immediately.
            download_file(&name, &data);
            return;
        }

        let stream_id = self.alloc_stream_id();
        self.file_transfers.insert(stream_id, FileTransferState::AwaitingData {
            file_index,
            name,
            total,
            data,
            data_id,
        });
        let request = FileContentsRequest {
            stream_id,
            index: file_index,
            flags: FileContentsFlags::RANGE,
            position,
            requested_size: chunk,
            data_id,
        };
        let _ = self.tx.unbounded_send(InputEvent::Cliprdr(
            ClipboardMessage::SendFileContentsRequest(request),
        ));
    }
}

impl ironrdp_core::AsAny for WasmCliprdrBackend {
    fn as_any(&self) -> &dyn core::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any { self }
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

/// Store a file dropped/pasted from the browser and advertise it to the remote.
/// This enables Local→Remote file transfer via CLIPRDR.
#[wasm_bindgen]
pub fn set_pending_clipboard_file(name: String, bytes: &[u8], session: &crate::session::Session) {
    let size = bytes.len() as u64;
    PENDING_FILES.with(|cell| {
        let mut files = cell.borrow_mut();
        files.clear();
        files.push((name.clone(), bytes.to_vec()));
    });
    PENDING_CLIPBOARD_TEXT.with(|cell| *cell.borrow_mut() = None);
    PENDING_CLIPBOARD_IMAGE.with(|cell| *cell.borrow_mut() = None);

    let descriptor = FileDescriptor::new(name)
        .with_file_size(size)
        .with_attributes(ClipboardFileAttributes::NORMAL);
    session.send_file_copy(vec![descriptor]);
}

/// Called from JS when the user explicitly requests to download files from the remote clipboard.
/// Sends a SendInitiatePaste with the stored file-list format ID, which triggers on_remote_file_list.
#[wasm_bindgen]
pub fn trigger_remote_file_download(session: &crate::session::Session) {
    let fmt_id = PENDING_REMOTE_FILE_FORMAT.with(|cell| cell.borrow().clone());
    if let Some(id) = fmt_id {
        session.send_cliprdr_message(ClipboardMessage::SendInitiatePaste(id));
    }
}

// ── Helpers ──────────────────────────────────────────────

fn from_utf16le(data: &[u8]) -> String {
    let u16_iter = data
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]));
    let chars: Vec<u16> = u16_iter.take_while(|&c| c != 0).collect();
    String::from_utf16_lossy(&chars)
}

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
    if has_dibv5 { Some(ClipboardFormatId::CF_DIBV5) }
    else if has_dib { Some(ClipboardFormatId::CF_DIB) }
    else { None }
}

// ── CliprdrBackend impl ──────────────────────────────────

impl CliprdrBackend for WasmCliprdrBackend {
    fn temporary_directory(&self) -> &str { ".clipboard" }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        let mut caps = ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES;
        if self.enable_file {
            caps |= ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED;
        }
        caps
    }

    fn on_ready(&mut self) {
        crate::log("Clipboard: channel ready");
    }

    fn on_request_format_list(&mut self) {
        let has_text = PENDING_CLIPBOARD_TEXT.with(|cell| cell.borrow().is_some());
        let has_image = PENDING_CLIPBOARD_IMAGE.with(|cell| cell.borrow().is_some());
        let has_files = PENDING_FILES.with(|cell| !cell.borrow().is_empty());

        let mut formats = Vec::new();
        if has_text {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
        }
        if has_image {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIBV5));
        }
        // Files are advertised via initiate_file_copy (not format list), so skip here.
        let _ = has_files;
        let _ = self.tx.unbounded_send(InputEvent::Cliprdr(
            ClipboardMessage::SendInitiateCopy(formats),
        ));
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        _capabilities: ClipboardGeneralCapabilityFlags,
    ) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        if self.enable_text {
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
                return;
            }
        }

        if self.enable_file {
            // Check for files (FileGroupDescriptorW — identified by format name, not a fixed ID).
            let file_format = available_formats.iter().find(|f| {
                f.name().map(|n| n.value()) == Some(FORMAT_NAME_FILE_LIST)
            });

            if let Some(fmt) = file_format {
                // Defer: store format ID and let JS show a download notification.
                // We only call SendInitiatePaste when the user explicitly clicks "Download"
                // (via trigger_remote_file_download) so internal RDP copy-paste isn't disrupted.
                PENDING_REMOTE_FILE_FORMAT.with(|cell| *cell.borrow_mut() = Some(fmt.id()));
                notify_remote_clipboard_has_files();
            }
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        if !self.enable_text {
            let _ = self.tx.unbounded_send(InputEvent::Cliprdr(
                ClipboardMessage::SendFormatData(FormatDataResponse::new_error()),
            ));
            return;
        }
        if request.format == ClipboardFormatId::CF_UNICODETEXT {
            let text = PENDING_CLIPBOARD_TEXT
                .with(|cell| cell.borrow().clone())
                .unwrap_or_default();
            let response = FormatDataResponse::new_unicode_string(&text).into_owned();
            let _ = self.tx.unbounded_send(InputEvent::Cliprdr(
                ClipboardMessage::SendFormatData(response),
            ));
        } else if request.format == ClipboardFormatId::CF_DIBV5 {
            let png_data = PENDING_CLIPBOARD_IMAGE.with(|cell| cell.borrow().clone());
            let response = match png_data {
                Some(png) => match png_to_cf_dibv5(&png) {
                    Ok(dib_data) => FormatDataResponse::new_data(dib_data).into_owned(),
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

    /// Called when the remote advertises files via FileGroupDescriptorW.
    /// Issue a SIZE request for each file to start the download sequence.
    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        crate::log(&format!("Clipboard: {} file(s) available from remote", files.len()));
        for (i, file) in files.iter().enumerate() {
            self.request_file_size(i as i32, file.name.clone(), clip_data_id);
        }
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        let stream_id = response.stream_id();

        if response.is_error() {
            crate::log_error(&format!("Clipboard: FileContentsResponse error for stream {stream_id}"));
            self.file_transfers.remove(&stream_id);
            return;
        }

        match self.file_transfers.remove(&stream_id) {
            Some(FileTransferState::AwaitingSize { file_index, name, data_id }) => {
                match response.data_as_size() {
                    Ok(total) => {
                        crate::log(&format!("Clipboard: file '{name}' size = {total} bytes"));
                        // Kick off data download starting at offset 0.
                        self.request_file_range(file_index, name, total, Vec::new(), data_id);
                    }
                    Err(e) => {
                        crate::log_error(&format!("Clipboard: bad SIZE response for '{name}': {e}"));
                    }
                }
            }
            Some(FileTransferState::AwaitingData { file_index, name, total, mut data, data_id }) => {
                data.extend_from_slice(response.data());
                if (data.len() as u64) >= total {
                    crate::log(&format!("Clipboard: downloading '{name}' ({total} bytes)"));
                    download_file(&name, &data);
                } else {
                    // More chunks needed.
                    self.request_file_range(file_index, name, total, data, data_id);
                }
            }
            None => {
                crate::log_error(&format!("Clipboard: unexpected FileContentsResponse for stream {stream_id}"));
            }
        }
    }

    /// Called when the remote requests file contents from our local clipboard.
    /// Responds with SIZE or RANGE data from the PENDING_FILES cache.
    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        if !self.enable_file {
            let _ = self.tx.unbounded_send(InputEvent::Cliprdr(
                ClipboardMessage::SendFileContentsResponse(
                    FileContentsResponse::new_error(request.stream_id).into_owned(),
                ),
            ));
            return;
        }
        let stream_id = request.stream_id;
        let file_index = request.index as usize;

        let result = PENDING_FILES.with(|cell| {
            let files = cell.borrow();
            if let Some((_, data)) = files.get(file_index) {
                if request.flags.contains(FileContentsFlags::SIZE) {
                    Ok(FileContentsResponse::new_size_response(stream_id, data.len() as u64)
                        .into_owned())
                } else {
                    // RANGE request.
                    let start = request.position as usize;
                    let end = (start + request.requested_size as usize).min(data.len());
                    Ok(FileContentsResponse::new_data_response(
                        stream_id,
                        data[start..end].to_vec(),
                    ).into_owned())
                }
            } else {
                Err(())
            }
        });

        let response = result.unwrap_or_else(|_| {
            crate::log_error(&format!("Clipboard: no file at index {file_index} for stream {stream_id}"));
            FileContentsResponse::new_error(stream_id).into_owned()
        });

        let _ = self.tx.unbounded_send(InputEvent::Cliprdr(
            ClipboardMessage::SendFileContentsResponse(response),
        ));
    }

    fn on_lock(&mut self, _data_id: LockDataId) {}
    fn on_unlock(&mut self, _data_id: LockDataId) {}

    fn now_ms(&self) -> u64 {
        js_sys::Date::now() as u64
    }
}
