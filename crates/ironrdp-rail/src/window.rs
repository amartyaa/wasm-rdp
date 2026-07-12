//! Window Information Orders — [MS-RDPERP] 2.2.1.3, delivered as Alternate
//! Secondary Drawing Orders inside TS_UPDATETYPE_ORDERS.
//!
//! Ported from FreeRDP `libfreerdp/core/window.c` (order-field parsing) and
//! `libfreerdp/core/orders.c` (`update_recv_order` / `update_recv_altsec_order`
//! order-class dispatch). Only the fields needed to place a window on screen are
//! surfaced; every order is bounded by its `orderSize` so unparsed trailing
//! fields (icons, Win8+ appbar/taskbar, notify icons) are skipped safely
//! ([MS-RDPERP] 2.2.1.3.1.1 TS_WINDOW_ORDER_HEADER.OrderSize).

use tracing::trace;

// Order-class control-flag bits — FreeRDP `libfreerdp/core/orders.h`.
const ORDER_STANDARD: u8 = 0x01;
const ORDER_TYPE_WINDOW: u8 = 0x0B; // ALTSEC window order
const ORDER_TYPE_FRAME_MARKER: u8 = 0x0D;

// FieldsPresentFlags — FreeRDP `include/freerdp/window.h`.
const WINDOW_ORDER_TYPE_WINDOW: u32 = 0x0100_0000;
const WINDOW_ORDER_TYPE_NOTIFY: u32 = 0x0200_0000;
const WINDOW_ORDER_TYPE_DESKTOP: u32 = 0x0400_0000;
const WINDOW_ORDER_STATE_NEW: u32 = 0x1000_0000;
const WINDOW_ORDER_STATE_DELETED: u32 = 0x2000_0000;
const WINDOW_ORDER_ICON: u32 = 0x4000_0000;
const WINDOW_ORDER_CACHED_ICON: u32 = 0x8000_0000;

const WINDOW_ORDER_FIELD_OWNER: u32 = 0x0000_0002;
const WINDOW_ORDER_FIELD_STYLE: u32 = 0x0000_0008;
const WINDOW_ORDER_FIELD_SHOW: u32 = 0x0000_0010;
const WINDOW_ORDER_FIELD_TITLE: u32 = 0x0000_0004;
const WINDOW_ORDER_FIELD_CLIENT_AREA_OFFSET: u32 = 0x0000_4000;
const WINDOW_ORDER_FIELD_CLIENT_AREA_SIZE: u32 = 0x0001_0000;
const WINDOW_ORDER_FIELD_RESIZE_MARGIN_X: u32 = 0x0000_0080;
const WINDOW_ORDER_FIELD_RESIZE_MARGIN_Y: u32 = 0x0800_0000;
const WINDOW_ORDER_FIELD_RP_CONTENT: u32 = 0x0002_0000;
const WINDOW_ORDER_FIELD_ROOT_PARENT: u32 = 0x0004_0000;
const WINDOW_ORDER_FIELD_WND_OFFSET: u32 = 0x0000_0800;
const WINDOW_ORDER_FIELD_WND_CLIENT_DELTA: u32 = 0x0000_8000;
const WINDOW_ORDER_FIELD_WND_SIZE: u32 = 0x0000_0400;
const WINDOW_ORDER_FIELD_WND_RECTS: u32 = 0x0000_0100;
const WINDOW_ORDER_FIELD_VIS_OFFSET: u32 = 0x0000_1000;
const WINDOW_ORDER_FIELD_VISIBILITY: u32 = 0x0000_0200;

const WINDOW_ORDER_FIELD_DESKTOP_ZORDER: u32 = 0x0000_0010;
const WINDOW_ORDER_FIELD_DESKTOP_ACTIVE_WND: u32 = 0x0000_0020;

/// Window `showState` values ([MS-RDPERP] 2.2.1.3.1.2.1, standard Win32 SW_*).
pub const SW_HIDE: u8 = 0;
pub const SW_SHOWMINIMIZED: u8 = 2;

/// A visibility rectangle (window's visible region), server coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

/// Fields extracted from a window create/update order. `None` means the order
/// did not carry that field (client keeps its previous value).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowInfo {
    pub window_id: u32,
    pub is_new: bool,
    pub show_state: Option<u8>,
    pub title: Option<String>,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Present only when the order carried >1 visibility rect (for clip-path).
    pub visibility_rects: Option<Vec<Rect>>,
}

/// One decoded window-management event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowEvent {
    NewOrUpdate(WindowInfo),
    Deleted(u32),
    /// Actively-monitored desktop z-order, top-most first.
    ZOrder(Vec<u32>),
    ActiveWindow(u32),
}

