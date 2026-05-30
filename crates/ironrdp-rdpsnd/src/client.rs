use std::borrow::Cow;

use ironrdp_core::{Decode as _, EncodeResult, ReadCursor, cast_length, impl_as_any};
use ironrdp_pdu::gcc::ChannelName;
use ironrdp_pdu::{PduResult, decode_err, encode_err, pdu_other_err};
use ironrdp_svc::{CompressionCondition, SvcClientProcessor, SvcMessage, SvcProcessor};
use tracing::{debug, error, warn};

use crate::pdu::{self, AudioFormat, PitchPdu, ServerAudioFormatPdu, TrainingPdu, VolumePdu, WaveInfoPdu};
use crate::server::RdpsndSvcMessages;

pub trait RdpsndClientHandler: Send + core::fmt::Debug {
    fn get_flags(&self) -> pdu::AudioFormatFlags {
        pdu::AudioFormatFlags::empty()
    }

    /// Returns the WaveFormat tags this handler supports (e.g., PCM).
    /// Used during negotiation: any server format whose tag matches will be accepted.
    fn supported_formats(&self) -> &[pdu::WaveFormat];

    /// Called when audio data is received. `format` is the negotiated AudioFormat
    /// for this wave (looked up from the client's sent format list).
    fn wave(&mut self, format: &AudioFormat, ts: u32, data: Cow<'_, [u8]>);

    fn set_volume(&mut self, volume: VolumePdu);

    fn set_pitch(&mut self, pitch: PitchPdu);

    fn close(&mut self);
}

#[derive(Debug)]
pub struct NoopRdpsndBackend;

impl RdpsndClientHandler for NoopRdpsndBackend {
    fn supported_formats(&self) -> &[pdu::WaveFormat] {
        &[]
    }

