use std::borrow::Cow;

use ironrdp::rdpsnd::pdu::{AudioFormat, WaveFormat, VolumePdu, PitchPdu};
use ironrdp::rdpsnd::client::RdpsndClientHandler;

use crate::log;

/// WASM-compatible RDPSND handler that forwards PCM audio data to JavaScript.
///
/// Accepts any PCM format the server offers. The actual sample rate, channel
/// count, and bit depth come from the negotiated AudioFormat passed to wave().
#[derive(Debug)]
pub struct WasmRdpsndHandler {
    supported_tags: Vec<WaveFormat>,
}

impl WasmRdpsndHandler {
    pub fn new() -> Self {
        Self {
            // We support raw PCM — the browser's Web Audio API handles resampling.
            supported_tags: vec![WaveFormat::PCM],
        }
    }
}

impl RdpsndClientHandler for WasmRdpsndHandler {
    fn supported_formats(&self) -> &[WaveFormat] {
        &self.supported_tags
    }

    fn wave(&mut self, format: &AudioFormat, _ts: u32, data: Cow<'_, [u8]>) {
        crate::notify_audio_data(format.n_channels, format.n_samples_per_sec, format.bits_per_sample, &data);
    }

    fn set_volume(&mut self, _volume: VolumePdu) {
        // Volume is controlled browser-side via GainNode
    }

    fn set_pitch(&mut self, _pitch: PitchPdu) {
        // Pitch adjustment not needed for web playback
    }

    fn close(&mut self) {
        log("RDPSND: stream closed");
    }
}
