//! A/B equivalence test: render the same scene (clear + a solid quad + a memory texture) through
//! both the [`VulkanRenderer`] and Smithay's CPU `PixmanRenderer`, offscreen, and assert the
//! read-back pixels match within tolerance.
//!
//! Pixman is a deterministic, GPU-free reference implementation of the exact renderer traits, so
//! it makes an ideal oracle: the Pixman side needs no device, and the Vulkan side guard-skips when
//! no Vulkan device is present. Runs on Venus (real target) and lavapipe (deterministic CPU).

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element, Kind, RenderElement};
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::renderer::{
    Bind, Color32F, ExportMem, Frame, ImportMem, Offscreen, Renderer,
};
use smithay::utils::{Buffer as BufferCoord, Physical, Point, Rectangle, Scale, Size, Transform};

use super::VulkanRenderer;
use crate::niri::OutputRenderElements;
use crate::render_helpers::rounded_texture::RoundedTextureRenderElement;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

const W: i32 = 64;
const H: i32 = 64;
/// Texture size (drawn 1:1 so no filtering divergence between renderers).
const TW: i32 = 32;
const TH: i32 = 32;

const CLEAR: [f32; 4] = [0.25, 0.25, 0.25, 1.0];
const SOLID: [f32; 4] = [0.80, 0.10, 0.10, 1.0];
const GREEN: [f32; 4] = [0.10, 0.70, 0.20, 1.0];

/// Per-channel tolerance: absorbs the ±1 the two renderers can differ by when rounding f32→u8.
const TOL: u8 = 2;

/// A deterministic opaque 32×32 RGBA pattern (a gradient), tight-packed as `[R,G,B,A]`.
fn texels() -> Vec<u8> {
    let mut v = Vec::with_capacity((TW * TH * 4) as usize);
    for y in 0..TH {
        for x in 0..TW {
            v.extend_from_slice(&[(x * 8) as u8, (y * 8) as u8, 128, 255]);
        }
    }
    v
}

/// Build the scene's elements for a given renderer: a red solid over the left half, and the
/// gradient texture at native size in the top-right.
fn scene_elements<R: Renderer + ImportMem>(
    r: &mut R,
) -> (SolidColorRenderElement, TextureRenderElement<R::TextureId>) {
    let solid_buffer =
        SolidColorBuffer::new(Size::<f64, _>::from((W as f64 / 2.0, H as f64)), SOLID);
    let solid = SolidColorRenderElement::from_buffer(
        &solid_buffer,
        Point::<f64, _>::from((0.0, 0.0)),
        1.0,
        Kind::Unspecified,
    );

    let buffer = TextureBuffer::from_memory(
        r,
        &texels(),
        Fourcc::Abgr8888,
        (TW, TH),
        false,
        1.0,
        Transform::Normal,
        Vec::new(),
    )
    .expect("import texture");
    let texture = TextureRenderElement::from_texture_buffer(
        buffer,
        Point::<f64, _>::from((W as f64 / 2.0, 0.0)),
        1.0,
        None,
        None,
        Kind::Unspecified,
    );

    (solid, texture)
}

/// Render the scene into `target` and read it back as tight `Abgr8888` (`[R,G,B,A]`) bytes.
///
/// Generic over the renderer `R` and its already-created offscreen target `T`; `T` is inferred
/// from `target`, so this works for both renderers without naming their private target types.
fn render_into<R, T>(r: &mut R, target: &mut T) -> Vec<u8>
where
    R: Renderer + ImportMem + ExportMem + Bind<T>,
{
    let size = Size::<i32, Physical>::from((W, H));
    let scale = Scale::<f64>::from(1.0);
    let (solid, texture) = scene_elements(r);

    {
        let mut fb = r.bind(&mut *target).expect("bind");
        let mut frame = r.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .clear(Color32F::from(CLEAR), &[Rectangle::from_size(size)])
            .expect("clear");
        // `damage` is element-local (dst-relative): renderers offset it by the element's dst.loc.
        let solid_geo = Element::geometry(&solid, scale);
        RenderElement::<R>::draw(
            &solid,
            &mut frame,
            Element::src(&solid),
            solid_geo,
            &[Rectangle::from_size(solid_geo.size)],
            &[],
            None,
        )
        .expect("draw solid");
        let texture_geo = Element::geometry(&texture, scale);
        RenderElement::<R>::draw(
            &texture,
            &mut frame,
            Element::src(&texture),
            texture_geo,
            &[Rectangle::from_size(texture_geo.size)],
            &[],
            None,
        )
        .expect("draw texture");
        // finish() submits + fence-waits synchronously, so the sync point is already signaled.
        let _sync = frame.finish().expect("finish");
    }

    let fb = r.bind(&mut *target).expect("rebind for readback");
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((W, H)));
    let mapping = r
        .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
        .expect("copy_framebuffer");
    r.map_texture(&mapping).expect("map_texture").to_vec()
}

