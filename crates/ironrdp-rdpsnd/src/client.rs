use std::borrow::Cow;

use ironrdp_core::{Decode as _, ReadCursor, impl_as_any};
use ironrdp_pdu::gcc::ChannelName;
use ironrdp_pdu::{PduResult, decode_err, pdu_other_err};
use ironrdp_dvc::{DvcClientProcessor, DvcMessage, DvcProcessor};
use ironrdp_svc::{CompressionCondition, SvcClientProcessor, SvcMessage, SvcProcessor};
use tracing::{debug, error, info, warn};

use crate::pdu::{self, AudioFormat, PitchPdu, ServerAudioFormatPdu, TrainingPdu, VolumePdu, WaveInfoPdu};

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
    /// One-shot info log on the first wave (diagnostic for "no audio" reports).
    first_wave_logged: bool,
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
            first_wave_logged: false,
        }
    }

    fn log_first_wave(&mut self, format: &AudioFormat) {
        if !self.first_wave_logged {
            self.first_wave_logged = true;
            info!(
                "RDPSND: first wave received (format {:?}, {} Hz, {} ch, {} bit)",
                format.format, format.n_samples_per_sec, format.n_channels, format.bits_per_sample
            );
        }
    }

    pub fn version(&self) -> PduResult<pdu::Version> {
        let server_format = self
            .server_format
            .as_ref()
            .ok_or_else(|| pdu_other_err!("invalid state - no version"))?;

        Ok(server_format.version)
    }

    pub fn client_formats(&mut self) -> PduResult<Vec<pdu::ClientAudioOutputPdu>> {
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

        // Info-level: one-shot per connection, and the single most important
        // diagnostic for "no audio" reports (which formats the server offered
        // vs. what we accepted).
        info!(
            "RDPSND format negotiation: server offered {:?}, matched {} (client supports {:?})",
            server_formats
                .iter()
                .map(|f| (f.format, f.n_samples_per_sec, f.n_channels, f.bits_per_sample))
                .collect::<Vec<_>>(),
            negotiated.len(),
            supported_tags,
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
        Ok(vec![pdu::ClientAudioOutputPdu::AudioFormat(pdu)])
    }

    pub fn quality_mode(&mut self) -> PduResult<Vec<pdu::ClientAudioOutputPdu>> {
        let pdu = pdu::QualityModePdu {
            quality_mode: pdu::QualityMode::High,
        };
        Ok(vec![pdu::ClientAudioOutputPdu::QualityMode(pdu)])
    }

    pub fn training_confirm(&mut self, pdu: &TrainingPdu) -> PduResult<Vec<pdu::ClientAudioOutputPdu>> {
        // Echo wTimeStamp and wPackSize verbatim — servers validate both and
        // reject the confirm (killing the audio channel) on any mismatch.
        let pdu = pdu::TrainingConfirmPdu {
            timestamp: pdu.timestamp,
            pack_size: pdu.pack_size,
        };
        Ok(vec![pdu::ClientAudioOutputPdu::TrainingConfirm(pdu)])
    }

    pub fn wave_confirm(&mut self, timestamp: u16, block_no: u8) -> PduResult<Vec<pdu::ClientAudioOutputPdu>> {
        let pdu = pdu::WaveConfirmPdu { timestamp, block_no };
        Ok(vec![pdu::ClientAudioOutputPdu::WaveConfirm(pdu)])
    }

    /// Look up the AudioFormat for a given format_no from the negotiated list.
    fn lookup_format(&self, format_no: u16) -> PduResult<&AudioFormat> {
        self.negotiated_formats
            .get(usize::from(format_no))
            .ok_or_else(|| pdu_other_err!("invalid format_no in wave PDU"))
    }

    /// Handle the second part of a two-phase SNDC_WAVE message.
    /// The payload is raw Wave data: bPad(4) + audio samples.
    fn process_wave_data(&mut self, wave_info: WaveInfoPdu, payload: &[u8]) -> PduResult<Vec<pdu::ClientAudioOutputPdu>> {
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
        self.log_first_wave(&format);
        let ts = u32::from(wave_info.timestamp);
        self.handler.wave(&format, ts, data.into());
        self.wave_confirm(wave_info.timestamp, wave_info.block_no)
    }

    /// Process a PDU in the Ready state.
    fn process_ready_pdu(&mut self, pdu: pdu::ServerAudioOutputPdu<'_>) -> PduResult<Vec<pdu::ClientAudioOutputPdu>> {
        match pdu {
            pdu::ServerAudioOutputPdu::Wave2(pdu) => {
                let format = self.lookup_format(pdu.format_no)?.clone();
                self.log_first_wave(&format);
                let ts = pdu.audio_timestamp;
                self.handler.wave(&format, ts, pdu.data);
                return self.wave_confirm(pdu.timestamp, pdu.block_no);
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
            pdu::ServerAudioOutputPdu::Training(pdu) => return self.training_confirm(&pdu),
            pdu::ServerAudioOutputPdu::AudioFormat(af) => {
                self.handler.close();
                self.server_format = Some(af);
                self.state = RdpsndState::WaitingForTraining;
                let mut msgs = self.client_formats()?;
                if self.version()? >= pdu::Version::V6 {
                    msgs.append(&mut self.quality_mode()?);
                }
                return Ok(msgs);
            }
            _ => {
                debug!("Ignoring unhandled RDPSND PDU in Ready state");
            }
        }
        Ok(vec![])
    }

    /// Transport-agnostic MS-RDPEA driver: one complete server PDU in,
    /// client response PDUs out. Shared by the static-channel (`SvcProcessor`)
    /// and dynamic-channel ([`RdpsndDvcClient`]) transports.
    pub fn handle_pdu(&mut self, payload: &[u8]) -> PduResult<Vec<pdu::ClientAudioOutputPdu>> {
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
        match self.state {
            RdpsndState::Start => {
                let pdu::ServerAudioOutputPdu::AudioFormat(af) = pdu else {
                    error!("Invalid pdu");
                    self.state = RdpsndState::Stop;
                    return Ok(vec![]);
                };
                self.server_format = Some(af);
                self.state = RdpsndState::WaitingForTraining;
                let mut msgs = self.client_formats()?;
                if self.version()? >= pdu::Version::V6 {
                    msgs.append(&mut self.quality_mode()?);
                }
                Ok(msgs)
            }
            RdpsndState::WaitingForTraining => {
                match pdu {
                    pdu::ServerAudioOutputPdu::Training(pdu) => {
                        info!("RDPSND: training received, confirming; negotiation complete");
                        self.state = RdpsndState::Ready;
                        self.training_confirm(&pdu)
                    }
                    other => {
                        // Windows RDP may skip training (MS-RDPEA says SHOULD, not MUST).
                        // Transition to Ready and process the PDU normally.
                        warn!("RDPSND: no Training PDU received, transitioning to Ready");
                        self.state = RdpsndState::Ready;
                        self.process_ready_pdu(other)
                    }
                }
            }
            RdpsndState::Ready => self.process_ready_pdu(pdu),
            state => {
                error!(?state, "Invalid state");
                Ok(vec![])
            }
        }
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
        Ok(self.handle_pdu(payload)?.into_iter().map(SvcMessage::from).collect())
    }
}

impl Drop for Rdpsnd {
    fn drop(&mut self) {
        self.handler.close();
    }
}

impl SvcClientProcessor for Rdpsnd {}

impl ironrdp_dvc::DvcEncode for pdu::ClientAudioOutputPdu {}

/// MS-RDPEA over the `AUDIO_PLAYBACK_DVC` dynamic virtual channel.
///
/// Modern servers move audio here as soon as the client advertises DRDYNVC:
/// GNOME Remote Desktop implements *only* this transport, and Windows/xrdp
/// prefer it over the static `rdpsnd` channel when dynamic channels are
/// available. Same PDUs and state machine as the static [`Rdpsnd`] — only the
/// transport differs (the DVC layer handles fragmentation/reassembly).
#[derive(Debug)]
pub struct RdpsndDvcClient {
    inner: Rdpsnd,
}

impl RdpsndDvcClient {
    pub const CHANNEL_NAME: &'static str = "AUDIO_PLAYBACK_DVC";

    pub fn new(handler: Box<dyn RdpsndClientHandler>) -> Self {
        Self {
            inner: Rdpsnd::new(handler),
        }
    }
}

impl_as_any!(RdpsndDvcClient);

impl DvcProcessor for RdpsndDvcClient {
    fn channel_name(&self) -> &str {
        Self::CHANNEL_NAME
    }

    // The server speaks first (Server Audio Formats PDU), exactly as on the
    // static channel — nothing to send at channel creation.
    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        Ok(self
            .inner
            .handle_pdu(payload)?
            .into_iter()
            .map(|pdu| Box::new(pdu) as DvcMessage)
            .collect())
    }
}

impl DvcClientProcessor for RdpsndDvcClient {}
