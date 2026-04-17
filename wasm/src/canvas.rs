use anyhow::Context as _;
use ironrdp::session::image::DecodedImage;
use wasm_bindgen::prelude::*;
use wasm_bindgen::Clamped;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};

/// Canvas renderer: writes RGBA pixels directly via putImageData (no re-encoding).
pub(crate) struct Canvas {
    ctx: CanvasRenderingContext2d,
    canvas: HtmlCanvasElement,
}

impl Canvas {
    pub fn new(canvas_id: &str, width: u16, height: u16) -> anyhow::Result<Self> {
        let window = web_sys::window().context("no window")?;
        let document = window.document().context("no document")?;
        let element = document
            .get_element_by_id(canvas_id)
            .context("canvas element not found")?;
        let canvas: HtmlCanvasElement = element
            .dyn_into()
            .map_err(|_| anyhow::anyhow!("element is not a canvas"))?;

        canvas.set_width(u32::from(width));
        canvas.set_height(u32::from(height));

        let ctx: CanvasRenderingContext2d = canvas
            .get_context("2d")
            .map_err(|_| anyhow::anyhow!("get_context failed"))?
            .context("no 2d context")?
            .dyn_into()
            .map_err(|_| anyhow::anyhow!("not CanvasRenderingContext2d"))?;

        // Disable image smoothing for crisp pixel rendering
        ctx.set_image_smoothing_enabled(false);

        Ok(Self { ctx, canvas })
    }

    /// Draw a dirty region from the DecodedImage onto the canvas.
    /// Uses putImageData for zero-copy RGBA pixel transfer.
    pub fn draw(
        &mut self,
        image: &DecodedImage,
        region: ironrdp::pdu::geometry::InclusiveRectangle,
    ) -> anyhow::Result<()> {
        let x = region.left;
        let y = region.top;
        let w = region.right.saturating_sub(region.left).saturating_add(1);
        let h = region.bottom.saturating_sub(region.top).saturating_add(1);

        if w == 0 || h == 0 {
            return Ok(());
        }

        // Extract the dirty rectangle's pixel data from the full framebuffer
        let stride = usize::from(image.width()) * 4; // RGBA = 4 bytes per pixel
        let src = image.data();

        let mut rgba_buf = vec![0u8; usize::from(w) * usize::from(h) * 4];

        for row in 0..usize::from(h) {
            let src_offset = (usize::from(y) + row) * stride + usize::from(x) * 4;
            let dst_offset = row * usize::from(w) * 4;
            let row_bytes = usize::from(w) * 4;

            if src_offset + row_bytes <= src.len() {
                rgba_buf[dst_offset..dst_offset + row_bytes]
                    .copy_from_slice(&src[src_offset..src_offset + row_bytes]);
            }
        }

        let image_data = ImageData::new_with_u8_clamped_array_and_sh(
            Clamped(&rgba_buf),
            u32::from(w),
            u32::from(h),
        )
        .map_err(|_| anyhow::anyhow!("ImageData creation failed"))?;

        self.ctx
            .put_image_data(&image_data, f64::from(x), f64::from(y))
            .map_err(|_| anyhow::anyhow!("putImageData failed"))?;

        Ok(())
    }

    /// Set the cursor style on the canvas element.
    pub fn set_cursor(&self, style: &str) {
        // HtmlCanvasElement inherits from HtmlElement via Element
        // Use the js interop to set the style
        let element: &web_sys::HtmlElement = self.canvas.unchecked_ref();
        let _ = element.style().set_property("cursor", style);
    }
}