/// Assert two tight RGBA8 buffers match within `TOL` per channel, reporting the worst pixel.
fn assert_close(a: &[u8], b: &[u8]) {
    assert_eq!(a.len(), b.len(), "buffer size mismatch");
    let (mut worst, mut worst_at, mut over) = (0u8, (0i32, 0i32, 0usize), 0usize);
    for (i, (&x, &y)) in a.iter().zip(b.iter()).enumerate() {
        let d = x.abs_diff(y);
        if d > worst {
            worst = d;
            let px = i / 4;
            worst_at = (px as i32 % W, px as i32 / W, i % 4);
        }
        if d > TOL {
            over += 1;
        }
    }
    assert!(
        over == 0,
        "vulkan vs pixman differ in {over} channels; worst diff {worst} at \
         (x={}, y={}, channel={})",
        worst_at.0,
        worst_at.1,
        worst_at.2,
    );
}

/// Two opaque, non-overlapping solids: a red left half and a green top-right quadrant (clear shows
/// through in the bottom-right). Rebuilt per renderer; the [`SolidColorBuffer`]s only need to
/// outlive [`SolidColorRenderElement::from_buffer`], which copies the color and size out.
fn solid_scene() -> Vec<SolidColorRenderElement> {
    let red_buffer = SolidColorBuffer::new(Size::<f64, _>::from((W as f64 / 2.0, H as f64)), SOLID);
    let red = SolidColorRenderElement::from_buffer(
        &red_buffer,
        Point::<f64, _>::from((0.0, 0.0)),
        1.0,
        Kind::Unspecified,
    );

    let green_buffer = SolidColorBuffer::new(
        Size::<f64, _>::from((W as f64 / 2.0, H as f64 / 2.0)),
        GREEN,
    );
    let green = SolidColorRenderElement::from_buffer(
        &green_buffer,
        Point::<f64, _>::from((W as f64 / 2.0, 0.0)),
        1.0,
        Kind::Unspecified,
    );

    vec![red, green]
}

/// Render an arbitrary element list into `target` and read it back as tight `Abgr8888` bytes.
///
/// Generic over the element type `E` as well as the renderer, so the exact same clear→draw→readback
/// path drives either bare [`SolidColorRenderElement`]s (through Pixman) or the niri
/// [`OutputRenderElements`] enum (through Vulkan), letting the two be compared pixel-for-pixel.
fn render_elements_into<R, T, E>(r: &mut R, target: &mut T, elements: &[E]) -> Vec<u8>
where
    R: Renderer + ImportMem + ExportMem + Bind<T>,
    E: Element + RenderElement<R>,
{
    let size = Size::<i32, Physical>::from((W, H));
    let scale = Scale::<f64>::from(1.0);

    {
        let mut fb = r.bind(&mut *target).expect("bind");
        let mut frame = r.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .clear(Color32F::from(CLEAR), &[Rectangle::from_size(size)])
            .expect("clear");
        for e in elements {
            let geo = Element::geometry(e, scale);
            RenderElement::<R>::draw(
                e,
                &mut frame,
                Element::src(e),
                geo,
                &[Rectangle::from_size(geo.size)],
                &[],
                None,
            )
            .expect("draw element");
        }
        let _sync = frame.finish().expect("finish");
    }

    let fb = r.bind(&mut *target).expect("rebind for readback");
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((W, H)));
    let mapping = r
        .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
        .expect("copy_framebuffer");
    r.map_texture(&mapping).expect("map_texture").to_vec()
}

