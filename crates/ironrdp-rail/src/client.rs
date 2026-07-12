//! RAIL static virtual channel client — [MS-RDPERP] 1.3.2.1 connection sequence.
//!
//! Reactive `SvcProcessor` (same shape as the vendored `ironrdp-rdpsnd`
//! `Rdpsnd`): on the server Handshake it replies with the full client init
//! burst (Handshake → Client Information → SysParams → Client Execute), exactly
//! the order FreeRDP's client emits (`channels/rail/client/rail_main.c`).

use ironrdp_core::{ReadCursor, impl_as_any};
use ironrdp_pdu::{PduResult, decode_err};
use ironrdp_pdu::gcc::{ChannelName, ChannelOptions};
use ironrdp_svc::{ChannelFlags, CompressionCondition, SvcClientProcessor, SvcMessage, SvcProcessor};
use tracing::{info, warn};

use crate::pdu::{ClientRailOrder, ExecResult, ServerRailOrder};

/// Sink for the one server→client RAIL event the UI reacts to. Kept minimal
/// (only exec result); window metadata arrives via graphics Orders, not here.
pub trait RailClientHandler: Send + core::fmt::Debug {
    /// Server Execute Result — 0 (`RAIL_EXEC_S_OK`) is success.
    fn on_exec_result(&mut self, result: ExecResult);
}

/// RAIL SVC processor. Launches `program` (with `working_dir`/`arguments`) once
/// the server completes the handshake.
#[derive(Debug)]
pub struct Rail {
    handler: Box<dyn RailClientHandler>,
    program: String,
    working_dir: String,
    arguments: String,
    width: u16,
    height: u16,
    handshake_done: bool,
}

impl Rail {
    pub const NAME: ChannelName = ChannelName::from_static(b"rail\0\0\0\0");

    pub fn new(
        handler: Box<dyn RailClientHandler>,
        program: String,
        working_dir: String,
        arguments: String,
        width: u16,
        height: u16,
    ) -> Self {
        Self {
            handler,
            program,
            working_dir,
            arguments,
            width,
            height,
            handshake_done: false,
        }
    }

    /// Client init burst sent in response to the server Handshake, in FreeRDP's
    /// order: Handshake, Client Information, SysParam(HighContrast off),
    /// SysParam(WorkArea = full desktop), Client Execute.
    fn client_init(&self, build_number: u32) -> Vec<ClientRailOrder> {
        vec![
            ClientRailOrder::Handshake { build_number },
            // flags = 0: no ALLOWLOCALMOVESIZE → server owns move/size and draws
            // window chrome; we forward mouse to it (v1 defers local move/size).
            ClientRailOrder::ClientStatus { flags: 0 },
            ClientRailOrder::SysParamHighContrastOff,
            ClientRailOrder::SysParamWorkArea {
                width: self.width,
                height: self.height,
            },
            ClientRailOrder::Exec {
                exe_or_file: self.program.clone(),
                working_dir: self.working_dir.clone(),
                arguments: self.arguments.clone(),
            },
        ]
    }

    /// Wrap an order for sending. Every client→server rail PDU carries
    /// CHANNEL_FLAG_SHOW_PROTOCOL, matching mstsc/FreeRDP (the rail channel is
    /// declared with CHANNEL_OPTION_SHOW_PROTOCOL, and rdpshell expects the
    /// channel header to be visible — FreeRDP freerdp_channel_send sets the
    /// per-PDU flag whenever the option is declared).
    fn msg(order: ClientRailOrder) -> SvcMessage {
        SvcMessage::from(order).with_flags(ChannelFlags::SHOW_PROTOCOL)
    }

    /// Focus a window ([MS-RDPERP] 2.2.2.6.2 Client Activate PDU).
    pub fn activate(&self, window_id: u32, enabled: bool) -> Vec<SvcMessage> {
        vec![Self::msg(ClientRailOrder::Activate { window_id, enabled })]
    }

    /// Send a system command — minimize/restore/close ([MS-RDPERP] 2.2.2.6.1).
    pub fn sys_command(&self, window_id: u32, command: u16) -> Vec<SvcMessage> {
        vec![Self::msg(ClientRailOrder::SysCommand { window_id, command })]
    }

    fn handle_pdu(&mut self, payload: &[u8]) -> PduResult<Vec<ClientRailOrder>> {
        let mut src = ReadCursor::new(payload);
        match ServerRailOrder::decode(&mut src).map_err(|e| decode_err!(e))? {
            ServerRailOrder::Handshake => {
                if self.handshake_done {
                    return Ok(Vec::new());
                }
                self.handshake_done = true;
                info!("RAIL: server handshake received, launching '{}'", self.program);
                Ok(self.client_init(RAIL_CLIENT_BUILD))
            }
            ServerRailOrder::ExecResult(result) => {
                if result.exec_result == 0 {
                    info!("RAIL: exec succeeded");
                } else {
                    warn!(
                        "RAIL: exec failed (execResult={}, rawResult=0x{:08x})",
                        result.exec_result, result.raw_result
                    );
                }
                self.handler.on_exec_result(result);
                Ok(Vec::new())
            }
            ServerRailOrder::Other(order_type) => {
                // info-level on purpose: seeing (or not seeing) the server's
                // SysParam/Taskbar orders in the console is the negotiation
                // health signal while RAIL is being stabilized.
                info!("RAIL: ignoring server order 0x{order_type:04x}");
                Ok(Vec::new())
            }
        }
    }
}

// FreeRDP echoes the client's FreeRDP_ClientBuild; a fixed Windows build number
// is equally accepted (servers don't validate it). 7601 = Win7 SP1 RTM.
const RAIL_CLIENT_BUILD: u32 = 7601;

impl_as_any!(Rail);

impl SvcProcessor for Rail {
    fn channel_name(&self) -> ChannelName {
        Self::NAME
    }

    fn compression_condition(&self) -> CompressionCondition {
        CompressionCondition::Never
    }

    fn channel_options(&self) -> ChannelOptions {
        // Match mstsc's rail CHANNEL_DEF options exactly (0xC0A00000) —
        // FreeRDP rail_main.c: INITIALIZED | ENCRYPT_RDP | COMPRESS_RDP |
        // SHOW_PROTOCOL. rdpshell is only ever exercised against clients that
        // declare these; an empty options field risks the host discarding our
        // client→server rail PDUs.
        ChannelOptions::INITIALIZED
            | ChannelOptions::ENCRYPT_RDP
            | ChannelOptions::COMPRESS_RDP
            | ChannelOptions::SHOW_PROTOCOL
    }

    fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        Ok(self.handle_pdu(payload)?.into_iter().map(Self::msg).collect())
    }
}

impl SvcClientProcessor for Rail {}
