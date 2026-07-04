use std::cell::RefCell;
use std::mem;
use std::rc::Rc;

use anyhow::Context as _;
use futures_channel::mpsc;
use futures_util::{SinkExt, StreamExt, select};
use futures_util::FutureExt;
use gloo_net::websocket::futures::WebSocket;
use gloo_net::websocket;
use ironrdp::connector::{self, ClientConnector, ClientConnectorState, ConnectionResult, Credentials, Sequence as _, State as _};
use ironrdp::pdu::gcc;
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::pdu::input::fast_path::FastPathInputEvent;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageOutput};
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp_core::WriteBuf;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::canvas::Canvas;
use crate::framed::WasmFramed;
use crate::redirect;
use crate::{log, log_error};

// ===== EGFX graphics pipeline (MS-RDPEGFX) — RFX-Progressive over WASM =====
// On a GPU-less Windows host the graphics pipeline DVC delivers RFX-Progressive
// (WireToSurface2) instead of the legacy RFX fast-path. We advertise EGFX **V8
// only** (no AVC) so the server always picks progressive — there's no GPU to
// H.264-encode, and we have no WebCodecs path here. The progressive codec is
// decoded on the WASM thread (`ironrdp_graphics::rfx_progressive`) and blitted
// straight to the shared monitor canvas(es); when EGFX is active the legacy
// GraphicsUpdate path goes quiet, so this handler is the sole renderer.
// xrdp ignores the GFX flag entirely and stays on legacy RFX — no regression.

use std::collections::HashMap;
use ironrdp::graphics::rfx_progressive::ProgressiveDecoder;

/// Per-EGFX-surface render state owned by the graphics-pipeline handler.
struct GfxSurface {
    decoder: ProgressiveDecoder,
    /// RGBA framebuffer for this surface (surface-local, `width*height*4` bytes).
    fb: Vec<u8>,
    width: u16,
    height: u16,
    /// Surface position within the combined desktop (from MapSurfaceToOutput).
    origin_x: u16,
    origin_y: u16,
    /// Whether this surface is mapped to the output. Windows composes in
    /// OFFSCREEN surfaces and copies/maps them later — blitting an unmapped
    /// surface to the canvas paints garbage over real output.
    mapped: bool,
}

/// One bitmap-cache slot (MS-RDPEGFX 3.3.1.4). The cache is MANDATORY for a
/// GFX client (Client Implementation Requirements) — servers save regions with
/// SurfaceToCache and restore them later with CacheToSurface (cursor
/// save-under, window scroll). No-oping these leaves never-painted (black)
/// regions and cursor trails.
struct CacheSlot {
    width: u16,
    height: u16,
    data: Vec<u8>, // RGBA, width*height*4
}

/// Which video codec is actually painting, surfaced to the HUD. Defaults to the
/// legacy RFX fast-path and flips when the EGFX pipeline delivers a frame
/// (progressive on a GPU-less Windows host, Planar on xrdp).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum VideoCodec {
    Rfx,
    Progressive,
    Planar,
    Uncompressed,
}

impl VideoCodec {
    fn as_str(self) -> &'static str {
        match self {
            VideoCodec::Rfx => "RFX",
            VideoCodec::Progressive => "RFX-Progressive",
            VideoCodec::Planar => "Planar",
            VideoCodec::Uncompressed => "Uncompressed",
        }
    }
}

struct WasmGfxHandler {
    /// Shared monitor canvases (same list the legacy path renders to).
    surfaces: Rc<RefCell<Vec<Canvas>>>,
    gfx: HashMap<u16, GfxSurface>,
    /// Bitmap cache: slot → saved pixels. Server-managed; we just store/restore.
    cache: HashMap<u16, CacheSlot>,
    /// Set once the first EGFX surface exists; run_session uses it to stop the
    /// legacy renderer from blitting its (black) DecodedImage over EGFX output.
    egfx_active: Rc<std::cell::Cell<bool>>,
    /// Live codec the EGFX pipeline is painting with; read by the Session HUD getter.
    video_codec: Rc<std::cell::Cell<VideoCodec>>,
    /// Progressive frames rendered; gates verbose first-frame logging.
    frames: u32,
    /// Planar/uncompressed BitmapUpdate tiles rendered; gates verbose logging.
    bitmaps: u32,
    /// Per-op log counters (each op type logged independently, ~first 40).
    log_fill: u32,
    log_s2s: u32,
    log_cache: u32,
    log_other: u32,
}

// SAFETY: `GraphicsPipelineHandler: Send`, but the handler holds `Rc`/web-sys
// canvas state that is not `Send`. The wasm32 `--target web` build is strictly
// single-threaded (no threads/atomics), and this handler is only ever created
// and invoked on that one JS thread, so there is never cross-thread access.
unsafe impl Send for WasmGfxHandler {}

impl WasmGfxHandler {
    fn new(
        surfaces: Rc<RefCell<Vec<Canvas>>>,
        egfx_active: Rc<std::cell::Cell<bool>>,
        video_codec: Rc<std::cell::Cell<VideoCodec>>,
    ) -> Self {
        Self {
            surfaces,
            gfx: HashMap::new(),
            cache: HashMap::new(),
            egfx_active,
            video_codec,
            frames: 0,
            bitmaps: 0,
            log_fill: 0,
            log_s2s: 0,
            log_cache: 0,
            log_other: 0,
        }
    }
}

impl ironrdp::egfx::client::GraphicsPipelineHandler for WasmGfxHandler {
    fn capabilities(&self) -> Vec<ironrdp::egfx::pdu::CapabilitySet> {
        use ironrdp::egfx::pdu::{CapabilitiesV8Flags, CapabilitySet};
        // Advertise V8 only (no AVC). A GPU-less host then selects RFX-Progressive;
        // a GPU host would otherwise send H.264 we can't decode here.
        //
        // SMALL_CACHE: the bitmap cache is MANDATORY per MS-RDPEGFX (there is no
        // opt-out flag — empty flags still mean "full 100MB cache"); SMALL_CACHE
        // just bounds it. We implement the cache below.
        log("[EGFX] graphics channel opened — advertising V8 (RFX-Progressive, no AVC, small cache)");
        vec![CapabilitySet::V8 {
            flags: CapabilitiesV8Flags::SMALL_CACHE,
        }]
    }

