use ironrdp::cliprdr::backend::{ClipboardMessage, CliprdrBackend};
use ironrdp::cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse,
    FormatDataRequest, FormatDataResponse, LockDataId, OwnedFormatDataResponse,
};
use core::any::Any;

#[derive(Debug)]
pub struct WasmCliprdrBackend {
    // We could store channels to communicate with JS here
}

impl WasmCliprdrBackend {
    pub fn new() -> Self {
        Self {}
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

impl CliprdrBackend for WasmCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        ".clipboard"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {
        crate::log("CliprdrBackend: Ready");
    }

    fn on_request_format_list(&mut self) {
        crate::log("CliprdrBackend: System requested format list (local -> remote)");
        // TODO: We could query JS clipboard and advertise CF_UNICODETEXT
    }

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        crate::log("CliprdrBackend: Remote copied data");
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        crate::log(&format!("CliprdrBackend: Remote requested format {:?}", request.format));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        crate::log("CliprdrBackend: Received format data response");
        if response.is_error() {
            crate::log_error("CliprdrBackend: Received error from server");
            return;
        }
        
        // TODO: Parse response.data and send to JS navigator.clipboard.writeText
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}
    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}
    fn on_lock(&mut self, _data_id: LockDataId) {}
    fn on_unlock(&mut self, _data_id: LockDataId) {}
}
