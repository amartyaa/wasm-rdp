//! RAIL (RemoteApp) client support — [MS-RDPERP].
//!
//! - [`client`]: the `rail` static virtual channel processor + launch sequence.
//! - [`window`]: Window Information Order parsing (delivered as graphics Orders).
//! - [`pdu`]: on-the-wire RAIL PDUs.
//!
//! Ported from FreeRDP's `channels/rail` + `libfreerdp/core/window.c`; see
//! module docs for the exact source files and spec sections.

pub mod client;
pub mod pdu;
pub mod window;

pub use client::{Rail, RailClientHandler};
pub use pdu::{ExecResult, SC_CLOSE, SC_MAXIMIZE, SC_MINIMIZE, SC_RESTORE};
pub use window::{parse as parse_window_orders, Rect, WindowEvent, WindowInfo, SW_HIDE, SW_SHOWMINIMIZED};