    fn on_capabilities_confirmed(&mut self, caps: &ironrdp::egfx::pdu::CapabilitySet) {
        log(&format!("[EGFX] capabilities confirmed by server: {caps:?}"));
    }

    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        log(&format!("[EGFX] reset graphics {width}x{height} — clearing {} surface(s)", self.gfx.len()));
        self.gfx.clear();
    }

    fn on_surface_created(&mut self, surface: &ironrdp::egfx::client::Surface) {
        log(&format!(
            "[EGFX] surface created id={} {}x{} fmt={:?}",
            surface.id, surface.width, surface.height, surface.pixel_format
        ));
        let w = surface.width;
        let h = surface.height;
        self.gfx.insert(
            surface.id,
            GfxSurface {
                decoder: ProgressiveDecoder::new(w, h),
                fb: vec![0u8; usize::from(w) * usize::from(h) * 4],
                width: w,
                height: h,
                origin_x: 0,
                origin_y: 0,
                mapped: false,
            },
        );
        // EGFX now owns rendering — the legacy DecodedImage path must stop
        // painting (its framebuffer is black; blitting it stamps over us).
        self.egfx_active.set(true);
    }

    fn on_surface_deleted(&mut self, surface_id: u16) {
        log(&format!("[EGFX] surface deleted id={surface_id}"));
        self.gfx.remove(&surface_id);
    }

    fn on_surface_mapped(&mut self, surface_id: u16, origin_x: u32, origin_y: u32) {
        log(&format!("[EGFX] surface {surface_id} mapped to output ({origin_x},{origin_y})"));
        if let Some(s) = self.gfx.get_mut(&surface_id) {
            s.origin_x = origin_x as u16;
            s.origin_y = origin_y as u16;
            s.mapped = true;
            // The surface may have been fully composed offscreen before being
            // mapped — present its current contents now.
            let full = [ironrdp::pdu::geometry::InclusiveRectangle {
                left: 0,
                top: 0,
                right: s.width.saturating_sub(1),
                bottom: s.height.saturating_sub(1),
            }];
            blit_rects(&self.surfaces, &s.fb, s.width, s.height, s.origin_x, s.origin_y, &full);
            crate::notify_frame();
        }
    }

    fn on_map_surface_to_scaled_output(&mut self, pdu: &ironrdp::egfx::pdu::MapSurfaceToScaledOutputPdu) {
        // Treat like a plain map; we don't scale (target size is logged so a
        // mismatch is visible in testing).
        log(&format!(
            "[EGFX] surface {} mapped to SCALED output ({},{}) target {}x{} — rendering unscaled",
            pdu.surface_id, pdu.output_origin_x, pdu.output_origin_y, pdu.target_width, pdu.target_height
        ));
        self.on_surface_mapped(pdu.surface_id, pdu.output_origin_x, pdu.output_origin_y);
    }

    fn on_progressive_data(
        &mut self,
        surface_id: u16,
        _origin_x: u32,
        _origin_y: u32,
        _width: u16,
        _height: u16,
        data: &[u8],
    ) {
        let Some(s) = self.gfx.get_mut(&surface_id) else {
            log(&format!("[EGFX] progressive data for unknown surface {surface_id} ({} bytes)", data.len()));
            return;
        };

        let stride = usize::from(s.width) * 4;
        let update = match s.decoder.decode(data, &mut s.fb, stride) {
            Ok(u) => u,
            Err(e) => {
                log_error(&format!("[EGFX] progressive decode failed (surface {surface_id}): {e}"));
                return;
            }
        };
        self.video_codec.set(VideoCodec::Progressive);

        // Verbose first-frame diagnostics + ALWAYS log any tile that failed to
        // decode (these are the black-region / cursor-trail suspects).
        if self.frames < 5 || update.errors > 0 {
            log(&format!(
                "[EGFX] prog frame {} surf={surface_id} {}x{} extrap={} regions={}(rects0={}) tiles[simple={} first={} upgrade={} coeffDiff={}] dirty={} errors={} black={} firstBlack={:?} bytes={}",
                self.frames, s.width, s.height, update.extrapolate,
                update.regions, update.region0_rects,
                update.tiles_simple, update.tiles_first, update.tiles_upgrade, update.coeff_diff_tiles,
                update.dirty.len(), update.errors, update.black_tiles, update.first_black, data.len()
            ));
            if let Some(err) = &update.first_error {
                log_error(&format!("[EGFX] first tile error: {err}"));
            }
        }
        self.frames = self.frames.wrapping_add(1);

        if update.dirty.is_empty() || !s.mapped {
            return;
        }

        if blit_rects(&self.surfaces, &s.fb, s.width, s.height, s.origin_x, s.origin_y, &update.dirty) {
            crate::notify_frame();
        }
    }

    // Decoded RGBA tile from the EGFX client (RDP6 Planar — what xrdp uses — or
    // Uncompressed). Composite into the surface framebuffer at the destination
    // rectangle and blit to the canvas.
    fn on_bitmap_updated(&mut self, update: &ironrdp::egfx::client::BitmapUpdate) {
        let Some(s) = self.gfx.get_mut(&update.surface_id) else { return };
        self.video_codec.set(
            if matches!(update.codec_id, ironrdp::egfx::pdu::Codec1Type::Planar) {
                VideoCodec::Planar
            } else {
                VideoCodec::Uncompressed
            },
        );
        let dx = usize::from(update.destination_rectangle.left);
        let dy = usize::from(update.destination_rectangle.top);
        let w = usize::from(update.width);
        let h = usize::from(update.height);
        let sw = usize::from(s.width);
        let sh = usize::from(s.height);

        // Sample the incoming tile: is it (near-)black? The cursor/region black
        // artifacts on xrdp appear to come from Planar tiles, so always flag black
        // ones (with position) regardless of frame count.
        let is_black = {
            const T: u8 = 10;
            let mut black = !update.data.is_empty();
            'outer: for sy in [4usize, h / 2, h.saturating_sub(4)] {
                for sx in [4usize, w / 2, w.saturating_sub(4)] {
                    let o = (sy * w + sx) * 4;
                    if o + 2 < update.data.len()
                        && (update.data[o] > T || update.data[o + 1] > T || update.data[o + 2] > T)
                    {
                        black = false;
                        break 'outer;
                    }
                }
            }
            black
        };
        if self.bitmaps < 6 {
            log(&format!(
                "[EGFX] bitmap update surf={} codec={:?} at ({dx},{dy}) {w}x{h} black={is_black} bytes={}",
                update.surface_id, update.codec_id, update.data.len()
            ));
        }
        self.bitmaps = self.bitmaps.wrapping_add(1);

        // NOTE: black tiles are painted as-is. xrdp deliberately clears regions to
        // black (513-byte all-black RLE planar) before painting content and relies
        // on the bitmap cache to restore saved pixels afterwards — now that the
        // cache is implemented the sequence resolves correctly, and skipping black
        // would break genuinely-black content (e.g. terminals).

        // Copy the w×h RGBA tile into the surface framebuffer, clipped to bounds.
        let cw = w.min(sw.saturating_sub(dx));
        for row in 0..h {
            let sy = dy + row;
            if sy >= sh {
                break;
            }
            let src_off = row * w * 4;
            let dst_off = (sy * sw + dx) * 4;
            if src_off + cw * 4 <= update.data.len() && dst_off + cw * 4 <= s.fb.len() {
                s.fb[dst_off..dst_off + cw * 4].copy_from_slice(&update.data[src_off..src_off + cw * 4]);
            }
        }

        if !s.mapped {
            return;
        }
        let dirty = [ironrdp::pdu::geometry::InclusiveRectangle {
            left: dx as u16,
            top: dy as u16,
            right: (dx + w).min(sw).saturating_sub(1) as u16,
            bottom: (dy + h).min(sh).saturating_sub(1) as u16,
        }];
        if blit_rects(&self.surfaces, &s.fb, s.width, s.height, s.origin_x, s.origin_y, &dirty) {
            crate::notify_frame();
        }
    }

    fn on_solid_fill(&mut self, pdu: &ironrdp::egfx::pdu::SolidFillPdu) {
        if self.log_fill < 40 {
            self.log_fill += 1;
            let c = &pdu.fill_pixel;
            let first = pdu.rectangles.first();
            log(&format!(
                "[EGFX] SolidFill surf={} rgb=({},{},{}) rects={} first={:?}",
                pdu.surface_id, c.r, c.g, c.b, pdu.rectangles.len(),
                first.map(|r| (r.left, r.top, r.right, r.bottom))
            ));
        }
        let Some(s) = self.gfx.get_mut(&pdu.surface_id) else { return };
        let c = &pdu.fill_pixel;
        let rgba = [c.r, c.g, c.b, 0xFF];
        let sw = usize::from(s.width);
        let sh = usize::from(s.height);

        let mut dirty = Vec::with_capacity(pdu.rectangles.len());
        for rect in &pdu.rectangles {
            let x0 = usize::from(rect.left).min(sw);
            let y0 = usize::from(rect.top).min(sh);
            let x1 = usize::from(rect.right).min(sw);
            let y1 = usize::from(rect.bottom).min(sh);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            for y in y0..y1 {
                let row = y * sw * 4;
                for x in x0..x1 {
                    let off = row + x * 4;
                    s.fb[off..off + 4].copy_from_slice(&rgba);
                }
            }
            dirty.push(ironrdp::pdu::geometry::InclusiveRectangle {
                left: x0 as u16,
                top: y0 as u16,
                right: (x1 - 1) as u16,
                bottom: (y1 - 1) as u16,
            });
        }
        if s.mapped && blit_rects(&self.surfaces, &s.fb, s.width, s.height, s.origin_x, s.origin_y, &dirty) {
            crate::notify_frame();
        }
    }

    fn on_surface_to_surface(&mut self, pdu: &ironrdp::egfx::pdu::SurfaceToSurfacePdu) {
        if self.log_s2s < 40 {
            self.log_s2s += 1;
            let r = &pdu.source_rectangle;
            log(&format!(
                "[EGFX] SurfaceToSurface {}->{} src=({},{},{},{}) destPts={} first={:?}",
                pdu.source_surface_id, pdu.destination_surface_id,
                r.left, r.top, r.right, r.bottom, pdu.destination_points.len(),
                pdu.destination_points.first().map(|p| (p.x, p.y))
            ));
        }

        let src = &pdu.source_rectangle;
        let src_x = usize::from(src.left);
        let src_y = usize::from(src.top);
        let rect_w = usize::from(src.right).saturating_sub(src_x);
        let rect_h = usize::from(src.bottom).saturating_sub(src_y);

        // Snapshot the source region into a temp buffer. Needed for overlapping
        // same-surface copies (scroll), and it sidesteps the double-borrow for
        // cross-surface copies (Windows composes via offscreen surfaces).
        let (tmp, copy_w, copy_h) = {
            let Some(src_s) = self.gfx.get(&pdu.source_surface_id) else { return };
            let ssw = usize::from(src_s.width);
            let ssh = usize::from(src_s.height);
            let copy_w = rect_w.min(ssw.saturating_sub(src_x));
            let copy_h = rect_h.min(ssh.saturating_sub(src_y));
            if copy_w == 0 || copy_h == 0 {
                return;
            }
            let mut tmp = vec![0u8; copy_w * copy_h * 4];
            for row in 0..copy_h {
                let s_off = ((src_y + row) * ssw + src_x) * 4;
                let t_off = row * copy_w * 4;
                tmp[t_off..t_off + copy_w * 4].copy_from_slice(&src_s.fb[s_off..s_off + copy_w * 4]);
            }
            (tmp, copy_w, copy_h)
        };

        let Some(s) = self.gfx.get_mut(&pdu.destination_surface_id) else { return };
        let sw = usize::from(s.width);
        let sh = usize::from(s.height);

        let mut dirty = Vec::with_capacity(pdu.destination_points.len());
        for p in &pdu.destination_points {
            let dx = usize::from(p.x);
            let dy = usize::from(p.y);
            let w = copy_w.min(sw.saturating_sub(dx));
            let h = copy_h.min(sh.saturating_sub(dy));
            if w == 0 || h == 0 {
                continue;
            }
            for row in 0..h {
                let t_off = row * copy_w * 4;
                let d_off = ((dy + row) * sw + dx) * 4;
                s.fb[d_off..d_off + w * 4].copy_from_slice(&tmp[t_off..t_off + w * 4]);
            }
            dirty.push(ironrdp::pdu::geometry::InclusiveRectangle {
                left: dx as u16,
                top: dy as u16,
                right: (dx + w - 1) as u16,
                bottom: (dy + h - 1) as u16,
            });
        }
        if s.mapped && blit_rects(&self.surfaces, &s.fb, s.width, s.height, s.origin_x, s.origin_y, &dirty) {
            crate::notify_frame();
        }
    }

    fn on_frame_complete(&mut self, _frame_id: u32) {}

    // ── Bitmap cache (MANDATORY per MS-RDPEGFX Client Implementation
    // Requirements). The server saves surface regions to slots and restores them
    // later — cursor save-under on xrdp, scroll/window-move on Windows. ──

    fn on_surface_to_cache(&mut self, pdu: &ironrdp::egfx::pdu::SurfaceToCachePdu) {
        let Some(s) = self.gfx.get(&pdu.surface_id) else { return };
        let sw = usize::from(s.width);
        let sh = usize::from(s.height);
        let r = &pdu.source_rectangle;
        let x0 = usize::from(r.left).min(sw);
        let y0 = usize::from(r.top).min(sh);
        let x1 = usize::from(r.right).min(sw);
        let y1 = usize::from(r.bottom).min(sh);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let w = x1 - x0;
        let h = y1 - y0;
        let mut data = vec![0u8; w * h * 4];
        for row in 0..h {
            let s_off = ((y0 + row) * sw + x0) * 4;
            let d_off = row * w * 4;
            data[d_off..d_off + w * 4].copy_from_slice(&s.fb[s_off..s_off + w * 4]);
        }
        if self.log_cache < 60 {
            self.log_cache += 1;
            log(&format!(
                "[EGFX] SurfaceToCache surf={} slot={} rect=({x0},{y0} {w}x{h})",
                pdu.surface_id, pdu.cache_slot
            ));
        }
        self.cache.insert(
            pdu.cache_slot,
            CacheSlot {
                width: w as u16,
                height: h as u16,
                data,
            },
        );
    }

    fn on_cache_to_surface(&mut self, pdu: &ironrdp::egfx::pdu::CacheToSurfacePdu) {
        let Some(entry) = self.cache.get(&pdu.cache_slot) else {
            log(&format!(
                "[EGFX] CacheToSurface: slot {} is EMPTY (surf={}) — region will be missing",
                pdu.cache_slot, pdu.surface_id
            ));
            return;
        };
        let Some(s) = self.gfx.get_mut(&pdu.surface_id) else { return };
        let sw = usize::from(s.width);
        let sh = usize::from(s.height);
        let cw = usize::from(entry.width);
        let ch = usize::from(entry.height);

        if self.log_cache < 60 {
            self.log_cache += 1;
            log(&format!(
                "[EGFX] CacheToSurface slot={} -> surf={} {}x{} at {:?}",
                pdu.cache_slot,
                pdu.surface_id,
                cw,
                ch,
                pdu.destination_points.first().map(|p| (p.x, p.y))
            ));
        }

        let mut dirty = Vec::with_capacity(pdu.destination_points.len());
        for p in &pdu.destination_points {
            let dx = usize::from(p.x);
            let dy = usize::from(p.y);
            let w = cw.min(sw.saturating_sub(dx));
            let h = ch.min(sh.saturating_sub(dy));
            if w == 0 || h == 0 {
                continue;
            }
            for row in 0..h {
                let c_off = row * cw * 4;
                let d_off = ((dy + row) * sw + dx) * 4;
                s.fb[d_off..d_off + w * 4].copy_from_slice(&entry.data[c_off..c_off + w * 4]);
            }
            dirty.push(ironrdp::pdu::geometry::InclusiveRectangle {
                left: dx as u16,
                top: dy as u16,
                right: (dx + w - 1) as u16,
                bottom: (dy + h - 1) as u16,
            });
        }
        if s.mapped && blit_rects(&self.surfaces, &s.fb, s.width, s.height, s.origin_x, s.origin_y, &dirty) {
            crate::notify_frame();
        }
    }

    fn on_evict_cache_entry(&mut self, pdu: &ironrdp::egfx::pdu::EvictCacheEntryPdu) {
        self.cache.remove(&pdu.cache_slot);
    }

    fn on_unhandled_pdu(&mut self, pdu: &ironrdp::egfx::pdu::GfxPdu) {
        if self.log_other < 30 {
            self.log_other += 1;
            log(&format!("[EGFX] unhandled PDU: {pdu:?}"));
        }
    }

    fn on_close(&mut self) {
        log("[EGFX] graphics channel closed");
    }
}