    fn wave(&mut self, _format: &AudioFormat, _ts: u32, _data: Cow<'_, [u8]>) {}

    fn set_volume(&mut self, _volume: VolumePdu) {}

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {}
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum RdpsndState {
    Start,
    WaitingForTraining,
    Ready,
    Stop,
}

/// Required for rdpdr to work: [\[MS-RDPEFS\] Appendix A<1>]
///
/// [\[MS-RDPEFS\] Appendix A<1>]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpefs/fd28bfd9-dae2-4a78-abe1-b4efa208b7aa#Appendix_A_1
#[derive(Debug)]
pub struct Rdpsnd {
    handler: Box<dyn RdpsndClientHandler>,
    state: RdpsndState,
    server_format: Option<ServerAudioFormatPdu>,
    /// The format list actually sent to the server (and used for format_no lookups).
    negotiated_formats: Vec<AudioFormat>,
    /// Pending WaveInfo from a two-phase SNDC_WAVE message (MS-RDPEA 2.2.3.5/2.2.3.6).
    /// xRDP sends WaveInfo and Wave data as two separate SVC messages.
    pending_wave_info: Option<WaveInfoPdu>,
}

impl Rdpsnd {
    pub const NAME: ChannelName = ChannelName::from_static(b"rdpsnd\0\0");

    pub fn new(handler: Box<dyn RdpsndClientHandler>) -> Self {
        Self {
            handler,
            state: RdpsndState::Start,
            server_format: None,
            negotiated_formats: Vec::new(),
            pending_wave_info: None,
        }
    }

    pub fn version(&self) -> PduResult<pdu::Version> {
        let server_format = self
            .server_format
            .as_ref()
            .ok_or_else(|| pdu_other_err!("invalid state - no version"))?;

        Ok(server_format.version)
    }

    pub fn client_formats(&mut self) -> PduResult<RdpsndSvcMessages> {
        let server_formats = &self
            .server_format
            .as_ref()
            .ok_or_else(|| pdu_other_err!("invalid state - no server format"))?
            .formats;

        let supported_tags = self.handler.supported_formats();

        // Accept any server format whose WaveFormat tag matches one we support.
        // This is much more flexible than strict AudioFormat equality — we accept
        // any sample rate, channel count, or bit depth for a supported codec.
        let negotiated: Vec<AudioFormat> = server_formats
            .iter()
            .filter(|sf| supported_tags.iter().any(|tag| *tag == sf.format))
            .cloned()
            .collect();

        debug!(
            "RDPSND format negotiation: server offered {} formats, {} matched",
            server_formats.len(),
            negotiated.len()
        );

        self.negotiated_formats = negotiated.clone();

        let pdu = pdu::ClientAudioFormatPdu {
            version: self.version()?,
            flags: self.handler.get_flags() | pdu::AudioFormatFlags::ALIVE,
            formats: negotiated,
            volume_left: 0xFFFF,
            volume_right: 0xFFFF,
            pitch: 0x00010000,
            dgram_port: 0,
        };
        Ok(RdpsndSvcMessages::new(vec![
            pdu::ClientAudioOutputPdu::AudioFormat(pdu).into(),
        ]))
    }

    pub fn quality_mode(&mut self) -> PduResult<RdpsndSvcMessages> {
        let pdu = pdu::QualityModePdu {
            quality_mode: pdu::QualityMode::High,
        };
        Ok(RdpsndSvcMessages::new(vec![
            pdu::ClientAudioOutputPdu::QualityMode(pdu).into(),
        ]))
    }

    pub fn training_confirm(&mut self, pdu: &TrainingPdu) -> PduResult<RdpsndSvcMessages> {
        let pack_size: EncodeResult<_> = cast_length!("wPackSize", pdu.data.len());
        let pack_size = pack_size.map_err(|e| encode_err!(e))?;
        let pdu = pdu::TrainingConfirmPdu {
            timestamp: pdu.timestamp,
            pack_size,
        };
        Ok(RdpsndSvcMessages::new(vec![
            pdu::ClientAudioOutputPdu::TrainingConfirm(pdu).into(),
        ]))
    }

    pub fn wave_confirm(&mut self, timestamp: u16, block_no: u8) -> PduResult<RdpsndSvcMessages> {
        let pdu = pdu::WaveConfirmPdu { timestamp, block_no };
        Ok(RdpsndSvcMessages::new(vec![
            pdu::ClientAudioOutputPdu::WaveConfirm(pdu).into(),
        ]))
    }

    /// Look up the AudioFormat for a given format_no from the negotiated list.
    fn lookup_format(&self, format_no: u16) -> PduResult<&AudioFormat> {
        self.negotiated_formats
            .get(usize::from(format_no))
            .ok_or_else(|| pdu_other_err!("invalid format_no in wave PDU"))
    }

    /// Handle the second part of a two-phase SNDC_WAVE message.
    /// The payload is raw Wave data: bPad(4) + audio samples.
    fn process_wave_data(&mut self, wave_info: WaveInfoPdu, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        let mut src = ReadCursor::new(payload);

        // Wave PDU: 4 bytes padding + audio data
        let padding_len = 4.min(payload.len());
        let _ = src.read_slice(padding_len); // skip padding
        let wave_data = src.read_remaining();

        // Combine: first 4 bytes from WaveInfo.data + rest from Wave PDU
        let mut data = Vec::with_capacity(4 + wave_data.len());
        data.extend_from_slice(&wave_info.data);
        data.extend_from_slice(wave_data);

        let format = self.lookup_format(wave_info.format_no)?.clone();
        let ts = u32::from(wave_info.timestamp);
        self.handler.wave(&format, ts, data.into());
        Ok(self.wave_confirm(wave_info.timestamp, wave_info.block_no)?.into())
    }

    /// Process a PDU in the Ready state.
    fn process_ready_pdu(&mut self, pdu: pdu::ServerAudioOutputPdu<'_>) -> PduResult<Vec<SvcMessage>> {
        match pdu {
            pdu::ServerAudioOutputPdu::Wave2(pdu) => {
                let format = self.lookup_format(pdu.format_no)?.clone();
                let ts = pdu.audio_timestamp;
                self.handler.wave(&format, ts, pdu.data);
                return Ok(self.wave_confirm(pdu.timestamp, pdu.block_no)?.into());
            }
            pdu::ServerAudioOutputPdu::Volume(pdu) => {
                self.handler.set_volume(pdu);
            }
            pdu::ServerAudioOutputPdu::Pitch(pdu) => {
                self.handler.set_pitch(pdu);
            }
            pdu::ServerAudioOutputPdu::Close => {
                self.handler.close();
            }
            pdu::ServerAudioOutputPdu::Training(pdu) => return Ok(self.training_confirm(&pdu)?.into()),
            pdu::ServerAudioOutputPdu::AudioFormat(af) => {
                self.handler.close();
                self.server_format = Some(af);
                self.state = RdpsndState::WaitingForTraining;
                let mut msgs: Vec<SvcMessage> = self.client_formats()?.into();
                if self.version()? >= pdu::Version::V6 {
                    let mut m = self.quality_mode()?.into();
                    msgs.append(&mut m);
                }
                return Ok(msgs);
            }
            _ => {
                debug!("Ignoring unhandled RDPSND PDU in Ready state");
            }
        }
        Ok(vec![])
    }
}

impl_as_any!(Rdpsnd);

impl SvcProcessor for Rdpsnd {
    fn channel_name(&self) -> ChannelName {
        Self::NAME
    }

    fn compression_condition(&self) -> CompressionCondition {
        CompressionCondition::Never
    }

    fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        // Phase 2 of two-phase SNDC_WAVE: if we have a pending WaveInfo,
        // the incoming payload is the raw Wave data (no SNDPROLOG header).
        if let Some(wave_info) = self.pending_wave_info.take() {
            return self.process_wave_data(wave_info, payload);
        }

        // Check for SNDC_WAVE (msgType 0x02) which uses the two-phase protocol.
        // We intercept before ServerAudioOutputPdu::decode because that decoder
        // expects both WaveInfo + SndWave in a single buffer, but xRDP sends
        // them as two separate SVC messages.
        const SNDC_WAVE: u8 = 0x02;
        if payload.len() >= 4 && payload[0] == SNDC_WAVE {
            let mut src = ReadCursor::new(payload);
            let _msg_type = src.read_u8(); // 0x02
            let _ = src.read_u8(); // padding
            let _body_size = src.read_u16();

            let wave_info = WaveInfoPdu::decode(&mut src).map_err(|e| decode_err!(e))?;

            // Check if the Wave data is also present in this same payload
            // (some servers send both parts together).
            if src.remaining().len() >= 4 {
                return self.process_wave_data(wave_info, src.remaining());
            }

            // Two-phase: save WaveInfo, wait for Wave data in next process() call
            self.pending_wave_info = Some(wave_info);
            return Ok(vec![]);
        }

        // Normal PDU decode path for all other message types
        let pdu = pdu::ServerAudioOutputPdu::decode(&mut ReadCursor::new(payload)).map_err(|e| decode_err!(e))?;

        debug!(?pdu, ?self.state);
        let msg = match self.state {
            RdpsndState::Start => {
                let pdu::ServerAudioOutputPdu::AudioFormat(af) = pdu else {
                    error!("Invalid pdu");
                    self.state = RdpsndState::Stop;
                    return Ok(vec![]);
                };
                self.server_format = Some(af);
                self.state = RdpsndState::WaitingForTraining;
                let mut msgs: Vec<SvcMessage> = self.client_formats()?.into();
                if self.version()? >= pdu::Version::V6 {
                    let mut m = self.quality_mode()?.into();
                    msgs.append(&mut m);
                }
                msgs
            }
            RdpsndState::WaitingForTraining => {
                match pdu {
                    pdu::ServerAudioOutputPdu::Training(pdu) => {
                        self.state = RdpsndState::Ready;
                        self.training_confirm(&pdu)?.into()
                    }
                    other => {
                        // Windows RDP may skip training (MS-RDPEA says SHOULD, not MUST).
                        // Transition to Ready and process the PDU normally.
                        warn!("RDPSND: no Training PDU received, transitioning to Ready");
                        self.state = RdpsndState::Ready;
                        self.process_ready_pdu(other)?
                    }
                }
            }
            RdpsndState::Ready => {
                self.process_ready_pdu(pdu)?
            }
            state => {
                error!(?state, "Invalid state");
                vec![]
            }
        };

        Ok(msg)
    }
}

impl Drop for Rdpsnd {
    fn drop(&mut self) {
        self.handler.close();
    }
}

impl SvcClientProcessor for Rdpsnd {}
