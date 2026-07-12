//! RAIL virtual-channel PDUs — [MS-RDPERP] 2.2.2.
//!
//! Wire formats ported from FreeRDP `channels/rail/rail_common.c` +
//! `channels/rail/client/rail_orders.c` (order writers/readers) and cross-checked
//! against [MS-RDPERP] 2.2.2.2 (Handshake), 2.2.2.3.1 (Client Execute),
//! 2.2.2.4.2 (SysParam), 2.2.2.6.x (Activate/SysCommand), 2.2.2.3.2 (Exec Result).
//!
//! [MS-RDPERP]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdperp/485e6f6d-2401-4a9c-9330-46454f0c5aba

use ironrdp_core::{
    Encode, EncodeResult, ReadCursor, WriteCursor, cast_length, ensure_size, invalid_field_err,
};
use ironrdp_pdu::DecodeResult;
use ironrdp_svc::SvcEncode;

// [MS-RDPERP] 2.2.2.1 TS_RAIL_PDU_HEADER: orderType (2) + orderLength (2).
// orderLength counts the whole PDU including this 4-byte header.
const RAIL_PDU_HEADER_SIZE: usize = 4;

// ORDER_TYPE values — FreeRDP `include/freerdp/rail.h` ORDER_TYPE enum.
const TS_RAIL_ORDER_EXEC: u16 = 0x0001;
const TS_RAIL_ORDER_ACTIVATE: u16 = 0x0002;
const TS_RAIL_ORDER_SYSPARAM: u16 = 0x0003;
const TS_RAIL_ORDER_SYSCOMMAND: u16 = 0x0004;
const TS_RAIL_ORDER_HANDSHAKE: u16 = 0x0005;
const TS_RAIL_ORDER_CLIENTSTATUS: u16 = 0x000B;
const TS_RAIL_ORDER_HANDSHAKE_EX: u16 = 0x0013;
const TS_RAIL_ORDER_EXEC_RESULT: u16 = 0x0080;

// [MS-RDPERP] 2.2.2.4.2 systemParam values (FreeRDP rail.h SPI_* defines).
const SPI_SET_WORK_AREA: u32 = 0x0000_002F;
const SPI_SET_HIGH_CONTRAST: u32 = 0x0000_0043;

// [MS-RDPERP] 2.2.2.3.1 Client Execute flags (FreeRDP rail.h TS_RAIL_EXEC_FLAG).
const RAIL_EXEC_FLAG_EXPAND_WORKINGDIRECTORY: u16 = 0x0001;
const RAIL_EXEC_FLAG_EXPAND_ARGUMENTS: u16 = 0x0008;

/// SysCommand values [MS-RDPERP] 2.2.2.6.1 (standard Win32 SC_*).
pub const SC_MINIMIZE: u16 = 0xF020;
pub const SC_MAXIMIZE: u16 = 0xF030;
pub const SC_CLOSE: u16 = 0xF060;
pub const SC_RESTORE: u16 = 0xF120;

/// A client-originated RAIL order. Encoding writes the TS_RAIL_PDU_HEADER
/// followed by the order body (FreeRDP `rail_send_pdu`).
#[derive(Debug, Clone)]
pub enum ClientRailOrder {
    /// TS_RAIL_ORDER_HANDSHAKE — buildNumber echoed to the server.
    Handshake { build_number: u32 },
    /// TS_RAIL_ORDER_CLIENTSTATUS — flags (0 = server handles local move/size).
    ClientStatus { flags: u32 },
    /// TS_RAIL_ORDER_SYSPARAM, SPI_SETHIGHCONTRAST = off (headless client).
    SysParamHighContrastOff,
    /// TS_RAIL_ORDER_SYSPARAM, SPI_SETWORKAREA = full desktop (0,0,w,h).
    SysParamWorkArea { width: u16, height: u16 },
    /// TS_RAIL_ORDER_EXEC — launch the published application.
    Exec {
        exe_or_file: String,
        working_dir: String,
        arguments: String,
    },
    /// TS_RAIL_ORDER_ACTIVATE — focus a window.
    Activate { window_id: u32, enabled: bool },
    /// TS_RAIL_ORDER_SYSCOMMAND — minimize/close/restore a window.
    SysCommand { window_id: u32, command: u16 },
}

impl ClientRailOrder {
    fn order_type(&self) -> u16 {
        match self {
            Self::Handshake { .. } => TS_RAIL_ORDER_HANDSHAKE,
            Self::ClientStatus { .. } => TS_RAIL_ORDER_CLIENTSTATUS,
            Self::SysParamHighContrastOff | Self::SysParamWorkArea { .. } => TS_RAIL_ORDER_SYSPARAM,
            Self::Exec { .. } => TS_RAIL_ORDER_EXEC,
            Self::Activate { .. } => TS_RAIL_ORDER_ACTIVATE,
            Self::SysCommand { .. } => TS_RAIL_ORDER_SYSCOMMAND,
        }
    }