/// Blit surface-local dirty rectangles to every monitor canvas. Rects are
/// translated into combined-desktop coordinates by the surface's output origin;
/// each canvas clips to its own monitor rect. Returns whether anything painted.
fn blit_rects(
    surfaces: &Rc<RefCell<Vec<Canvas>>>,
    fb: &[u8],
    sw: u16,
    sh: u16,
    ox: u16,
    oy: u16,
    rects: &[ironrdp::pdu::geometry::InclusiveRectangle],
) -> bool {
    let mut painted = false;
    let mut canvases = surfaces.borrow_mut();
    for r in rects {
        let region = ironrdp::pdu::geometry::InclusiveRectangle {
            left: r.left.saturating_add(ox),
            top: r.top.saturating_add(oy),
            right: r.right.saturating_add(ox),
            bottom: r.bottom.saturating_add(oy),
        };
        for canvas in canvases.iter_mut() {
            if canvas.draw_rgba(fb, sw, sh, ox, oy, region.clone()).is_ok() {
                painted = true;
            }
        }
    }
    painted
}
// ===== END EGFX graphics pipeline =====

/// An active RDP session handle exposed to JavaScript.
#[wasm_bindgen]
pub struct Session {
    input_tx: mpsc::UnboundedSender<InputEvent>,
    input_db: ironrdp::input::Database,
    desktop_width: u16,
    desktop_height: u16,
    stats: Rc<RefCell<SessionStats>>,
    /// Render surfaces, one per monitor, shared with the session loop. JS adds
    /// popup-window surfaces here for multi-monitor; the rAF paint iterates them.
    surfaces: Rc<RefCell<Vec<Canvas>>>,
    /// Live video codec, updated by the EGFX handler; surfaced to the HUD.
    video_codec: Rc<std::cell::Cell<VideoCodec>>,
}

/// Bandwidth statistics shared between the session, framed reader, and writer.
#[derive(Default)]
pub(crate) struct SessionStats {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

pub(crate) enum InputEvent {
    FastPath(smallvec::SmallVec<[FastPathInputEvent; 2]>),
    Resize { width: u16, height: u16 },
    /// Apply a new multi-monitor layout to a live session via DisplayControl.
    /// Flat `[left, top, width, height, primary]` per monitor.
    MonitorLayout { monitors: Vec<i32> },
    Cliprdr(ironrdp::cliprdr::backend::ClipboardMessage),
    /// Advertise local files to the remote (Local→Remote file transfer).
    FileCopy(Vec<ironrdp::cliprdr::pdu::FileDescriptor>),
    Terminate,
}

#[wasm_bindgen]
impl Session {
    pub(crate) async fn connect(
        ws_url: String,
        username: String,
        password: String,
        domain: String,
        width: u16,
        height: u16,
        canvas_id: String,
        enable_opus: bool,
        enable_aac: bool,
        monitors: Vec<i32>,
        enable_text_clipboard: bool,
        enable_file_clipboard: bool,
        fps_cap: u32,
        enable_audio: bool,
        enable_font_smoothing: bool,
        disable_cursor_effects: bool,
        allow_wallpaper: bool,
        allow_themes: bool,
        allow_animations: bool,
    ) -> anyhow::Result<Session> {
        log(&format!("Connecting to proxy: {ws_url}"));

        // Multi-monitor: parse the flat layout from JS into GCC monitor rects.
        // Empty ⇒ single-monitor (unchanged legacy behavior). When present, the
        // requested desktop becomes the bounding box of all monitors.
        let monitor_layout = parse_monitor_layout(&monitors);
        let (width, height) = if monitor_layout.is_empty() {
            (width, height)
        } else {
            let (cw, ch) = combined_desktop_size(&monitor_layout);
            log(&format!(
                "Multi-monitor: {} monitors, combined desktop {cw}x{ch}",
                monitor_layout.len()
            ));
            (cw, ch)
        };

        // Rect of the surface backed by the main page canvas. Single-monitor:
        // the whole desktop. Multi-monitor: the primary monitor (JS adds the
        // secondary popup surfaces after connect via `add_surface`).
        let primary_rect = primary_surface_rect(&monitor_layout, width, height);

        // Open WebSocket to the proxy
        let ws = WebSocket::open(&ws_url).context("Failed to open WebSocket")?;

        // Wait for WebSocket to be ready
        loop {
            match ws.state() {
                websocket::State::Closing | websocket::State::Closed => {
                    anyhow::bail!("WebSocket connection failed");
                }
                websocket::State::Connecting => {
                    gloo_timers::future::sleep(std::time::Duration::from_millis(5)).await;
                }
                websocket::State::Open => break,
            }
        }
        log("WebSocket connected to proxy");

        // Advertise the MS-RDPEGFX graphics pipeline so a GPU-less Windows host
        // delivers RFX-Progressive. TODO(Phase 4): gate behind a UI toggle; on
        // during dev so the progressive path is exercised end-to-end. (xrdp
        // ignores the flag and stays on legacy RFX — no effect there.)
        let enable_gfx = true;

        // Build IronRDP connector config
        let config = build_connector_config(
            username.clone(), password.clone(), domain.clone(), width, height,
            monitor_layout, enable_audio, enable_font_smoothing, disable_cursor_effects,
            allow_wallpaper, allow_themes, allow_animations,
            enable_gfx,
        );

        // Split WebSocket for bidirectional I/O
        let (ws_write, ws_read) = ws.split();

        // Create shared stats for bandwidth tracking
        let stats = Rc::new(RefCell::new(SessionStats::default()));

        // Create the framed reader for RDP PDU parsing
        let framed = WasmFramed::new(ws_read, stats.clone());

        // Create connector and perform RDP handshake
        let socket_addr = std::net::SocketAddr::from(([127, 0, 0, 1], 3389));
        
        // Create input channel (must be done before CliprdrBackend to pass tx)
        let (input_tx, input_rx) = mpsc::unbounded();

        // Set up the render surface (the main page canvas) BEFORE wiring the
        // connector, so the EGFX graphics-pipeline handler can share it. Additional
        // monitor surfaces are added by JS via `add_surface` for multi-monitor.
        let (px, py, pw, ph) = primary_rect;
        let canvas = Canvas::new(&canvas_id, px, py, pw, ph)
            .context("Failed to initialize canvas")?;
        let surfaces: Rc<RefCell<Vec<Canvas>>> = Rc::new(RefCell::new(vec![canvas]));

        let cliprdr = ironrdp::cliprdr::Cliprdr::new(Box::new(
            crate::clipboard::WasmCliprdrBackend::new(input_tx.clone(), enable_text_clipboard, enable_file_clipboard)
        ));

        // Only wire the RDPSND channel when the user enabled audio. With it
        // absent, the server sends no audio PDUs and the main loop is free from
        // audio-processing overhead — critical for smooth video playback.
        let connector = ClientConnector::new(config, socket_addr)
            .with_static_channel(cliprdr);
        let connector = if enable_audio {
            let rdpsnd = ironrdp::rdpsnd::client::Rdpsnd::new(
                Box::new(crate::audio::WasmRdpsndHandler::new(enable_opus, enable_aac))
            );
            connector.with_static_channel(rdpsnd)
        } else {
            connector
        };

        // Attach the EGFX graphics pipeline over DRDYNVC when enabled. The matching
        // `config.support_graphics_pipeline` flag tells the server we accept the
        // pipeline; the handler decodes RFX-Progressive and renders to `surfaces`.
        // `egfx_active` flips true on the first GFX surface; run_session then stops
        // the legacy renderer from blitting its (black) DecodedImage over GFX output.
        //
        // Audio rides DRDYNVC too: once a client advertises dynamic channels,
        // modern servers move audio to "AUDIO_PLAYBACK_DVC" instead of the
        // static rdpsnd channel (GNOME Remote Desktop implements only the DVC
        // transport). The static channel above stays registered for servers
        // that never open the dynamic one; whichever the server picks gets the
        // traffic, the other stays silent.
        let egfx_active = Rc::new(std::cell::Cell::new(false));
        let video_codec = Rc::new(std::cell::Cell::new(VideoCodec::Rfx));
        let connector = if enable_gfx || enable_audio {
            let mut drdynvc = ironrdp::dvc::DrdynvcClient::new();
            if enable_gfx {
                log("[EGFX] attaching graphics pipeline DVC (RFX-Progressive)");
                drdynvc = drdynvc.with_dynamic_channel(
                    ironrdp::egfx::client::GraphicsPipelineClient::new(
                        Box::new(WasmGfxHandler::new(surfaces.clone(), egfx_active.clone(), video_codec.clone())),
                        None, // no H.264 decoder — we advertise V8/progressive only
                    ),
                );
            }
            if enable_audio {
                log("[RDPSND] attaching audio DVC (AUDIO_PLAYBACK_DVC)");
                drdynvc = drdynvc.with_dynamic_channel(ironrdp::rdpsnd::client::RdpsndDvcClient::new(
                    Box::new(crate::audio::WasmRdpsndHandler::new(enable_opus, enable_aac)),
                ));
            }
            connector.with_static_channel(drdynvc)
        } else {
            connector
        };

        log("Starting RDP connection sequence...");

        let (connection_result, framed, ws_write) = perform_connection(
            connector, framed, ws_write,
            &username, &password, &domain,
        ).await?;

        let desktop_width = connection_result.desktop_size.width;
        let desktop_height = connection_result.desktop_size.height;

        log(&format!(
            "RDP connected! Desktop: {desktop_width}x{desktop_height}"
        ));

        // Create the writer channel
        let (writer_tx, mut writer_rx) = mpsc::unbounded::<Vec<u8>>();

        // Spawn writer task
        spawn_local({
            let mut ws_write = ws_write;
            let stats = stats.clone();
            async move {
                while let Some(frame) = writer_rx.next().await {
                    use gloo_net::websocket::Message;
                    stats.borrow_mut().tx_bytes += frame.len() as u64;
                    if ws_write.send(Message::Bytes(frame)).await.is_err() {
                        break;
                    }
                }
            }
        });

        // Spawn the main RDP session loop
        spawn_local({
            let writer_tx = writer_tx.clone();
            let surfaces = surfaces.clone();
            async move {
                let reason = match run_session(
                    connection_result,
                    framed,
                    writer_tx,
                    input_rx,
                    surfaces,
                    desktop_width,
                    desktop_height,
                    fps_cap,
                    egfx_active,
                ).await {
                    Ok(tag) => {
                        log("RDP session ended");
                        tag
                    }
                    Err(e) => {
                        log_error(&format!("RDP session error: {e:#}"));
                        "connection_lost"
                    }
                };
                crate::notify_session_ended(reason);
            }
        });

        Ok(Session {
            input_tx,
            input_db: ironrdp::input::Database::new(),
            desktop_width,
            desktop_height,
            stats,
            surfaces,
            video_codec,
        })
    }

