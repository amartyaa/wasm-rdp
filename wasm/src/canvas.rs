use anyhow::Context as _;
use ironrdp::session::image::DecodedImage;
use wasm_bindgen::prelude::*;
use wasm_bindgen::Clamped;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};

/// Canvas renderer for one monitor: writes RGBA pixels directly via putImageData
/// (no re-encoding).
///
/// Each `Canvas` covers a rectangle `[origin_x, origin_x+width) × [origin_y,
/// origin_y+height)` of the **combined** desktop framebuffer. For a single
/// monitor this is the whole framebuffer at origin (0,0); for multi-monitor each
/// surface maps to one physical monitor's sub-rectangle.
pub(crate) struct Canvas {
    ctx: CanvasRenderingContext2d,
    canvas: HtmlCanvasElement,
    /// This surface's position within the combined desktop.
    origin_x: u16,
    origin_y: u16,
    width: u16,
    height: u16,
    /// Persistent scratch buffer for dirty-region extraction, reused across frames.
    rgba_buf: Vec<u8>,
}

impl Canvas {
    /// Build a surface from a canvas element id (used for the main page canvas).
    pub fn new(
        canvas_id: &str,
        origin_x: u16,
        origin_y: u16,
        width: u16,
        height: u16,
    ) -> anyhow::Result<Self> {
        let window = web_sys::window().context("no window")?;
        let document = window.document().context("no document")?;
        let element = document
            .get_element_by_id(canvas_id)
            .context("canvas element not found")?;
        let canvas: HtmlCanvasElement = element
            .dyn_into()
            .map_err(|_| anyhow::anyhow!("element is not a canvas"))?;
        Self::from_element(canvas, origin_x, origin_y, width, height)
    }

    /// Build a surface from a canvas element directly (used for popup-window
    /// canvases that live in another document and can't be looked up by id here).
    pub fn from_element(
        canvas: HtmlCanvasElement,
        origin_x: u16,
        origin_y: u16,
        width: u16,
        height: u16,
    ) -> anyhow::Result<Self> {
        canvas.set_width(u32::from(width));
        canvas.set_height(u32::from(height));

        // `unchecked_into` (not `dyn_into`): a popup-window canvas belongs to a
        // different JS realm, so an `instanceof CanvasRenderingContext2d` check
        // against the main realm's constructor fails. getContext("2d") is
        // guaranteed to return a 2D context; the method calls on it work across
        // same-origin realms.
        let ctx: CanvasRenderingContext2d = canvas
            .get_context("2d")
            .map_err(|_| anyhow::anyhow!("get_context failed"))?
            .context("no 2d context")?
            .unchecked_into();

        // Disable image smoothing for crisp pixel rendering
        ctx.set_image_smoothing_enabled(false);

        Ok(Self {
            ctx,
            canvas,
            origin_x,
            origin_y,
            width,
            height,
            rgba_buf: Vec::new(),
        })
    }