    fn body_size(&self) -> usize {
        match self {
            Self::Handshake { .. } => 4,                       // buildNumber
            Self::ClientStatus { .. } => 4,                    // flags
            Self::SysParamHighContrastOff => 4 + 4 + 4 + 2,    // param + flags + colorSchemeLen + cbString(0)
            Self::SysParamWorkArea { .. } => 4 + 2 * 4,        // param + left/top/right/bottom
            Self::Exec {
                exe_or_file,
                working_dir,
                arguments,
            } => 2 + 2 + 2 + 2 + utf16_len(exe_or_file) + utf16_len(working_dir) + utf16_len(arguments),
            Self::Activate { .. } => 4 + 1,   // windowId + enabled
            Self::SysCommand { .. } => 4 + 2, // windowId + command
        }
    }
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count() * 2
}

fn write_utf16(dst: &mut WriteCursor<'_>, s: &str) {
    for unit in s.encode_utf16() {
        dst.write_u16(unit);
    }
}

impl Encode for ClientRailOrder {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_size!(in: dst, size: self.size());

        // TS_RAIL_PDU_HEADER (FreeRDP rail_write_pdu_header).
        dst.write_u16(self.order_type());
        dst.write_u16(cast_length!("ClientRailOrder::orderLength", self.size())?);

        match self {
            Self::Handshake { build_number } => dst.write_u32(*build_number),
            Self::ClientStatus { flags } => dst.write_u32(*flags),
            Self::SysParamHighContrastOff => {
                // rail_write_sysparam_order + rail_write_high_contrast:
                // param, then flags(4)=off, colorSchemeLength(4)=cb+2, cbString(2)=0.
                dst.write_u32(SPI_SET_HIGH_CONTRAST);
                dst.write_u32(0); // HCF flags: high contrast off
                dst.write_u32(2); // colorSchemeLength = colorScheme.length(0) + 2
                dst.write_u16(0); // colorScheme cbString = 0 (empty)
            }
            Self::SysParamWorkArea { width, height } => {
                dst.write_u32(SPI_SET_WORK_AREA);
                dst.write_u16(0); // left
                dst.write_u16(0); // top
                dst.write_u16(*width); // right
                dst.write_u16(*height); // bottom
            }
            Self::Exec {
                exe_or_file,
                working_dir,
                arguments,
            } => {
                // rail_write_client_exec_order: flags, 3 byte-lengths, then the
                // 3 UTF-16LE strings back-to-back (no per-string prefix).
                dst.write_u16(RAIL_EXEC_FLAG_EXPAND_ARGUMENTS | RAIL_EXEC_FLAG_EXPAND_WORKINGDIRECTORY);
                dst.write_u16(cast_length!("Exec::exeLen", utf16_len(exe_or_file))?);
                dst.write_u16(cast_length!("Exec::dirLen", utf16_len(working_dir))?);
                dst.write_u16(cast_length!("Exec::argsLen", utf16_len(arguments))?);
                write_utf16(dst, exe_or_file);
                write_utf16(dst, working_dir);
                write_utf16(dst, arguments);
            }
            Self::Activate { window_id, enabled } => {
                dst.write_u32(*window_id);
                dst.write_u8(u8::from(*enabled));
            }
            Self::SysCommand { window_id, command } => {
                dst.write_u32(*window_id);
                dst.write_u16(*command);
            }
        }

        Ok(())
    }

    fn name(&self) -> &'static str {
        "ClientRailOrder"
    }

    fn size(&self) -> usize {
        RAIL_PDU_HEADER_SIZE + self.body_size()
    }
}

impl SvcEncode for ClientRailOrder {}

/// A server-originated RAIL order, decoded only far enough to drive the client
/// state machine. Runtime-only orders we don't act on collapse to `Other`.
#[derive(Debug, Clone)]
pub enum ServerRailOrder {
    /// Server Handshake / HandshakeEx — triggers the client init sequence.
    Handshake,
    /// TS_RAIL_ORDER_EXEC_RESULT — outcome of the Client Execute PDU.
    ExecResult(ExecResult),
    /// Any other server order (SysParam, MinMaxInfo, LocalMoveSize, LangBar…).
    Other(u16),
}

/// [MS-RDPERP] 2.2.2.3.2 Server Execute Result (RAIL_EXEC_RESULT_ORDER).
#[derive(Debug, Clone, Copy)]
pub struct ExecResult {
    /// TS_RAIL_EXEC_* code; 0 (RAIL_EXEC_S_OK) means success.
    pub exec_result: u16,
    /// Windows HRESULT / GetLastError from the launch attempt.
    pub raw_result: u32,
}

impl ServerRailOrder {
    /// Decode one complete RAIL channel PDU (header + body).
    pub fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        if src.len() < RAIL_PDU_HEADER_SIZE {
            return Err(invalid_field_err!("railPduHeader", "truncated"));
        }
        let order_type = src.read_u16();
        let _order_length = src.read_u16();

        match order_type {
            TS_RAIL_ORDER_HANDSHAKE | TS_RAIL_ORDER_HANDSHAKE_EX => Ok(Self::Handshake),
            TS_RAIL_ORDER_EXEC_RESULT => {
                // flags(2) + execResult(2) + rawResult(4) + padding(2) + exeOrFile(string)
                if src.len() < 8 {
                    return Err(invalid_field_err!("execResult", "truncated"));
                }
                let _flags = src.read_u16();
                let exec_result = src.read_u16();
                let raw_result = src.read_u32();
                Ok(Self::ExecResult(ExecResult { exec_result, raw_result }))
            }
            other => Ok(Self::Other(other)),
        }
    }
}