    /// Completes a deferred Server Redirection handoff (see `run_session`'s
    /// `ActiveStageOutput::Redirect` handling, which stashes the redirect
    /// packet's fields and returns the `"redirected"` disconnect reason). JS
    /// calls this immediately in response, with the same connection
    /// parameters as the original `connect()` — no username/password/domain,
    /// since those come from the stashed redirect packet instead.
    pub(crate) async fn connect_redirected(
        ws_url: String,
        width: u16,
        height: u16,
        canvas_id: String,
        enable_opus: bool,
        enable_aac: bool,
        monitors: Vec<i32>,
        enable_text_clipboard: bool,
        enable_file_clipboard: bool,
        fps_cap: u32,
        enable_audio: bool,
        enable_font_smoothing: bool,
        disable_cursor_effects: bool,
        allow_wallpaper: bool,
        allow_themes: bool,
        allow_animations: bool,
    ) -> anyhow::Result<Session> {
        let pending = PENDING_REDIRECT.with(|cell| cell.borrow_mut().take())
            .context("connect_redirected called without a pending Server Redirection")?;

        log(&format!("Reconnecting to proxy after redirect: {ws_url}"));

        let monitor_layout = parse_monitor_layout(&monitors);
        let (width, height) = if monitor_layout.is_empty() {
            (width, height)
        } else {
            let (cw, ch) = combined_desktop_size(&monitor_layout);
            (cw, ch)
        };
        let primary_rect = primary_surface_rect(&monitor_layout, width, height);

        let ws = WebSocket::open(&ws_url).context("Failed to open WebSocket")?;
        loop {
            match ws.state() {
                websocket::State::Closing | websocket::State::Closed => {
                    anyhow::bail!("WebSocket connection failed");
                }
                websocket::State::Connecting => {
                    gloo_timers::future::sleep(std::time::Duration::from_millis(5)).await;
                }
                websocket::State::Open => break,
            }
        }
        log("WebSocket connected to proxy");

        let enable_gfx = true;

        // Username/domain come from the redirect packet (UTF-16LE on the
        // wire); password is left empty — RDSTLS already authenticated this
        // connection, and the pre-encrypted blob in the packet can't safely
        // be reused as a cleartext Client Info password.
        let username = redirect::utf16le_to_string(&pending.user_name);
        let domain = redirect::utf16le_to_string(&pending.domain);
        let config = build_connector_config(
            username, String::new(), domain, width, height,
            monitor_layout, enable_audio, enable_font_smoothing, disable_cursor_effects,
            allow_wallpaper, allow_themes, allow_animations,
            enable_gfx,
        );

        let (ws_write, ws_read) = ws.split();
        let stats = Rc::new(RefCell::new(SessionStats::default()));
        let framed = WasmFramed::new(ws_read, stats.clone());
        let socket_addr = std::net::SocketAddr::from(([127, 0, 0, 1], 3389));
        let (input_tx, input_rx) = mpsc::unbounded();

        let (px, py, pw, ph) = primary_rect;
        let canvas = Canvas::new(&canvas_id, px, py, pw, ph)
            .context("Failed to initialize canvas")?;
        let surfaces: Rc<RefCell<Vec<Canvas>>> = Rc::new(RefCell::new(vec![canvas]));

        let cliprdr = ironrdp::cliprdr::Cliprdr::new(Box::new(
            crate::clipboard::WasmCliprdrBackend::new(input_tx.clone(), enable_text_clipboard, enable_file_clipboard)
        ));

        let connector = ClientConnector::new(config, socket_addr)
            .with_static_channel(cliprdr);
        let connector = if enable_audio {
            let rdpsnd = ironrdp::rdpsnd::client::Rdpsnd::new(
                Box::new(crate::audio::WasmRdpsndHandler::new(enable_opus, enable_aac))
            );
            connector.with_static_channel(rdpsnd)
        } else {
            connector
        };

        // DRDYNVC hosts both EGFX and DVC audio — same rationale as connect().
        let egfx_active = Rc::new(std::cell::Cell::new(false));
        let video_codec = Rc::new(std::cell::Cell::new(VideoCodec::Rfx));
        let connector = if enable_gfx || enable_audio {
            let mut drdynvc = ironrdp::dvc::DrdynvcClient::new();
            if enable_gfx {
                drdynvc = drdynvc.with_dynamic_channel(
                    ironrdp::egfx::client::GraphicsPipelineClient::new(
                        Box::new(WasmGfxHandler::new(surfaces.clone(), egfx_active.clone(), video_codec.clone())),
                        None,
                    ),
                );
            }
            if enable_audio {
                drdynvc = drdynvc.with_dynamic_channel(ironrdp::rdpsnd::client::RdpsndDvcClient::new(
                    Box::new(crate::audio::WasmRdpsndHandler::new(enable_opus, enable_aac)),
                ));
            }
            connector.with_static_channel(drdynvc)
        } else {
            connector
        };

        log("[Redirect] starting redirected connection sequence...");

        let (connection_result, framed, ws_write) =
            perform_redirected_connection(connector, framed, ws_write, &pending).await?;

        let desktop_width = connection_result.desktop_size.width;
        let desktop_height = connection_result.desktop_size.height;

        log(&format!("RDP reconnected after redirect! Desktop: {desktop_width}x{desktop_height}"));

        let (writer_tx, mut writer_rx) = mpsc::unbounded::<Vec<u8>>();

        spawn_local({
            let mut ws_write = ws_write;
            let stats = stats.clone();
            async move {
                while let Some(frame) = writer_rx.next().await {
                    use gloo_net::websocket::Message;
                    stats.borrow_mut().tx_bytes += frame.len() as u64;
                    if ws_write.send(Message::Bytes(frame)).await.is_err() {
                        break;
                    }
                }
            }
        });

        spawn_local({
            let writer_tx = writer_tx.clone();
            let surfaces = surfaces.clone();
            async move {
                let reason = match run_session(
                    connection_result,
                    framed,
                    writer_tx,
                    input_rx,
                    surfaces,
                    desktop_width,
                    desktop_height,
                    fps_cap,
                    egfx_active,
                ).await {
                    Ok(tag) => {
                        log("RDP session ended");
                        tag
                    }
                    Err(e) => {
                        log_error(&format!("RDP session error: {e:#}"));
                        "connection_lost"
                    }
                };
                crate::notify_session_ended(reason);
            }
        });

        Ok(Session {
            input_tx,
            input_db: ironrdp::input::Database::new(),
            desktop_width,
            desktop_height,
            stats,
            surfaces,
            video_codec,
        })
    }

    /// Send a keyboard scancode event
    #[wasm_bindgen]
    pub fn send_keyboard(&mut self, scancode: u8, is_pressed: bool, is_extended: bool) {
        let sc = ironrdp::input::Scancode::from_u8(is_extended, scancode);
        let op = if is_pressed {
            ironrdp::input::Operation::KeyPressed(sc)
        } else {
            ironrdp::input::Operation::KeyReleased(sc)
        };

        let events = self.input_db.apply(std::iter::once(op));
        let _ = self.input_tx.unbounded_send(InputEvent::FastPath(events));
    }