    /// Draw the part of a combined-desktop dirty region that falls on this
    /// surface. `region` is in combined-desktop coordinates (inclusive edges);
    /// pixels are clipped to this surface and blitted at surface-local offsets.
    pub fn draw(
        &mut self,
        image: &DecodedImage,
        region: ironrdp::pdu::geometry::InclusiveRectangle,
    ) -> anyhow::Result<()> {
        // Clip the incoming region to this surface's rect (all in combined,
        // inclusive coords). Work in u32 to avoid u16 overflow at the edges.
        let surf_left = u32::from(self.origin_x);
        let surf_top = u32::from(self.origin_y);
        let surf_right = surf_left + u32::from(self.width) - 1;
        let surf_bottom = surf_top + u32::from(self.height) - 1;

        let sx0 = u32::from(region.left).max(surf_left);
        let sy0 = u32::from(region.top).max(surf_top);
        let sx1 = u32::from(region.right).min(surf_right);
        let sy1 = u32::from(region.bottom).min(surf_bottom);

        if sx0 > sx1 || sy0 > sy1 {
            return Ok(()); // region doesn't touch this monitor
        }

        let w = (sx1 - sx0 + 1) as usize;
        let h = (sy1 - sy0 + 1) as usize;
        // Destination on this canvas = combined coord minus this surface's origin.
        let dx = (sx0 - surf_left) as f64;
        let dy = (sy0 - surf_top) as f64;

        let stride = usize::from(image.width()) * 4; // RGBA = 4 bytes per pixel
        let src = image.data();
        let region_bytes = w * h * 4;

        // Fast path: the clipped slice spans this surface's full source width and
        // starts at the framebuffer's left edge, so the rows are contiguous in
        // `src` — slice directly, no row-by-row copy. (This is the single-monitor
        // common case: surface == whole framebuffer.)
        if w == usize::from(image.width()) && sx0 == 0 {
            let src_start = sy0 as usize * stride;
            let src_end = src_start + region_bytes;
            if src_end <= src.len() {
                let slice = &src[src_start..src_end];
                let image_data = ImageData::new_with_u8_clamped_array_and_sh(
                    Clamped(slice),
                    w as u32,
                    h as u32,
                )
                .map_err(|_| anyhow::anyhow!("ImageData creation failed"))?;

                self.ctx
                    .put_image_data(&image_data, dx, dy)
                    .map_err(|_| anyhow::anyhow!("putImageData failed"))?;

                return Ok(());
            }
        }

        // Partial region: copy the overlapping rows into the persistent buffer.
        self.rgba_buf.resize(region_bytes, 0);

        let row_bytes = w * 4;
        for row in 0..h {
            let src_offset = (sy0 as usize + row) * stride + sx0 as usize * 4;
            let dst_offset = row * row_bytes;

            if src_offset + row_bytes <= src.len() {
                self.rgba_buf[dst_offset..dst_offset + row_bytes]
                    .copy_from_slice(&src[src_offset..src_offset + row_bytes]);
            }
        }

        let image_data = ImageData::new_with_u8_clamped_array_and_sh(
            Clamped(&self.rgba_buf),
            w as u32,
            h as u32,
        )
        .map_err(|_| anyhow::anyhow!("ImageData creation failed"))?;

        self.ctx
            .put_image_data(&image_data, dx, dy)
            .map_err(|_| anyhow::anyhow!("putImageData failed"))?;

        Ok(())
    }

    /// Set the cursor style on the canvas element.
    pub fn set_cursor(&self, style: &str) {
        let element: &web_sys::HtmlElement = self.canvas.unchecked_ref();
        let _ = element.style().set_property("cursor", style);
    }

    /// Set a custom cursor from an RGBA bitmap.
    /// Creates an offscreen canvas, renders the bitmap, converts to data URL,
    /// and sets it as CSS cursor with the given hotspot.
    pub fn set_custom_cursor(
        &self,
        rgba_data: &[u8],
        width: u32,
        height: u32,
        hotspot_x: u32,
        hotspot_y: u32,
    ) {
        let window = match web_sys::window() {
            Some(w) => w,
            None => return,
        };
        let document = match window.document() {
            Some(d) => d,
            None => return,
        };

        // Create an offscreen canvas to render the cursor bitmap
        let off_canvas = match document.create_element("canvas") {
            Ok(el) => el,
            Err(_) => return,
        };
        let off_canvas: HtmlCanvasElement = match off_canvas.dyn_into() {
            Ok(c) => c,
            Err(_) => return,
        };
        off_canvas.set_width(width);
        off_canvas.set_height(height);

        let off_ctx: CanvasRenderingContext2d = match off_canvas
            .get_context("2d")
            .ok()
            .flatten()
            .and_then(|c| c.dyn_into().ok())
        {
            Some(ctx) => ctx,
            None => return,
        };

        // putImageData expects RGBA — pointer bitmaps from IronRDP are already RGBA
        if let Ok(img_data) = ImageData::new_with_u8_clamped_array_and_sh(
            Clamped(&rgba_data[..]),
            width,
            height,
        ) {
            let _ = off_ctx.put_image_data(&img_data, 0.0, 0.0);
        }

        // Convert to data URL and set as CSS cursor
        if let Ok(data_url) = off_canvas.to_data_url() {
            let cursor_css = format!("url({data_url}) {hotspot_x} {hotspot_y}, auto");
            self.set_cursor(&cursor_css);
        }
    }
}
