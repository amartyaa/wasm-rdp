use std::borrow::Cow;

use ironrdp::rdpsnd::pdu::{AudioFormat, WaveFormat, VolumePdu, PitchPdu};
use ironrdp::rdpsnd::client::RdpsndClientHandler;

use crate::log;

/// WASM-compatible RDPSND handler that forwards PCM audio data to JavaScript.
///
/// Stores only Send-safe types (Vec<AudioFormat>). All JS interop is done
/// via temporary js_sys calls in wave(), never stored on the struct.
#[derive(Debug)]
pub struct WasmRdpsndHandler {
    formats: Vec<AudioFormat>,
}

impl WasmRdpsndHandler {
    pub fn new() -> Self {
        let formats = vec![
            // PCM 16-bit, stereo, 44100 Hz
            AudioFormat {
                format: WaveFormat::PCM,
                n_channels: 2,
                n_samples_per_sec: 44100,
                n_avg_bytes_per_sec: 44100 * 2 * 2, // sampleRate * channels * bytesPerSample
                n_block_align: 4,                    // channels * bytesPerSample
                bits_per_sample: 16,
                data: None,
            },
            // PCM 16-bit, stereo, 48000 Hz
            AudioFormat {
                format: WaveFormat::PCM,
                n_channels: 2,
                n_samples_per_sec: 48000,
                n_avg_bytes_per_sec: 48000 * 2 * 2,
                n_block_align: 4,
                bits_per_sample: 16,
                data: None,
            },
        ];
        Self { formats }
    }
}

impl RdpsndClientHandler for WasmRdpsndHandler {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn wave(&mut self, format_no: usize, _ts: u32, data: Cow<'_, [u8]>) {
        let (channels, sample_rate, bits_per_sample) = if let Some(fmt) = self.formats.get(format_no) {
            (fmt.n_channels, fmt.n_samples_per_sec, fmt.bits_per_sample)
        } else {
            // Fallback: assume first format
            (2, 44100, 16)
        };

        crate::notify_audio_data(channels, sample_rate, bits_per_sample, &data);
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