    /// Send a mouse move event
    #[wasm_bindgen]
    pub fn send_mouse_move(&mut self, x: u16, y: u16) {
        let op = ironrdp::input::Operation::MouseMove(ironrdp::input::MousePosition { x, y });
        let events = self.input_db.apply(std::iter::once(op));
        let _ = self.input_tx.unbounded_send(InputEvent::FastPath(events));
    }

    /// Send a mouse button event
    #[wasm_bindgen]
    pub fn send_mouse_button(&mut self, button: u8, is_pressed: bool, x: u16, y: u16) {
        let btn = match button {
            0 => ironrdp::input::MouseButton::Left,
            1 => ironrdp::input::MouseButton::Middle,
            2 => ironrdp::input::MouseButton::Right,
            3 => ironrdp::input::MouseButton::X1,
            4 => ironrdp::input::MouseButton::X2,
            _ => return,
        };
        let op = if is_pressed {
            ironrdp::input::Operation::MouseButtonPressed(btn)
        } else {
            ironrdp::input::Operation::MouseButtonReleased(btn)
        };
        let move_op = ironrdp::input::Operation::MouseMove(ironrdp::input::MousePosition { x, y });

        let events = self.input_db.apply([move_op, op].into_iter());
        let _ = self.input_tx.unbounded_send(InputEvent::FastPath(events));
    }

    /// Send a mouse wheel event
    #[wasm_bindgen]
    pub fn send_mouse_wheel(&mut self, horizontal: bool, delta: i16) {
        let rotations = ironrdp::input::WheelRotations {
            is_vertical: !horizontal,
            rotation_units: delta,
        };
        let op = ironrdp::input::Operation::WheelRotations(rotations);
        let events = self.input_db.apply(std::iter::once(op));
        let _ = self.input_tx.unbounded_send(InputEvent::FastPath(events));
    }

    /// Get the desktop width
    #[wasm_bindgen(getter)]
    pub fn width(&self) -> u16 {
        self.desktop_width
    }

    /// Get the desktop height
    #[wasm_bindgen(getter)]
    pub fn height(&self) -> u16 {
        self.desktop_height
    }

    /// Get total bytes received from the RDP server
    #[wasm_bindgen(getter)]
    pub fn rx_bytes(&self) -> f64 {
        self.stats.borrow().rx_bytes as f64
    }

    /// Get total bytes sent to the RDP server
    #[wasm_bindgen(getter)]
    pub fn tx_bytes(&self) -> f64 {
        self.stats.borrow().tx_bytes as f64
    }

    /// Active video codec label for the HUD ("RFX", "RFX-Progressive", "Planar").
    #[wasm_bindgen(getter)]
    pub fn video_codec(&self) -> String {
        self.video_codec.get().as_str().to_string()
    }
    /// Request desktop resize (e.g. on fullscreen change)
    #[wasm_bindgen]
    pub fn resize(&mut self, width: u16, height: u16) {
        self.desktop_width = width;
        self.desktop_height = height;
        let _ = self.input_tx.unbounded_send(InputEvent::Resize { width, height });
    }

    /// Apply a new multi-monitor layout to the live session over DisplayControl
    /// (Windows hosts only). `monitors` is the same flat
    /// `[left, top, width, height, primary]` layout passed to `connect`. After
    /// calling this, JS should rebuild the render surfaces (`clear_surfaces` +
    /// `add_surface`) to match the new combined desktop.
    #[wasm_bindgen]
    pub fn apply_monitor_layout(&mut self, monitors: Vec<i32>) {
        if monitors.is_empty() {
            return;
        }
        let (cw, ch) = combined_desktop_size(&parse_monitor_layout(&monitors));
        self.desktop_width = cw;
        self.desktop_height = ch;
        let _ = self.input_tx.unbounded_send(InputEvent::MonitorLayout { monitors });
    }

    /// Terminate the session
    #[wasm_bindgen]
    pub fn shutdown(&self) {
        let _ = self.input_tx.unbounded_send(InputEvent::Terminate);
    }

    /// Remove all render surfaces. Used before re-declaring the full set on a
    /// multi-monitor (re)layout. The next painted frame is skipped until at
    /// least one surface is added again.
    #[wasm_bindgen]
    pub fn clear_surfaces(&self) {
        self.surfaces.borrow_mut().clear();
    }