/// The M2 seam: drive a scene through the real `OutputRenderElements<VulkanRenderer>` enum (whose
/// `RenderElement<VulkanRenderer>` arm the macro now emits) and assert it matches the Pixman oracle
/// drawing the same solids bare. This exercises the generic enum dispatch — not just the leaf
/// element draws that [`vulkan_matches_pixman`] covers — so it proves the whole element tree can
/// compose and render through the owned Vulkan renderer.
#[test]
fn vulkan_output_render_elements_match_pixman() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "skipping vulkan_output_render_elements_match_pixman: no Vulkan device ({e})"
            );
            return;
        }
    };

    // Vulkan side: wrap each solid in the OutputRenderElements enum so the draw goes through the
    // macro-generated `RenderElement<VulkanRenderer>` dispatch, exactly as niri's real render path.
    let vk_elements: Vec<OutputRenderElements<VulkanRenderer>> = solid_scene()
        .into_iter()
        .map(OutputRenderElements::SolidColor)
        .collect();
    let mut vk_target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("vulkan offscreen");
    let vk_pixels = render_elements_into(&mut vk, &mut vk_target, &vk_elements);

    // Pixman oracle: the same solids, bare. Pixman is not a `NiriRenderer` (its foreign error type
    // can't carry `From<GlesError>`), so it can't hold `OutputRenderElements`; but the enum arm
    // only delegates to these leaf draws, so a bare-vs-enum match confirms the dispatch is
    // transparent.
    let px_elements = solid_scene();
    let mut px = PixmanRenderer::new().expect("pixman renderer");
    let mut px_target = px
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("pixman offscreen");
    let px_pixels = render_elements_into(&mut px, &mut px_target, &px_elements);

    assert_close(&vk_pixels, &px_pixels);
}

#[test]
fn vulkan_matches_pixman() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_matches_pixman: no Vulkan device ({e})");
            return;
        }
    };
    eprintln!("vulkan A/B against pixman on device: {}", vk.device_name());

    let mut vk_target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("vulkan offscreen");
    let vk_pixels = render_into(&mut vk, &mut vk_target);

    let mut px = PixmanRenderer::new().expect("pixman renderer");
    let mut px_target = px
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("pixman offscreen");
    let px_pixels = render_into(&mut px, &mut px_target);

    assert_close(&vk_pixels, &px_pixels);
}

// --- M3 step 1: RoundedTextureRenderElement through the owned Vulkan renderer -------------------

/// Corner radius (logical px == physical px here, scale 1) for the rounded-texture tests.
const RADIUS: f64 = 16.0;

/// A `W×H` opaque gradient with every channel well above `CLEAR` (so an interior pixel is
/// unambiguously distinguishable from the cleared background), tight `[R,G,B,A]`. Blue is constant
/// so coverage — not the gradient — is what varies it across the corner.
fn rounded_texels() -> Vec<u8> {
    let mut v = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            v.extend_from_slice(&expected_texel(x, y));
        }
    }
    v
}

/// The gradient value of [`rounded_texels`] at `(x, y)` — the color an opaque (covered) output
/// pixel should hold when the texture is drawn 1:1.
fn expected_texel(x: i32, y: i32) -> [u8; 4] {
    [(96 + x) as u8, (96 + y) as u8, 180, 255]
}

/// The opaque background color `CLEAR` as read back (`round(0.25 * 255) = 64`).
fn clear_u8() -> [u8; 4] {
    CLEAR.map(|c| (c * 255.0).round() as u8)
}

