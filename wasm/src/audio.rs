use std::borrow::Cow;

use ironrdp::rdpsnd::pdu::{AudioFormat, WaveFormat, VolumePdu, PitchPdu};
use ironrdp::rdpsnd::client::RdpsndClientHandler;

use crate::log;

/// WASM-compatible RDPSND handler that forwards audio data to JavaScript.
///
/// PCM is always supported (browser resamples it). Opus and AAC are advertised
/// only when the browser reported WebCodecs support for them at connect time;
/// their encoded packets are forwarded verbatim to JS for WebCodecs decoding.
#[derive(Debug)]
pub struct WasmRdpsndHandler {
    supported_tags: Vec<WaveFormat>,
}

impl WasmRdpsndHandler {
    pub fn new(enable_opus: bool, enable_aac: bool) -> Self {
        // Advertise compressed codecs first (a server that supports them prefers
        // them), then PCM as the universal fallback. Standard Windows and xrdp
        // only speak PCM over RDPSND — Opus/AAC are non-standard formats our
        // vendored crate adds — so without PCM there is no common format and the
        // server sends nothing (silent). PCM is deinterleaved in JS and handed to
        // the AudioWorklet (off the main thread), so the per-packet cost that once
        // motivated dropping it no longer applies.
        let mut supported_tags = Vec::new();
        if enable_aac {
            supported_tags.push(WaveFormat::AAC_MS);
        }
        if enable_opus {
            supported_tags.push(WaveFormat::OPUS);
        }
        supported_tags.push(WaveFormat::PCM);
        Self { supported_tags }
    }
}

impl RdpsndClientHandler for WasmRdpsndHandler {
    fn supported_formats(&self) -> &[WaveFormat] {
        &self.supported_tags
    }

    fn wave(&mut self, format: &AudioFormat, _ts: u32, data: Cow<'_, [u8]>) {
        // Forward the raw wave payload plus the codec tag and any codec extra-data
        // (e.g. AAC AudioSpecificConfig). JS decodes encoded codecs via WebCodecs
        // and feeds the result into the same playback ring buffer as PCM.
        let codec = format.format;
        if codec != WaveFormat::PCM && codec != WaveFormat::OPUS && codec != WaveFormat::AAC_MS {
            crate::log_error("RDPSND: server sent an unadvertised wave format, dropping");
            return;
        }
        let codec_tag: u16 = if codec == WaveFormat::OPUS {
            0x704F
        } else if codec == WaveFormat::AAC_MS {
            0xA106
        } else {
            0x0001 // PCM
        };
        let extradata = format.data.as_deref().unwrap_or(&[]);
        crate::notify_audio_data(
            codec_tag,
            format.n_channels,
            format.n_samples_per_sec,
            format.bits_per_sample,
            &data,
            extradata,
        );
    }

    fn set_volume(&mut self, volume: VolumePdu) {
        // Forward to the browser GainNode (per-channel 0..0xFFFF).
        crate::notify_audio_volume(volume.volume_left, volume.volume_right);
    }

    fn set_pitch(&mut self, _pitch: PitchPdu) {
        // Pitch adjustment not needed for web playback
    }

    fn close(&mut self) {
        log("RDPSND: stream closed");
    }
}