    /// Add a render surface backed by `canvas`, covering the combined-desktop
    /// rectangle `[origin_x, origin_x+width) × [origin_y, origin_y+height)`.
    /// For multi-monitor, JS calls this once per monitor (main + popups).
    #[wasm_bindgen]
    pub fn add_surface(
        &self,
        canvas: web_sys::HtmlCanvasElement,
        origin_x: u16,
        origin_y: u16,
        width: u16,
        height: u16,
    ) {
        match crate::canvas::Canvas::from_element(canvas, origin_x, origin_y, width, height) {
            Ok(surface) => self.surfaces.borrow_mut().push(surface),
            Err(e) => log_error(&format!("add_surface failed: {e:#}")),
        }
    }
}

/// Perform the full RDP connection sequence using the ClientConnector state machine.
///
/// The connector drives through states:
/// ConnectionInitiationSendRequest → ConnectionInitiationWaitConfirm → ...
/// → SecurityUpgrade → CredSSP → ... → Connected
///
/// At the SecurityUpgrade state:
///   1. We send a `{"cmd":"tls_upgrade"}` text message to the proxy.
///   2. The proxy upgrades its TCP connection to TLS and returns the server
///      certificate as `{"cmd":"tls_ready","server_cert":"<hex>"}`.
///   3. We store the certificate for CredSSP channel binding.
///
/// At the CredSSP state:
///   1. We use `sspi::credssp::CredSspClient` with NTLM mode.
///   2. Exchange TSRequest PDUs with the RDP server through the proxy's TLS tunnel.
///   3. On success, mark CredSSP as done and continue with BasicSettingsExchange.
/// Tells the proxy to upgrade its TCP connection to TLS and waits for the
/// `tls_ready` response, returning the raw server certificate DER. Shared by
/// the normal connection sequence (`perform_connection`) and the redirected
/// reconnect (`redirect::perform_redirected_connection`), which both upgrade
/// to TLS the same way — only what happens with the cert differs (CredSSP
/// channel-binding SPKI extraction vs. a redirect-packet cert-pin check).
async fn perform_tls_upgrade(
    framed: &mut WasmFramed,
    ws_write: &mut futures_util::stream::SplitSink<WebSocket, gloo_net::websocket::Message>,
) -> anyhow::Result<Vec<u8>> {
    log("Security upgrade — requesting TLS from proxy...");

    // Tell the proxy to upgrade its TCP connection to TLS
    use gloo_net::websocket::Message as WsMsg;
    let cmd = r#"{"cmd":"tls_upgrade"}"#;
    ws_write.send(WsMsg::Text(cmd.to_string()))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to send tls_upgrade: {e}"))?;

    // Wait for the proxy's tls_ready response (arrives as a text WS message)
    let cert_hex = framed.read_text_message().await
        .context("Failed to read tls_ready response")?;

    // Parse the JSON response and extract the raw certificate DER
    let Some(cert_str) = parse_tls_ready(&cert_hex) else {
        log_error(&format!("Unexpected TLS response: {cert_hex}"));
        anyhow::bail!("Invalid tls_ready response from proxy");
    };
    let cert_der = hex_decode(&cert_str)?;
    log(&format!("TLS upgrade complete — cert DER: {} bytes", cert_der.len()));
    Ok(cert_der)
}

async fn perform_connection(
    mut connector: ClientConnector,
    mut framed: WasmFramed,
    ws_write: futures_util::stream::SplitSink<WebSocket, gloo_net::websocket::Message>,
    username: &str,
    password: &str,
    domain: &str,
) -> anyhow::Result<(ConnectionResult, WasmFramed, futures_util::stream::SplitSink<WebSocket, gloo_net::websocket::Message>)> {
    let mut ws_write = ws_write;
    let mut buf = WriteBuf::new();
    let mut server_public_key: Vec<u8> = Vec::new();

    loop {
        // Check if we've reached the Connected state
        if let ClientConnectorState::Connected { .. } = &connector.state {
            let state = mem::replace(&mut connector.state, ClientConnectorState::Consumed);
            if let ClientConnectorState::Connected { result } = state {
                return Ok((result, framed, ws_write));
            }
            unreachable!();
        }

        // Handle TLS security upgrade
        if connector.should_perform_security_upgrade() {
            let cert_der = perform_tls_upgrade(&mut framed, &mut ws_write).await?;

            // Extract the SubjectPublicKeyInfo from the X.509 certificate.
            // CredSSP uses the SPKI (not the full cert) for channel binding.
            server_public_key = extract_public_key(&cert_der)?;
            log(&format!(
                "Extracted SubjectPublicKeyInfo: {} bytes",
                server_public_key.len()
            ));

            connector.mark_security_upgrade_as_done();
            continue;
        }

        // Handle CredSSP authentication
        if connector.should_perform_credssp() {
            if server_public_key.is_empty() {
                log("CredSSP: no server cert available, skipping");
                connector.mark_credssp_as_done();
                continue;
            }

            log("CredSSP: starting NTLM authentication...");

            let hybrid_ex = format!("{:?}", connector.state).contains("HYBRID_EX");
            if hybrid_ex {
                log("CredSSP: HYBRID_EX negotiated");
            }

            match perform_credssp(
                &server_public_key,
                username,
                password,
                domain,
                &mut framed,
                &mut ws_write,
                hybrid_ex,
            ).await {
                Ok(()) => {
                    log("CredSSP: authentication successful!");
                }
                Err(e) => {
                    // The server required NLA (we're in should_perform_credssp),
                    // so a CredSSP failure is fatal — continuing only produces a
                    // confusing downstream "connection closed" error. Propagate the
                    // real cause (it carries STATUS_LOGON_FAILURE / 0xc000006d for
                    // bad credentials) so the UI can show "check username/password".
                    log_error(&format!("CredSSP failed: {e:#}"));
                    return Err(e.context("CredSSP authentication failed"));
                }
            }

            connector.mark_credssp_as_done();
            continue;
        }

        let state_name = connector.state.name();
        log(&format!("Connector state: {state_name}"));

        match connector.next_pdu_hint() {
            Some(hint) => {
                let pdu = framed.read_by_hint(hint).await
                    .context("Failed to read PDU from server")?;

                log(&format!("  Received {} bytes from server", pdu.len()));

                let written = connector.step(&pdu, &mut buf)
                    .context("Connector step failed")?;

                if !written.is_nothing() {
                    let data = buf.filled().to_vec();
                    if !data.is_empty() {
                        log(&format!("  Sending {} bytes to server", data.len()));
                        use gloo_net::websocket::Message;
                        ws_write.send(Message::Bytes(data)).await
                            .map_err(|e| anyhow::anyhow!("WebSocket send: {e}"))?;
                    }
                    buf.clear();
                }
            }
            None => {
                let written = connector.step_no_input(&mut buf)
                    .context("Connector step_no_input failed")?;

                if !written.is_nothing() {
                    let data = buf.filled().to_vec();
                    if !data.is_empty() {
                        log(&format!("  Sending {} bytes to server", data.len()));
                        use gloo_net::websocket::Message;
                        ws_write.send(Message::Bytes(data)).await
                            .map_err(|e| anyhow::anyhow!("WebSocket send: {e}"))?;
                    }
                    buf.clear();
                }
            }
        }
    }
}

/// Completes a deferred RDP Server Redirection handoff (MS-RDPBCGR §2.2.13 /
/// §2.2.17 RDSTLS) on a freshly-opened connection: hand-rolled X.224
/// negotiation with the redirect's routing token, TLS upgrade + a best-effort
/// certificate pin check, then the RDSTLS Capabilities/AuthRequest/AuthResponse
/// exchange. RDSTLS replaces nego's own CredSSP phase, so once it succeeds we
/// jump the connector's state machine straight to where a normal connection
/// sits right after CredSSP and hand off to the ordinary `perform_connection`
/// loop (unchanged) for MCS/capability exchange/finalization.
async fn perform_redirected_connection(
    mut connector: ClientConnector,
    mut framed: WasmFramed,
    mut ws_write: futures_util::stream::SplitSink<WebSocket, gloo_net::websocket::Message>,
    pending: &redirect::PendingRedirect,
) -> anyhow::Result<(ConnectionResult, WasmFramed, futures_util::stream::SplitSink<WebSocket, gloo_net::websocket::Message>)> {
    use ironrdp::pdu::nego;
    use ironrdp::pdu::x224::X224;

    // 1. X.224 ConnectionRequest carrying the redirect's routing token instead
    // of the usual username cookie, requesting RDSTLS.
    let security_protocol = nego::SecurityProtocol::SSL | nego::SecurityProtocol::RDSTLS;
    // LoadBalanceInfo arrives as the complete routing-token line
    // ("Cookie: msts=<value>\r\n"), but IronRDP's RoutingToken encoder
    // re-adds the prefix and CRLF itself. Pass only the bare value: grd
    // peeks this line off the socket, strips the prefix, and parses the
    // value as a base-10 u32 to look up the pending handover session —
    // anything else makes it drop the connection on the spot.
    let nego_data = pending.routing_token.as_ref().map(|token| {
        let line = String::from_utf8_lossy(token);
        let value = line.trim_end_matches(['\r', '\n']);
        let value = value.strip_prefix("Cookie: msts=").unwrap_or(value);
        nego::NegoRequestData::routing_token(value.to_owned())
    });
    let connection_request = nego::ConnectionRequest {
        nego_data,
        flags: nego::RequestFlags::empty(),
        protocol: security_protocol,
    };
    let mut buf = WriteBuf::new();
    ironrdp_core::encode_buf(&X224(connection_request), &mut buf)
        .map_err(|e| anyhow::anyhow!("Failed to encode redirected ConnectionRequest: {e}"))?;
    log(&format!(
        "[Redirect] sending X.224 ConnectionRequest ({} bytes, routing token: {})",
        buf.filled().len(),
        pending.routing_token.is_some()
    ));
    ws_write
        .send(gloo_net::websocket::Message::Bytes(buf.filled().to_vec()))
        .await
        .map_err(|e| anyhow::anyhow!("WebSocket send failed: {e}"))?;

    // 2. Read the ConnectionConfirm and confirm RDSTLS was selected.
    let confirm_bytes = framed
        .read_by_hint(&ironrdp::pdu::X224_HINT)
        .await
        .context("Failed to read redirected ConnectionConfirm")?;
    let confirm = ironrdp_core::decode::<X224<nego::ConnectionConfirm>>(&confirm_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to decode ConnectionConfirm: {e}"))?
        .0;
    let selected_protocol = match confirm {
        nego::ConnectionConfirm::Response { protocol, .. } => protocol,
        nego::ConnectionConfirm::Failure { code } => {
            anyhow::bail!("Redirected connection negotiation failed: {code:?}");
        }
    };
    log(&format!("[Redirect] server selected protocol: {selected_protocol:?}"));
    if !selected_protocol.contains(nego::SecurityProtocol::RDSTLS) {
        anyhow::bail!("Redirected server did not select RDSTLS (selected {selected_protocol:?})");
    }

    // 3. TLS upgrade (same proxy dance as a normal connection) + a best-effort
    // certificate pin check against the redirect packet's TargetCertificate.
    let cert_der = perform_tls_upgrade(&mut framed, &mut ws_write).await?;
    redirect::check_cert_pin(pending.target_certificate.as_deref(), &cert_der);

    // 4. RDSTLS: Capabilities (server->client) -> Authentication Request
    // (client->server, forwarding the redirect packet's opaque credentials
    // verbatim) -> Authentication Response (server->client).
    let caps_bytes = framed
        .read_exact(8)
        .await
        .context("Failed to read RDSTLS Capabilities PDU")?;
    redirect::parse_capabilities(&caps_bytes)?;
    log("[Redirect] RDSTLS capabilities OK (version 1)");

    let auth_req = redirect::build_auth_request(
        &pending.redirection_guid,
        &pending.user_name,
        &pending.domain,
        &pending.password,
    );
    log(&format!(
        "[Redirect] sending RDSTLS Authentication Request ({} bytes)",
        auth_req.len()
    ));
    ws_write
        .send(gloo_net::websocket::Message::Bytes(auth_req))
        .await
        .map_err(|e| anyhow::anyhow!("WebSocket send failed: {e}"))?;

    let response_bytes = framed
        .read_exact(10)
        .await
        .context("Failed to read RDSTLS Authentication Response PDU")?;
    redirect::parse_auth_response(&response_bytes)?;
    log("[Redirect] RDSTLS authentication succeeded");

    // 5. Hand off to the ordinary post-CredSSP flow. Username/password/domain
    // are unused by `perform_connection` once `should_perform_credssp()` can
    // never be true again (we jumped past that state).
    connector.state = ClientConnectorState::BasicSettingsExchangeSendInitial { selected_protocol };
    perform_connection(connector, framed, ws_write, "", "", "").await
}

async fn perform_credssp(
    server_public_key: &[u8],
    username: &str,
    password: &str,
    domain: &str,
    framed: &mut WasmFramed,
    ws_write: &mut futures_util::stream::SplitSink<WebSocket, gloo_net::websocket::Message>,
    hybrid_ex: bool,
) -> anyhow::Result<()> {
    use sspi::credssp::{CredSspClient, CredSspMode, ClientState, ClientMode};
    use sspi::credssp::TsRequest;
    use sspi::ntlm::NtlmConfig;
    use sspi::generator::GeneratorState;

    // Build NTLM credentials using sspi types
    let sspi_username = sspi::Username::new(
        username,
        if domain.is_empty() { None } else { Some(domain) },
    ).map_err(|e| anyhow::anyhow!("Invalid username: {e}"))?;

    let credentials = sspi::Credentials::AuthIdentity(sspi::AuthIdentity {
        username: sspi_username,
        password: password.to_string().into(),
    });

    let spn = format!("TERMSRV/{}", "localhost");

    let mut credssp_client = CredSspClient::new(
        server_public_key.to_vec(),
        credentials,
        CredSspMode::WithCredentials,
        ClientMode::Ntlm(NtlmConfig::default()),
        spn,
    ).map_err(|e| anyhow::anyhow!("CredSSP init failed: {e}"))?;

    // First round: start with an empty TsRequest
    let mut ts_request = TsRequest::default();
    let mut round = 0;

    loop {
        round += 1;
        log(&format!("CredSSP round {round}"));

        // Process current ts_request through the CredSSP state machine
        let mut generator = credssp_client.process(ts_request);
        let client_state = match generator.start() {
            GeneratorState::Completed(result) => {
                result.map_err(|e| anyhow::anyhow!("CredSSP process error: {e}"))?
            }
            GeneratorState::Suspended(_network_request) => {
                // NTLM doesn't need network requests (only Kerberos does).
                // If we get here, it's unexpected for our NTLM-only flow.
                anyhow::bail!("CredSSP: unexpected network request (Kerberos not supported)");
            }
        };

        match client_state {
            ClientState::ReplyNeeded(reply_ts_request) => {
                // Encode and send the TSRequest to the RDP server
                let mut encoded = Vec::new();
                reply_ts_request.encode_ts_request(&mut encoded)
                    .map_err(|e| anyhow::anyhow!("TSRequest encode failed: {e}"))?;

                log(&format!("CredSSP: sending {} bytes", encoded.len()));
                use gloo_net::websocket::Message;
                ws_write.send(Message::Bytes(encoded)).await
                    .map_err(|e| anyhow::anyhow!("WebSocket send: {e}"))?;

                // Read the server's response TSRequest
                let response_data = framed.read_credssp_response().await
                    .context("Failed to read CredSSP response")?;

                log(&format!("CredSSP: received {} bytes", response_data.len()));

                ts_request = TsRequest::from_buffer(&response_data)
                    .map_err(|e| anyhow::anyhow!("TSRequest decode failed: {e}"))?;
            }
            ClientState::FinalMessage(final_ts_request) => {
                // Encode and send the final TSRequest (with auth_info)
                let mut encoded = Vec::new();
                final_ts_request.encode_ts_request(&mut encoded)
                    .map_err(|e| anyhow::anyhow!("Final TSRequest encode failed: {e}"))?;

                log(&format!("CredSSP: sending final message ({} bytes)", encoded.len()));
                use gloo_net::websocket::Message;
                ws_write.send(Message::Bytes(encoded)).await
                    .map_err(|e| anyhow::anyhow!("WebSocket send: {e}"))?;

                if hybrid_ex {
                    log("CredSSP: waiting for EarlyUserAuthResult (HYBRID_EX)");
                    let auth_result = framed.read_exact(4).await?;
                    log(&format!("CredSSP EarlyUserAuthResult: {:02X?}", auth_result));
                    if auth_result != [0, 0, 0, 0] {
                        anyhow::bail!("CredSSP EarlyUserAuthResult denied/invalid: {:?}", auth_result);
                    }
                }

                log("CredSSP: handshake complete");
                return Ok(());
            }
        }

        if round > 10 {
            anyhow::bail!("CredSSP: too many rounds, aborting");
        }
    }
}

/// Parse `{"cmd":"tls_ready","server_cert":"<hex>"}` and return the hex string.
fn parse_tls_ready(json_str: &str) -> Option<String> {
    // Simple JSON parsing without serde (avoid adding serde to WASM)
    if !json_str.contains("tls_ready") {
        return None;
    }
    // Find "server_cert":"..."
    let marker = "\"server_cert\":\"";
    let start = json_str.find(marker)? + marker.len();
    let end = json_str[start..].find('"')? + start;
    Some(json_str[start..end].to_string())
}

/// Decode a hex string to bytes.
fn hex_decode(hex: &str) -> anyhow::Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        anyhow::bail!("Odd-length hex string");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| anyhow::anyhow!("Invalid hex: {e}"))
        })
        .collect()
}