/// One `[R,G,B,A]` pixel out of a tight `W`-wide RGBA8 buffer.
fn px(buf: &[u8], x: i32, y: i32) -> [u8; 4] {
    let i = ((y * W + x) * 4) as usize;
    [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
}

/// Whether two pixels agree within `tol` per channel.
fn close_px(a: [u8; 4], b: [u8; 4], tol: u8) -> bool {
    a.iter().zip(b).all(|(&x, y)| x.abs_diff(y) <= tol)
}

/// Build a rounded `W×H` texture element on `vk` and render it (cleared to `CLEAR`) into a fresh
/// offscreen, returning the read-back pixels. Drawn 1:1 (no filtering), full-`src`, `geometry ==
/// dst`, scale 1 — the wallpaper-shaped case the M3 material handles.
fn render_rounded(vk: &mut VulkanRenderer, corner_radius: f64) -> Vec<u8> {
    let buffer = TextureBuffer::from_memory(
        vk,
        &rounded_texels(),
        Fourcc::Abgr8888,
        (W, H),
        false,
        1.0,
        Transform::Normal,
        Vec::new(),
    )
    .expect("import rounded texture");
    let inner = TextureRenderElement::from_texture_buffer(
        buffer,
        Point::<f64, _>::from((0.0, 0.0)),
        1.0,
        None,
        None,
        Kind::Unspecified,
    );
    let elem = RoundedTextureRenderElement::new_vulkan(
        inner,
        corner_radius,
        Rectangle::from_size(Size::<f64, _>::from((W as f64, H as f64))),
        Scale::from(1.0),
    );

    let mut target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("vulkan offscreen");
    render_elements_into(vk, &mut target, std::slice::from_ref(&elem))
}

/// The rounded-texture material cuts the quad's corners to the SDF disc: interior shows the source
/// texel, deep corners show the cleared background, the corner arc-center stays opaque (not a
/// square clip), and the boundary is antialiased (not a 1-bit mask). Oracle-free structural
/// invariants — Pixman renders square corners so it is an anti-oracle here, and these pins encode
/// the exact intended behavior.
#[test]
fn vulkan_rounded_texture_cuts_corners() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_rounded_texture_cuts_corners: no Vulkan device ({e})");
            return;
        }
    };
    let pixels = render_rounded(&mut vk, RADIUS);
    let clear = clear_u8();

    // Interior: the source texel, not the background (fails on the degraded no-op).
    assert!(
        close_px(px(&pixels, 32, 32), expected_texel(32, 32), 3),
        "interior should be the source texel, got {:?}",
        px(&pixels, 32, 32),
    );
    // Deep corner: cut to the background (fails on a plain/square-corner draw).
    assert!(
        close_px(px(&pixels, 2, 2), clear, 3),
        "corner should be cut to the background, got {:?}",
        px(&pixels, 2, 2),
    );
    // Arc center (radius, radius): inside the rounded region → opaque texel (fails on an
    // over-aggressive full-quadrant clip).
    assert!(
        close_px(px(&pixels, 16, 16), expected_texel(16, 16), 4),
        "corner arc-center should be opaque texel, got {:?}",
        px(&pixels, 16, 16),
    );
    // Edge midpoint: only the corners are cut → opaque texel.
    assert!(
        close_px(px(&pixels, 32, 2), expected_texel(32, 2), 3),
        "top edge midpoint should be opaque texel, got {:?}",
        px(&pixels, 32, 2),
    );
    // Antialiasing: some corner pixel is a partial blend of texel-blue (180) and clear-blue (64),
    // i.e. strictly between them — a 1-bit mask would have none.
    let has_aa = (0..12)
        .flat_map(|y| (0..12).map(move |x| (x, y)))
        .any(|(x, y)| {
            let b = px(&pixels, x, y)[2];
            b > 70 && b < 174
        });
    assert!(
        has_aa,
        "expected an antialiased partial-coverage pixel along the corner arc",
    );
}

/// A zero radius exercises the delegate-to-`inner` branch: no corners are cut, so the whole quad —
/// corners included — is the opaque source texture, identical to a plain textured draw.
#[test]
fn vulkan_rounded_texture_zero_radius_is_plain() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "skipping vulkan_rounded_texture_zero_radius_is_plain: no Vulkan device ({e})"
            );
            return;
        }
    };
    let pixels = render_rounded(&mut vk, 0.0);

    // The corner is now the plain texture, not the background.
    assert!(
        close_px(px(&pixels, 2, 2), expected_texel(2, 2), 3),
        "zero radius must draw the plain texture into the corner, got {:?}",
        px(&pixels, 2, 2),
    );
    assert!(
        close_px(px(&pixels, 32, 32), expected_texel(32, 32), 3),
        "interior should be the source texel, got {:?}",
        px(&pixels, 32, 32),
    );
}