/// Panic-free forward reader over an untrusted byte slice.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }
    fn u8(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn u16(&mut self) -> Option<u16> {
        let s = self.buf.get(self.pos..self.pos + 2)?;
        self.pos += 2;
        Some(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        let s = self.buf.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        if self.remaining() < n {
            return None;
        }
        self.pos += n;
        Some(())
    }
    /// RAIL_UNICODE_STRING: cbString (u16) + UTF-16LE bytes.
    fn unicode_string(&mut self) -> Option<String> {
        let cb = self.u16()? as usize;
        let bytes = self.buf.get(self.pos..self.pos + cb)?;
        self.pos += cb;
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Some(String::from_utf16_lossy(&units))
    }
}

/// Parse a normalized Orders payload (`numberOrders: u16` + order data).
/// Returns all window events found; a malformed order aborts parsing of the
/// remainder (logged, non-fatal — matches the "swallow PDU errors" policy).
pub fn parse(payload: &[u8]) -> Vec<WindowEvent> {
    let mut events = Vec::new();
    let mut r = Reader::new(payload);
    let Some(number_orders) = r.u16() else {
        return events;
    };

    for _ in 0..number_orders {
        if r.remaining() == 0 {
            break;
        }
        let order_start = r.pos;
        let Some(control_flags) = r.u8() else { break };

        // RAIL sends window metadata as ALTSEC orders (STANDARD bit clear). A
        // STANDARD order (primary/secondary) has no orderSize we can skip by, so
        // we can't safely continue — bail. Zeroed order_support should prevent
        // the server ever sending one (plan risk R3).
        if control_flags & ORDER_STANDARD != 0 {
            trace!("RAIL orders: unexpected standard order 0x{control_flags:02x}, aborting parse");
            break;
        }

        let order_type = control_flags >> 2;
        match order_type {
            ORDER_TYPE_FRAME_MARKER => {
                // action (4 bytes), no orderSize — FreeRDP update_read_frame_marker_order.
                if r.skip(4).is_none() {
                    break;
                }
            }
            ORDER_TYPE_WINDOW => {
                // TS_WINDOW_ORDER_HEADER: orderSize (2) + fieldFlags (4).
                let Some(order_size) = r.u16() else { break };
                let Some(field_flags) = r.u32() else { break };
                let order_end = order_start + order_size as usize;
                if order_size < 3 || order_end > payload.len() {
                    trace!("RAIL orders: bad orderSize {order_size}, aborting");
                    break;
                }

                parse_window_order(&mut r, field_flags, order_end, &mut events);

                // Trust orderSize: jump to the next order regardless of which
                // trailing fields we skipped (icons/notify/Win8+ fields).
                r.pos = order_end;
            }
            other => {
                trace!("RAIL orders: unhandled altsec order type 0x{other:02x}, aborting");
                break;
            }
        }
    }

    events
}

fn parse_window_order(r: &mut Reader<'_>, field_flags: u32, order_end: usize, events: &mut Vec<WindowEvent>) {
    if field_flags & WINDOW_ORDER_TYPE_DESKTOP != 0 {
        // Desktop order carries no windowId.
        parse_desktop_order(r, field_flags, events);
        return;
    }

    // Only WINDOW and NOTIFY orders start with a windowId; anything else with
    // neither type bit set is malformed for our purposes — skip via orderSize.
    if field_flags & (WINDOW_ORDER_TYPE_WINDOW | WINDOW_ORDER_TYPE_NOTIFY) == 0 {
        return;
    }
    let Some(window_id) = r.u32() else { return };

    if field_flags & (WINDOW_ORDER_ICON | WINDOW_ORDER_CACHED_ICON) != 0 {
        return; // icon order — skipped via orderSize by caller
    }
    if field_flags & WINDOW_ORDER_TYPE_NOTIFY != 0 {
        return; // systray notify icon — not supported (v1), skipped via orderSize
    }
    if field_flags & WINDOW_ORDER_STATE_DELETED != 0 {
        events.push(WindowEvent::Deleted(window_id));
        return;
    }

    // TYPE_WINDOW create/update: parse fields in FreeRDP's exact order up to
    // VISIBILITY (the last field we care about); the caller skips the tail.
    if let Some(info) = parse_window_state(r, field_flags, window_id, order_end) {
        events.push(WindowEvent::NewOrUpdate(info));
    }
}

fn parse_window_state(r: &mut Reader<'_>, ff: u32, window_id: u32, order_end: usize) -> Option<WindowInfo> {
    let mut info = WindowInfo {
        window_id,
        is_new: ff & WINDOW_ORDER_STATE_NEW != 0,
        ..Default::default()
    };

    if ff & WINDOW_ORDER_FIELD_OWNER != 0 {
        r.skip(4)?; // ownerWindowId
    }
    if ff & WINDOW_ORDER_FIELD_STYLE != 0 {
        r.skip(8)?; // style + extendedStyle
    }
    if ff & WINDOW_ORDER_FIELD_SHOW != 0 {
        info.show_state = Some(r.u8()?);
    }
    if ff & WINDOW_ORDER_FIELD_TITLE != 0 {
        info.title = Some(r.unicode_string()?);
    }
    if ff & WINDOW_ORDER_FIELD_CLIENT_AREA_OFFSET != 0 {
        r.skip(8)?; // clientOffsetX/Y
    }
    if ff & WINDOW_ORDER_FIELD_CLIENT_AREA_SIZE != 0 {
        r.skip(8)?; // clientAreaWidth/Height
    }
    if ff & WINDOW_ORDER_FIELD_RESIZE_MARGIN_X != 0 {
        r.skip(8)?;
    }
    if ff & WINDOW_ORDER_FIELD_RESIZE_MARGIN_Y != 0 {
        r.skip(8)?;
    }
    if ff & WINDOW_ORDER_FIELD_RP_CONTENT != 0 {
        r.skip(1)?; // RPContent
    }
    if ff & WINDOW_ORDER_FIELD_ROOT_PARENT != 0 {
        r.skip(4)?; // rootParentHandle
    }
    if ff & WINDOW_ORDER_FIELD_WND_OFFSET != 0 {
        info.x = Some(r.i32()?);
        info.y = Some(r.i32()?);
    }
    if ff & WINDOW_ORDER_FIELD_WND_CLIENT_DELTA != 0 {
        r.skip(8)?; // windowClientDeltaX/Y
    }
    if ff & WINDOW_ORDER_FIELD_WND_SIZE != 0 {
        info.width = Some(r.u32()?);
        info.height = Some(r.u32()?);
    }
    if ff & WINDOW_ORDER_FIELD_WND_RECTS != 0 {
        let num = r.u16()? as usize;
        r.skip(num * 8)?; // numWindowRects * RECTANGLE_16
    }
    if ff & WINDOW_ORDER_FIELD_VIS_OFFSET != 0 {
        r.skip(8)?; // visibleOffsetX/Y
    }
    if ff & WINDOW_ORDER_FIELD_VISIBILITY != 0 {
        let num = r.u16()? as usize;
        let mut rects = Vec::with_capacity(num);
        for _ in 0..num {
            rects.push(Rect {
                left: r.u16()?,
                top: r.u16()?,
                right: r.u16()?,
                bottom: r.u16()?,
            });
        }
        // Only meaningful for non-rectangular/occluded regions (>1 rect).
        if rects.len() > 1 {
            info.visibility_rects = Some(rects);
        }
    }

    let _ = order_end; // remaining fields skipped by caller via orderSize
    Some(info)
}

fn parse_desktop_order(r: &mut Reader<'_>, ff: u32, events: &mut Vec<WindowEvent>) {
    // update_read_desktop_actively_monitored_order: ACTIVE_WND then ZORDER.
    if ff & WINDOW_ORDER_FIELD_DESKTOP_ACTIVE_WND != 0 {
        if let Some(active) = r.u32() {
            events.push(WindowEvent::ActiveWindow(active));
        } else {
            return;
        }
    }
    if ff & WINDOW_ORDER_FIELD_DESKTOP_ZORDER != 0 {
        let Some(num) = r.u8() else { return };
        let mut ids = Vec::with_capacity(num as usize);
        for _ in 0..num {
            let Some(id) = r.u32() else { return };
            ids.push(id);
        }
        events.push(WindowEvent::ZOrder(ids));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build one ALTSEC window order: controlFlags, orderSize, fieldFlags, body.
    fn window_order(field_flags: u32, body: &[u8]) -> Vec<u8> {
        let order_size = 1 + 2 + 4 + body.len(); // controlFlags + orderSize + fieldFlags + body
        let mut o = Vec::new();
        o.push(ORDER_TYPE_WINDOW << 2); // STANDARD bit clear, altsec type 0x0B
        o.extend_from_slice(&(order_size as u16).to_le_bytes());
        o.extend_from_slice(&field_flags.to_le_bytes());
        o.extend_from_slice(body);
        o
    }

    fn payload(orders: &[Vec<u8>]) -> Vec<u8> {
        let mut p = (orders.len() as u16).to_le_bytes().to_vec();
        for o in orders {
            p.extend_from_slice(o);
        }
        p
    }

    #[test]
    fn create_window_with_geometry_and_title() {
        // windowId + SHOW(showState=1) + TITLE("Hi") + WND_OFFSET(30,40) + WND_SIZE(300,200)
        let mut body = Vec::new();
        body.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes()); // windowId
        body.push(1); // showState = SW_SHOWNORMAL
        let title: Vec<u16> = "Hi".encode_utf16().collect();
        body.extend_from_slice(&((title.len() * 2) as u16).to_le_bytes());
        for u in &title {
            body.extend_from_slice(&u.to_le_bytes());
        }
        body.extend_from_slice(&30i32.to_le_bytes()); // windowOffsetX
        body.extend_from_slice(&40i32.to_le_bytes()); // windowOffsetY
        body.extend_from_slice(&300u32.to_le_bytes()); // windowWidth
        body.extend_from_slice(&200u32.to_le_bytes()); // windowHeight

        let ff = WINDOW_ORDER_TYPE_WINDOW
            | WINDOW_ORDER_STATE_NEW
            | WINDOW_ORDER_FIELD_SHOW
            | WINDOW_ORDER_FIELD_TITLE
            | WINDOW_ORDER_FIELD_WND_OFFSET
            | WINDOW_ORDER_FIELD_WND_SIZE;
        let ev = parse(&payload(&[window_order(ff, &body)]));
        assert_eq!(ev.len(), 1);
        match &ev[0] {
            WindowEvent::NewOrUpdate(w) => {
                assert_eq!(w.window_id, 0xDEAD_BEEF);
                assert!(w.is_new);
                assert_eq!(w.show_state, Some(1));
                assert_eq!(w.title.as_deref(), Some("Hi"));
                assert_eq!(w.x, Some(30));
                assert_eq!(w.y, Some(40));
                assert_eq!(w.width, Some(300));
                assert_eq!(w.height, Some(200));
            }
            other => panic!("expected NewOrUpdate, got {other:?}"),
        }
    }

    #[test]
    fn delete_window() {
        let mut body = Vec::new();
        body.extend_from_slice(&0x1234u32.to_le_bytes());
        let ff = WINDOW_ORDER_TYPE_WINDOW | WINDOW_ORDER_STATE_DELETED;
        let ev = parse(&payload(&[window_order(ff, &body)]));
        assert_eq!(ev, vec![WindowEvent::Deleted(0x1234)]);
    }

    #[test]
    fn desktop_zorder_and_active() {
        let mut body = Vec::new();
        body.extend_from_slice(&0xAAu32.to_le_bytes()); // activeWindowId
        body.push(2); // numWindowIds
        body.extend_from_slice(&0xAAu32.to_le_bytes());
        body.extend_from_slice(&0xBBu32.to_le_bytes());
        let ff = WINDOW_ORDER_TYPE_DESKTOP | WINDOW_ORDER_FIELD_DESKTOP_ACTIVE_WND | WINDOW_ORDER_FIELD_DESKTOP_ZORDER;
        let ev = parse(&payload(&[window_order(ff, &body)]));
        assert_eq!(
            ev,
            vec![WindowEvent::ActiveWindow(0xAA), WindowEvent::ZOrder(vec![0xAA, 0xBB])]
        );
    }

    #[test]
    fn trailing_unknown_fields_skipped_via_ordersize() {
        // SHOW only, but append 5 junk trailing bytes inside orderSize.
        let mut body = Vec::new();
        body.extend_from_slice(&7u32.to_le_bytes()); // windowId
        body.push(SW_SHOWMINIMIZED);
        body.extend_from_slice(&[0xFF; 5]); // pretend Win8+ trailing fields
        let ff = WINDOW_ORDER_TYPE_WINDOW | WINDOW_ORDER_FIELD_SHOW;
        // second order proves we resync to the next order boundary
        let mut body2 = Vec::new();
        body2.extend_from_slice(&8u32.to_le_bytes());
        let ff2 = WINDOW_ORDER_TYPE_WINDOW | WINDOW_ORDER_STATE_DELETED;
        let ev = parse(&payload(&[window_order(ff, &body), window_order(ff2, &body2)]));
        assert_eq!(ev.len(), 2);
        assert!(matches!(&ev[0], WindowEvent::NewOrUpdate(w) if w.show_state == Some(SW_SHOWMINIMIZED)));
        assert_eq!(ev[1], WindowEvent::Deleted(8));
    }

    #[test]
    fn frame_marker_between_orders() {
        // frame marker (type 0x0D, 4-byte action) then a delete order.
        let mut fm = vec![ORDER_TYPE_FRAME_MARKER << 2];
        fm.extend_from_slice(&0u32.to_le_bytes());
        let mut body = Vec::new();
        body.extend_from_slice(&9u32.to_le_bytes());
        let del = window_order(WINDOW_ORDER_TYPE_WINDOW | WINDOW_ORDER_STATE_DELETED, &body);
        let ev = parse(&payload(&[fm, del]));
        assert_eq!(ev, vec![WindowEvent::Deleted(9)]);
    }
}