/// Extract the raw public key bytes from an X.509 certificate.
///
/// CredSSP channel binding requires the raw key (content of the BIT STRING
/// inside SubjectPublicKeyInfo), matching FreeRDP's `i2d_PublicKey()`.
/// For RSA this is `SEQUENCE { modulus INTEGER, exponent INTEGER }`.
fn extract_public_key(cert_der: &[u8]) -> anyhow::Result<Vec<u8>> {
    use x509_cert::Certificate;
    use x509_cert::der::Decode;

    let cert = Certificate::from_der(cert_der)
        .map_err(|e| anyhow::anyhow!("Failed to parse X.509 certificate: {e}"))?;

    let spki = &cert.tbs_certificate.subject_public_key_info;
    let raw_key = spki.subject_public_key
        .as_bytes()
        .ok_or_else(|| anyhow::anyhow!("SubjectPublicKey has unused bits"))?;

    Ok(raw_key.to_vec())
}

thread_local! {
    /// Set when `run_session` receives a deferred Server Redirection PDU (e.g.
    /// GNOME Remote Desktop's headless "Remote Login" handoff). JS reads the
    /// `redirected` disconnect reason and immediately calls `connect_redirected`,
    /// which consumes this to complete the reconnect. wasm32 is single-threaded
    /// (see `unsafe impl Send for WasmGfxHandler` above), so a thread-local is
    /// safe here without extra synchronization.
    static PENDING_REDIRECT: RefCell<Option<crate::redirect::PendingRedirect>> = RefCell::new(None);
}

async fn run_session(
    connection_result: ConnectionResult,
    mut framed: WasmFramed,
    writer_tx: mpsc::UnboundedSender<Vec<u8>>,
    mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
    surfaces: Rc<RefCell<Vec<Canvas>>>,
    width: u16,
    height: u16,
    _fps_cap: u32,
    egfx_active: Rc<std::cell::Cell<bool>>,
) -> anyhow::Result<&'static str> {
    let image = Rc::new(RefCell::new(DecodedImage::new(PixelFormat::RgbA32, width, height)));
    let mut active_stage = ActiveStage::new(connection_result);
    // Diagnostic: count legacy (non-EGFX) GraphicsUpdate paints. When EGFX owns
    // the surface these should be ZERO — any non-zero here means the legacy path
    // is blitting its (mostly-black) DecodedImage over the EGFX content.
    let mut legacy_paints: u32 = 0;

    loop {
        let outputs = select! {
            frame = framed.read_pdu().fuse() => {
                let (action, payload) = frame.context("read RDP PDU")?;
                match active_stage.process(&mut image.borrow_mut(), action, payload.as_ref()) {
                    Ok(outputs) => outputs,
                    Err(e) => {
                        // ponytail: temporary diagnostic to identify unhandled PDUs
                        // (e.g. server redirection) by their raw bytes; remove once found.
                        let hex: String = payload.iter().take(64).map(|b| format!("{b:02x}")).collect();
                        log_error(&format!(
                            "Ignoring PDU processing error: {e:#} | action={action:?} len={} bytes={hex}",
                            payload.len()
                        ));
                        Vec::new()
                    }
                }
            }
            event = input_rx.next().fuse() => {
                match event {
                    Some(InputEvent::FastPath(events)) => {
                        active_stage.process_fastpath_input(&mut image.borrow_mut(), &events)
                            .context("process input")?
                    }
                    Some(InputEvent::Resize { width: new_w, height: new_h }) => {
                        log(&format!("Resize requested: {new_w}x{new_h}"));
                        match active_stage.encode_resize(u32::from(new_w), u32::from(new_h), None, None) {
                            Some(Ok(resize_frame)) => {
                                vec![ActiveStageOutput::ResponseFrame(resize_frame)]
                            }
                            Some(Err(e)) => {
                                log_error(&format!("Resize failed: {e}"));
                                Vec::new()
                            }
                            None => {
                                log("Resize: displaycontrol not available");
                                Vec::new()
                            }
                        }
                    }
                    Some(InputEvent::MonitorLayout { monitors }) => {
                        let entries = parse_dc_monitor_layout(&monitors);
                        if entries.is_empty() {
                            log_error("MonitorLayout: no valid monitors after validation");
                            Vec::new()
                        } else {
                            // Resize our framebuffer to the new combined desktop
                            // before the server starts sending updates for it.
                            let (nw, nh) = combined_desktop_size(&parse_monitor_layout(&monitors));
                            log(&format!("MonitorLayout: {} monitors, combined {nw}x{nh}", entries.len()));
                            *image.borrow_mut() = DecodedImage::new(PixelFormat::RgbA32, nw, nh);
                            match active_stage.encode_monitor_layout(&entries) {
                                Some(Ok(frame)) => vec![ActiveStageOutput::ResponseFrame(frame)],
                                Some(Err(e)) => {
                                    log_error(&format!("MonitorLayout encode failed: {e}"));
                                    Vec::new()
                                }
                                None => {
                                    log("MonitorLayout: displaycontrol not available");
                                    Vec::new()
                                }
                            }
                        }
                    }
                    Some(InputEvent::Cliprdr(message)) => {
                        if let Some(cliprdr) = active_stage.get_svc_processor_mut::<ironrdp::cliprdr::CliprdrClient>() {
                            if let Some(svc_messages) = match message {
                                ironrdp::cliprdr::backend::ClipboardMessage::SendInitiateCopy(formats) => {
                                    match cliprdr.initiate_copy(&formats) {
                                        Ok(msgs) => Some(msgs),
                                        Err(e) => { log_error(&format!("Cliprdr copy error: {e}")); None }
                                    }
                                }
                                ironrdp::cliprdr::backend::ClipboardMessage::SendFormatData(response) => {
                                    match cliprdr.submit_format_data(response) {
                                        Ok(msgs) => Some(msgs),
                                        Err(e) => { log_error(&format!("Cliprdr format data error: {e}")); None }
                                    }
                                }
                                ironrdp::cliprdr::backend::ClipboardMessage::SendInitiatePaste(format) => {
                                    match cliprdr.initiate_paste(format) {
                                        Ok(msgs) => Some(msgs),
                                        Err(e) => { log_error(&format!("Cliprdr paste error: {e}")); None }
                                    }
                                }
                                ironrdp::cliprdr::backend::ClipboardMessage::SendFileContentsRequest(req) => {
                                    match cliprdr.request_file_contents(req) {
                                        Ok(msgs) => Some(msgs),
                                        Err(e) => { log_error(&format!("Cliprdr file contents request error: {e}")); None }
                                    }
                                }
                                ironrdp::cliprdr::backend::ClipboardMessage::SendFileContentsResponse(resp) => {
                                    match cliprdr.submit_file_contents(resp) {
                                        Ok(msgs) => Some(msgs),
                                        Err(e) => { log_error(&format!("Cliprdr file contents response error: {e}")); None }
                                    }
                                }
                                ironrdp::cliprdr::backend::ClipboardMessage::Error(e) => {
                                    log_error(&format!("Clipboard backend error: {e}"));
                                    None
                                }
                            } {
                                let frame = active_stage.process_svc_processor_messages(svc_messages)
                                    .context("encode cliprdr SVC messages")?;
                                vec![ActiveStageOutput::ResponseFrame(frame)]
                            } else {
                                Vec::new()
                            }
                        } else {
                            log_error("Clipboard event received but Cliprdr is not available");
                            Vec::new()
                        }
                    }
                    Some(InputEvent::FileCopy(files)) => {
                        if let Some(cliprdr) = active_stage.get_svc_processor_mut::<ironrdp::cliprdr::CliprdrClient>() {
                            match cliprdr.initiate_file_copy(files) {
                                Ok(msgs) => {
                                    let frame = active_stage.process_svc_processor_messages(msgs)
                                        .context("encode cliprdr file copy messages")?;
                                    vec![ActiveStageOutput::ResponseFrame(frame)]
                                }
                                Err(e) => {
                                    log_error(&format!("Cliprdr initiate_file_copy error: {e}"));
                                    Vec::new()
                                }
                            }
                        } else {
                            log_error("FileCopy event received but Cliprdr is not available");
                            Vec::new()
                        }
                    }
                    Some(InputEvent::Terminate) => {
                        active_stage.graceful_shutdown()
                            .context("graceful shutdown")?
                    }
                    None => break,
                }
            }
        };

        for out in outputs {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => {
                    writer_tx.unbounded_send(frame)
                        .context("send response frame")?;
                }
                ActiveStageOutput::GraphicsUpdate(region) => {
                    // Once EGFX owns rendering, the legacy DecodedImage is black
                    // (real content never reaches it) — blitting it would stamp
                    // black rectangles over the GFX output. Suppress, but log so
                    // any server that still mixes legacy updates is visible.
                    if egfx_active.get() {
                        if legacy_paints < 20 {
                            log(&format!(
                                "[LEGACY] GraphicsUpdate region=({},{},{},{}) SUPPRESSED (EGFX active)",
                                region.left, region.top, region.right, region.bottom
                            ));
                        }
                        legacy_paints = legacy_paints.wrapping_add(1);
                        continue;
                    }
                    let img = image.borrow();
                    let mut painted = false;
                    for surface in surfaces.borrow_mut().iter_mut() {
                        if surface.draw(&img, region.clone()).is_ok() {
                            painted = true;
                        }
                    }
                    if painted {
                        crate::notify_frame();
                    }
                }
                ActiveStageOutput::PointerDefault => {
                    for surface in surfaces.borrow().iter() {
                        surface.set_cursor("default");
                    }
                }
                ActiveStageOutput::PointerHidden => {
                    for surface in surfaces.borrow().iter() {
                        surface.set_cursor("none");
                    }
                }
                ActiveStageOutput::PointerBitmap(pointer) => {
                    for surface in surfaces.borrow_mut().iter_mut() {
                        surface.set_custom_cursor(
                            &pointer.bitmap_data,
                            pointer.width as u32,
                            pointer.height as u32,
                            pointer.hotspot_x as u32,
                            pointer.hotspot_y as u32,
                        );
                    }
                }
                ActiveStageOutput::Terminate(reason) => {
                    use ironrdp::session::GracefulDisconnectReason as Gdr;
                    log(&format!("RDP session terminated by server: {reason}"));
                    // Classify so JS can decide whether to reconnect. A server-
                    // initiated disconnect — logoff, admin action, or (critically)
                    // eviction by a NEW connection to the same single-session host —
                    // must NOT trigger reconnect: reconnecting evicts whoever
                    // connected after us, and the two clients ping-pong, spiking the
                    // connection count until the host falls over.
                    let tag = match &reason {
                        Gdr::Other(desc) if desc.contains("Another user connected") => "session_replaced",
                        Gdr::UserInitiated => "user_disconnect",
                        _ => "server_disconnect",
                    };
                    return Ok(tag);
                }
                ActiveStageOutput::Redirect(packet) => {
                    log(&format!(
                        "[Redirect] Server Redirection PDU received: session_id={} flags={:?} \
                         has_routing_token={} has_username={} has_password={} has_cert={}",
                        packet.session_id,
                        packet.flags,
                        packet.load_balance_info.is_some(),
                        packet.user_name.is_some(),
                        packet.password.is_some(),
                        packet.target_certificate.is_some(),
                    ));
                    let pending = crate::redirect::PendingRedirect::from_packet(&packet);
                    PENDING_REDIRECT.with(|cell| *cell.borrow_mut() = Some(pending));
                    return Ok("redirected");
                }
                _ => {}
            }
        }
    }

    // Reached when the input channel closes (Session handle dropped) — an app-side
    // teardown, treated like a user disconnect (no reconnect).
    Ok("user_disconnect")
}

