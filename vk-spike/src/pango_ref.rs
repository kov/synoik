//! Reference text render via pango + cairo — the same CPU path the panel uses today. Rendered
//! beside our swash atlas so 1x crispness can be compared directly (they will differ slightly:
//! pango/cairo use FreeType bytecode hinting, swash uses skrifa's autohinter).

use anyhow::{Context, Result};

/// Render `text` at `px` pixels into a `w`x`h` RGBA buffer, `fg` on opaque `bg`, pen at `origin`.
pub fn render(
    text: &str,
    w: i32,
    h: i32,
    px: f64,
    fg: [u8; 3],
    bg: [u8; 3],
    origin: (f64, f64),
) -> Result<Vec<u8>> {
    let surface =
        cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).context("cairo surface")?;
    {
        let cr = cairo::Context::new(&surface).context("cairo context")?;
        cr.set_source_rgb(
            bg[0] as f64 / 255.0,
            bg[1] as f64 / 255.0,
            bg[2] as f64 / 255.0,
        );
        cr.paint()?;

        let layout = pangocairo::functions::create_layout(&cr);
        let mut font = pango::FontDescription::new();
        font.set_family("Sans");
        font.set_absolute_size(px * pango::SCALE as f64);
        layout.set_font_description(Some(&font));
        layout.set_text(text);

        cr.set_source_rgb(
            fg[0] as f64 / 255.0,
            fg[1] as f64 / 255.0,
            fg[2] as f64 / 255.0,
        );
        cr.move_to(origin.0, origin.1);
        pangocairo::functions::show_layout(&cr, &layout);
    }
    surface.flush();

    let stride = surface.stride() as usize;
    let data = surface.take_data().context("cairo take_data")?;
    // ARGB32 is native-endian; on little-endian that is B,G,R,A bytes, premultiplied. The bg is
    // opaque so premultiplied == straight; just reorder to RGBA.
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let si = y * stride + x * 4;
            let di = (y * w as usize + x) * 4;
            rgba[di] = data[si + 2];
            rgba[di + 1] = data[si + 1];
            rgba[di + 2] = data[si];
            rgba[di + 3] = data[si + 3];
        }
    }
    Ok(rgba)
}