/// Parse the flat monitor array from JS (`[left, top, width, height, primary]`
/// per monitor, in combined-desktop pixels, primary at the origin) into GCC
/// monitor rects. RDP monitor edges are **inclusive**, so a 1920-wide monitor at
/// x=0 spans `left=0..=right=1919`. An empty input yields an empty layout
/// (single-monitor / legacy path).
fn parse_monitor_layout(flat: &[i32]) -> Vec<gcc::Monitor> {
    flat.chunks_exact(5)
        .map(|c| {
            let (left, top, w, h, primary) = (c[0], c[1], c[2], c[3], c[4]);
            gcc::Monitor {
                left,
                top,
                right: left + w - 1,
                bottom: top + h - 1,
                flags: if primary != 0 {
                    gcc::MonitorFlags::PRIMARY
                } else {
                    gcc::MonitorFlags::empty()
                },
            }
        })
        .collect()
}

/// Combined desktop size = bounding box of all monitors. Edges are inclusive, so
/// the size is `max(right) + 1` by `max(bottom) + 1`. Assumes a normalized,
/// non-negative layout (origin at 0,0) as produced by the JS side.
fn combined_desktop_size(monitors: &[gcc::Monitor]) -> (u16, u16) {
    let right = monitors.iter().map(|m| m.right).max().unwrap_or(0);
    let bottom = monitors.iter().map(|m| m.bottom).max().unwrap_or(0);
    let w = (right + 1).clamp(1, u16::MAX as i32) as u16;
    let h = (bottom + 1).clamp(1, u16::MAX as i32) as u16;
    (w, h)
}

/// Parse the flat monitor array into DisplayControl `MonitorLayoutEntry` values
/// for a live (re)layout. Widths are adjusted to the protocol's constraints
/// (even, 200..=8192); the primary must be at (0,0) (the JS side normalizes it
/// so). Invalid monitors are skipped.
fn parse_dc_monitor_layout(flat: &[i32]) -> Vec<ironrdp::displaycontrol::pdu::MonitorLayoutEntry> {
    use ironrdp::displaycontrol::pdu::MonitorLayoutEntry;
    flat.chunks_exact(5)
        .filter_map(|c| {
            let (left, top, w, h, primary) = (c[0], c[1], c[2], c[3], c[4]);
            let (w, h) = MonitorLayoutEntry::adjust_display_size(w.max(0) as u32, h.max(0) as u32);
            let entry = if primary != 0 {
                MonitorLayoutEntry::new_primary(w, h)
            } else {
                MonitorLayoutEntry::new_secondary(w, h)
            }
            .ok()?;
            entry.with_position(left, top).ok()
        })
        .collect()
}

/// Rect `(origin_x, origin_y, width, height)` of the surface backed by the main
/// page canvas. Single-monitor (empty layout): the whole desktop. Multi-monitor:
/// the primary monitor (the one flagged PRIMARY, else the first).
fn primary_surface_rect(monitors: &[gcc::Monitor], combined_w: u16, combined_h: u16) -> (u16, u16, u16, u16) {
    let Some(m) = monitors
        .iter()
        .find(|m| m.flags.contains(gcc::MonitorFlags::PRIMARY))
        .or_else(|| monitors.first())
    else {
        return (0, 0, combined_w, combined_h);
    };
    let ox = m.left.max(0).min(i32::from(u16::MAX)) as u16;
    let oy = m.top.max(0).min(i32::from(u16::MAX)) as u16;
    let w = (m.right - m.left + 1).clamp(1, i32::from(u16::MAX)) as u16;
    let h = (m.bottom - m.top + 1).clamp(1, i32::from(u16::MAX)) as u16;
    (ox, oy, w, h)
}

fn build_connector_config(
    username: String,
    password: String,
    domain: String,
    width: u16,
    height: u16,
    monitors: Vec<gcc::Monitor>,
    enable_audio: bool,
    enable_font_smoothing: bool,
    disable_cursor_effects: bool,
    allow_wallpaper: bool,
    allow_themes: bool,
    allow_animations: bool,
    enable_gfx: bool,
) -> connector::Config {
    let domain = if domain.is_empty() { None } else { Some(domain) };

    // Full-window-drag always off (minor bandwidth, no UX value in a web client).
    // Wallpaper/themes/animations are user-controlled; each adds significant bandwidth.
    // ENABLE_FONT_SMOOTHING: opt-in ClearType.
    // DISABLE_CURSORSETTINGS: removes cursor shadow/blink, minor bandwidth win.
    let mut perf = PerformanceFlags::default()
        | PerformanceFlags::DISABLE_FULLWINDOWDRAG;
    if !allow_wallpaper  { perf |= PerformanceFlags::DISABLE_WALLPAPER; }
    if !allow_themes     { perf |= PerformanceFlags::DISABLE_THEMING; }
    if !allow_animations { perf |= PerformanceFlags::DISABLE_MENUANIMATIONS; }
    if enable_font_smoothing  { perf |= PerformanceFlags::ENABLE_FONT_SMOOTHING; }
    if disable_cursor_effects { perf |= PerformanceFlags::DISABLE_CURSORSETTINGS; }

    connector::Config {
        credentials: Credentials::UsernamePassword { username, password },
        domain,
        enable_tls: true,
        enable_credssp: true,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: connector::DesktopSize { width, height },
        bitmap: None,
        client_build: 0,
        client_name: "web-rdp-rust".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: MajorPlatformType::UNSPECIFIED,
        enable_server_pointer: true,
        request_data: None,
        autologon: true,
        enable_audio_playback: enable_audio,
        // Render the cursor as an accelerated overlay (PointerBitmap → CSS cursor),
        // NOT composited into the legacy DecodedImage. Software rendering draws the
        // pointer into that framebuffer — which is black under EGFX — and emits it
        // as a GraphicsUpdate (the original black cursor trail). As an overlay the
        // browser positions a crisp, transparent cursor and the legacy path stays
        // out of EGFX rendering entirely.
        pointer_software_rendering: false,
        performance_flags: perf,
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        compression_type: None,
        multitransport_flags: None,
        monitors,
        monitors_extended: Vec::new(),
        // Advertise MS-RDPEGFX so a capable host opens the graphics DVC and
        // delivers RFX-Progressive (GPU-less Windows) / H.264. We attach a GFX
        // handler in run_session; without it a GFX-capable host would blank.
        support_graphics_pipeline: enable_gfx,
    }
}

impl Session {
    /// Expose a way for the clipboard module to send cliprdr messages to the event loop.
    pub(crate) fn send_cliprdr_message(&self, message: ironrdp::cliprdr::backend::ClipboardMessage) {
        let _ = self.input_tx.unbounded_send(InputEvent::Cliprdr(message));
    }

    /// Advertise local files to the remote via CLIPRDR (Local→Remote file transfer).
    pub(crate) fn send_file_copy(&self, files: Vec<ironrdp::cliprdr::pdu::FileDescriptor>) {
        let _ = self.input_tx.unbounded_send(InputEvent::FileCopy(files));
    }
}

