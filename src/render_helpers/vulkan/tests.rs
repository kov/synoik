//! A/B equivalence test: render the same scene (clear + a solid quad + a memory texture) through
//! both the [`VulkanRenderer`] and Smithay's CPU `PixmanRenderer`, offscreen, and assert the
//! read-back pixels match within tolerance.
//!
//! Pixman is a deterministic, GPU-free reference implementation of the exact renderer traits, so
//! it makes an ideal oracle: the Pixman side needs no device, and the Vulkan side guard-skips when
//! no Vulkan device is present. Runs on Venus (real target) and lavapipe (deterministic CPU).

use glam::Mat3;
use niri_config::{Color, CornerRadius, GradientInterpolation};
use niri_vk::render::{PostprocessPush, ResizePush};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element, Kind, RenderElement};
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::renderer::{
    Bind, Color32F, ExportMem, Frame, ImportMem, Offscreen, Renderer,
};
use smithay::utils::{
    Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Scale, Size, Transform,
};

use super::custom::{pack_affine, CustomAnimPush, CustomResizePush, CustomShaderType};
use super::{VkTexture, VulkanRenderer};
use crate::niri::OutputRenderElements;
use crate::render_helpers::blur::BlurOptions;
use crate::render_helpers::border::BorderRenderElement;
use crate::render_helpers::gradient_fade_texture::GradientFadeTextureRenderElement;
use crate::render_helpers::offscreen::OffscreenBuffer;
use crate::render_helpers::render_to_vec;
use crate::render_helpers::resize::ResizeRenderElement;
use crate::render_helpers::rounded_texture::RoundedTextureRenderElement;
use crate::render_helpers::shadow::ShadowRenderElement;
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

/// Build a rounded `W×H` texture element on `vk` from `texels` (a `W×H` image) sampling `src`
/// (logical; `None` = full), and render it (cleared to `CLEAR`) into a fresh offscreen, returning
/// the read-back pixels. Drawn 1:1 into the whole target, `geometry == dst`, scale 1 — the
/// wallpaper-shaped case the M3 material handles.
fn render_rounded_src(
    vk: &mut VulkanRenderer,
    texels: &[u8],
    corner_radius: f64,
    src: Option<Rectangle<f64, Logical>>,
) -> Vec<u8> {
    let buffer = TextureBuffer::from_memory(
        vk,
        texels,
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
        src,
        // Explicit dst size so the element geometry stays the full quad even for a partial `src`
        // (the wallpaper likewise passes its view size, not the cropped src size).
        Some(Size::<f64, _>::from((W as f64, H as f64))),
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

/// The wallpaper-shaped full-`src` gradient scene.
fn render_rounded(vk: &mut VulkanRenderer, corner_radius: f64) -> Vec<u8> {
    render_rounded_src(vk, &rounded_texels(), corner_radius, None)
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

/// A partial `src` (the overview wallpaper's zoom-crop) must be sampled through the shader's
/// `src_rect` remap: with a left-red / right-blue texture and `src` = the right half, the whole
/// quad shows blue — a pixel that would be red under a (wrong) full-`src` sample proves the remap.
#[test]
fn vulkan_rounded_texture_partial_src() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_rounded_texture_partial_src: no Vulkan device ({e})");
            return;
        }
    };

    const RED: [u8; 4] = [200, 40, 40, 255];
    const BLUE: [u8; 4] = [40, 40, 200, 255];
    let mut texels = Vec::with_capacity((W * H * 4) as usize);
    for _y in 0..H {
        for x in 0..W {
            texels.extend_from_slice(if x < W / 2 { &RED } else { &BLUE });
        }
    }
    // Sample only the right (blue) half, stretched across the whole quad.
    let src = Rectangle::new(
        Point::<f64, Logical>::from((W as f64 / 2.0, 0.0)),
        Size::<f64, Logical>::from((W as f64 / 2.0, H as f64)),
    );
    let pixels = render_rounded_src(&mut vk, &texels, RADIUS, Some(src));

    // Interior, and a left-side opaque point that a full-`src` sample would draw red, are blue.
    assert!(
        close_px(px(&pixels, 32, 32), BLUE, 3),
        "interior should sample the blue src half, got {:?}",
        px(&pixels, 32, 32),
    );
    assert!(
        close_px(px(&pixels, 8, 32), BLUE, 3),
        "left-edge pixel should sample blue (full-src would sample red here), got {:?}",
        px(&pixels, 8, 32),
    );
    // Rounding still cuts the corner to the background.
    assert!(
        close_px(px(&pixels, 2, 2), clear_u8(), 3),
        "corner should still be cut to the background, got {:?}",
        px(&pixels, 2, 2),
    );
}

// --- M3 step 1c: GradientFadeTextureRenderElement through the owned Vulkan renderer ------------

/// A horizontally-clipped texture (src narrower than the buffer) fades its alpha out toward the
/// clipped edge: the left stays opaque, the right edge fades to the background, and the band
/// between is a monotonic partial blend. Oracle-free structural invariants.
#[test]
fn vulkan_gradient_fade_clipped_texture() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_gradient_fade_clipped_texture: no Vulkan device ({e})");
            return;
        }
    };

    const ORANGE: [u8; 4] = [220, 120, 40, 255];
    let texels: Vec<u8> = ORANGE
        .iter()
        .copied()
        .cycle()
        .take((W * H * 4) as usize)
        .collect();
    let buffer = TextureBuffer::from_memory(
        &mut vk,
        &texels,
        Fourcc::Abgr8888,
        (W, H),
        false,
        1.0,
        Transform::Normal,
        Vec::new(),
    )
    .expect("import gradient texture");
    // src is only 48 of the 64 wide → clipped, so the element adds a fade near the right edge.
    let src = Rectangle::new(
        Point::<f64, Logical>::from((0.0, 0.0)),
        Size::<f64, Logical>::from((48.0, H as f64)),
    );
    let inner = TextureRenderElement::from_texture_buffer(
        buffer,
        Point::<f64, _>::from((0.0, 0.0)),
        1.0,
        Some(src),
        Some(Size::<f64, _>::from((W as f64, H as f64))),
        Kind::Unspecified,
    );
    let elem = GradientFadeTextureRenderElement::new_vulkan(inner);

    let mut target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("vulkan offscreen");
    let pixels = render_elements_into(&mut vk, &mut target, std::slice::from_ref(&elem));

    // Left of the fade band: opaque source color.
    assert!(
        close_px(px(&pixels, 4, 32), ORANGE, 4),
        "left should be opaque orange, got {:?}",
        px(&pixels, 4, 32),
    );
    // Right edge: faded almost fully out → close to the background (low red, orange R=220 vs
    // clear R=64).
    let right = px(&pixels, W - 1, 32);
    assert!(
        right[0] <= 90,
        "right edge should fade toward the background, got {right:?}",
    );
    // Band midpoint: a partial blend of orange (R=220) and background (R=64).
    let mid_r = px(&pixels, 52, 32)[0];
    assert!(
        (100..=180).contains(&mid_r),
        "fade band midpoint should be a partial blend, red={mid_r}",
    );
    // Monotonic: more fade toward the right → red decreases across the band.
    assert!(
        px(&pixels, 44, 32)[0] > px(&pixels, 58, 32)[0],
        "fade should be monotonic across the band",
    );
}

// --- M3 step 2: BorderRenderElement through the owned Vulkan renderer --------------------------

/// A rounded border with a horizontal red→blue sRGB gradient: the ring is colored (red on the
/// left, blue on the right), its interior is cut out to the background, and the rounded outer
/// corner is cut away. Exercises the procedural border pipeline (gradient + double rounded-rect
/// SDF + premultiplied blend). Oracle-free structural invariants.
#[test]
fn vulkan_border_ring_gradient_and_rounding() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_border_ring_gradient_and_rounding: no Vulkan device ({e})");
            return;
        }
    };

    let red = Color::new_unpremul(1.0, 0.0, 0.0, 1.0);
    let blue = Color::new_unpremul(0.0, 0.0, 1.0, 1.0);
    let geo = Rectangle::from_size(Size::<f64, Logical>::from((W as f64, H as f64)));
    let elem = BorderRenderElement::new(
        Size::<f64, Logical>::from((W as f64, H as f64)), // size
        geo,                                              // gradient_area
        GradientInterpolation::default(),                 // sRGB / shorter
        red,
        blue,
        0.0, // angle (horizontal gradient)
        geo, // geometry
        8.0, // border_width
        CornerRadius::from(16.0),
        1.0, // scale
        1.0, // alpha
    );

    let mut target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("vulkan offscreen");
    let pixels = render_elements_into(&mut vk, &mut target, std::slice::from_ref(&elem));

    // Left of the ring: reddish (gradient start).
    let left = px(&pixels, 2, 32);
    assert!(
        left[0] > 150 && left[2] < 90,
        "left ring should be reddish, got {left:?}",
    );
    // Right of the ring: bluish (gradient end).
    let right = px(&pixels, W - 3, 32);
    assert!(
        right[2] > 150 && right[0] < 90,
        "right ring should be bluish, got {right:?}",
    );
    // Interior (inside the border width): cut out to the background.
    assert!(
        close_px(px(&pixels, 32, 32), clear_u8(), 4),
        "interior should be cut out to the background, got {:?}",
        px(&pixels, 32, 32),
    );
    // Rounded outer corner: cut away to the background.
    assert!(
        close_px(px(&pixels, 1, 1), clear_u8(), 4),
        "outer corner should be rounded away, got {:?}",
        px(&pixels, 1, 1),
    );
    // Top edge band is the gradient (green channel far from the background's 64).
    let top = px(&pixels, 32, 2);
    assert!(
        top[1] < 40,
        "top edge should be the border gradient, not the background, got {top:?}",
    );
}

// --- M3 step 3: ShadowRenderElement through the owned Vulkan renderer --------------------------

/// A gaussian drop shadow of a 32×32 box centered in the 64×64 area: darkest at the box center,
/// fading smoothly to the background outward, monotonically. Exercises the shadow pipeline (erf
/// gaussian + premultiplied blend). Oracle-free structural invariants.
#[test]
fn vulkan_shadow_gaussian_falloff() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_shadow_gaussian_falloff: no Vulkan device ({e})");
            return;
        }
    };

    let black = Color::new_unpremul(0.0, 0.0, 0.0, 1.0);
    // A 32×32 box centered in the 64×64 element (so the shadow has room to fall off on all sides).
    let geometry = Rectangle::new(
        Point::<f64, Logical>::from((16.0, 16.0)),
        Size::<f64, Logical>::from((32.0, 32.0)),
    );
    let elem = ShadowRenderElement::new(
        Size::<f64, Logical>::from((W as f64, H as f64)), // size (the drawn area)
        geometry,
        black,
        4.0,                     // sigma
        CornerRadius::from(0.0), // corner_radius
        1.0,                     // scale
        Rectangle::default(),    // window_geometry (no cutout)
        CornerRadius::from(0.0), // window_corner_radius
        1.0,                     // alpha
    );

    let mut target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("vulkan offscreen");
    let pixels = render_elements_into(&mut vk, &mut target, std::slice::from_ref(&elem));

    // Box center: nearly full shadow → dark (black over the background).
    let center = px(&pixels, 32, 32);
    assert!(
        center[0] < 40 && center[1] < 40 && center[2] < 40,
        "box center should be a dark shadow, got {center:?}",
    );
    // Far corner: outside 3·sigma → essentially no shadow → background.
    assert!(
        close_px(px(&pixels, 0, 0), clear_u8(), 10),
        "far corner should be ~background, got {:?}",
        px(&pixels, 0, 0),
    );
    // Monotonic falloff along the left approach (box starts at x=16): closer to the box is darker.
    let far = px(&pixels, 2, 32)[0];
    let near = px(&pixels, 14, 32)[0];
    assert!(
        far > near,
        "shadow should deepen toward the box (far={far} should be lighter than near={near})",
    );
    // The approach has a genuine mid-tone (soft edge, not a hard step).
    let mid = px(&pixels, 10, 32)[0];
    assert!(
        (near..far).contains(&mid) || (20..60).contains(&mid),
        "shadow edge should be a soft gradient, mid={mid}",
    );
}

/// `ShadowRenderElement::with_alpha` must actually fade the drawn shadow.
///
/// It used to write only `inner.alpha` — the `Element` bookkeeping — while the Vulkan push reads
/// `params.alpha`, so the fade was a silent no-op on the live renderer and workspace shadows
/// popped in at full strength instead of fading with the overview
/// (`Monitor::render_workspace_shadows` is the caller). Renders the same shadow at full and at a
/// quarter alpha and asserts the faded one is visibly lighter; before the fix both were identical.
#[test]
fn vulkan_shadow_with_alpha_fades_the_draw() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_shadow_with_alpha_fades_the_draw: no Vulkan device ({e})");
            return;
        }
    };

    let shadow = |alpha: f32| {
        ShadowRenderElement::new(
            Size::<f64, Logical>::from((W as f64, H as f64)),
            Rectangle::new(
                Point::<f64, Logical>::from((16.0, 16.0)),
                Size::<f64, Logical>::from((32.0, 32.0)),
            ),
            Color::new_unpremul(0.0, 0.0, 0.0, 1.0),
            4.0,
            CornerRadius::from(0.0),
            1.0,
            Rectangle::default(),
            CornerRadius::from(0.0),
            1.0,
        )
        .with_alpha(alpha)
    };

    let render = |vk: &mut VulkanRenderer, elem: ShadowRenderElement| {
        let mut target = vk
            .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
            .expect("vulkan offscreen");
        render_elements_into(vk, &mut target, std::slice::from_ref(&elem))
    };

    let full = render(&mut vk, shadow(1.0));
    let faded = render(&mut vk, shadow(0.25));

    // At the box centre the shadow is at its darkest, so the fade shows up most clearly.
    let full_c = px(&full, 32, 32);
    let faded_c = px(&faded, 32, 32);
    assert!(
        faded_c[0] as i32 > full_c[0] as i32 + 40,
        "with_alpha(0.25) must draw a markedly lighter shadow than with_alpha(1.0), \
         got faded={faded_c:?} vs full={full_c:?}"
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

// --- The sampleable-offscreen bridge: render into an offscreen, then sample it ------------------

/// Render a scene into an offscreen [`VkTexture`], transition it to sampleable, then draw that
/// offscreen (full-`src`, 1:1) into a *second* offscreen and read the second back. The re-sampled
/// result must reproduce the source pixel-for-pixel — proving `Offscreen::create_buffer` targets
/// can be rendered into **and then re-sampled** (the offscreen-snapshot / blur / clipped-surface
/// bridge), not merely rendered into and read straight back.
#[test]
fn vulkan_offscreen_sampleable_roundtrip() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_offscreen_sampleable_roundtrip: no Vulkan device ({e})");
            return;
        }
    };

    let size = Size::<i32, Physical>::from((W, H));

    // Source offscreen A: the two-solid scene (red left half, green top-right, clear bottom-right).
    // `render_elements_into` leaves A holding the scene and returns its readback — our reference
    // for what a correct re-sample must reproduce.
    let mut a = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("offscreen A");
    let a_elements: Vec<OutputRenderElements<VulkanRenderer>> = solid_scene()
        .into_iter()
        .map(OutputRenderElements::SolidColor)
        .collect();
    let a_pixels = render_elements_into(&mut vk, &mut a, &a_elements);

    // The bridge: transition A from its post-render TRANSFER_SRC_OPTIMAL to
    // SHADER_READ_ONLY_OPTIMAL so it can be bound as a sampled texture.
    vk.make_sampleable(&a).expect("make A sampleable");

    // Destination offscreen B: clear, then sample all of A 1:1 over the whole quad.
    let mut b = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("offscreen B");
    {
        let mut fb = vk.bind(&mut b).expect("bind B");
        let mut frame = vk
            .render(&mut fb, size, Transform::Normal)
            .expect("render B");
        frame
            .clear(Color32F::from(CLEAR), &[Rectangle::from_size(size)])
            .expect("clear B");
        let full_src = Rectangle::<f64, BufferCoord>::from_size(Size::from((W as f64, H as f64)));
        let full_dst = Rectangle::<i32, Physical>::from_size(size);
        frame
            .render_texture_from_to(
                &a,
                full_src,
                full_dst,
                &[full_dst],
                &[],
                Transform::Normal,
                1.0,
            )
            .expect("sample A into B");
        let _sync = frame.finish().expect("finish B");
    }

    // Read B back and compare to A. A is opaque and the draw is full-coverage 1:1, so B must equal
    // A.
    let fb = vk.bind(&mut b).expect("rebind B for readback");
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((W, H)));
    let mapping = vk
        .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
        .expect("copy_framebuffer B");
    let b_pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

    assert_close(&a_pixels, &b_pixels);
}

// --- Offscreen snapshots: OffscreenBuffer renders a subtree, its element re-samples it ----------

/// niri's `OffscreenBuffer` renders a subtree into an offscreen texture and hands back an element
/// that re-samples it (window open/close + alpha-fade animations). Drive that whole machinery
/// through the owned Vulkan renderer — `OffscreenBuffer::render` create_buffer→bind→render→
/// make-sampleable, then `OffscreenRenderElement<VkTexture>`'s Vulkan draw — and assert the
/// snapshot reproduces a direct render of the same scene.
#[test]
fn vulkan_offscreen_snapshot() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_offscreen_snapshot: no Vulkan device ({e})");
            return;
        }
    };

    // Reference: the two-solid scene rendered directly into an offscreen (cleared to CLEAR).
    let mut ref_target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("reference offscreen");
    let direct = render_elements_into(&mut vk, &mut ref_target, &solid_scene());

    // Snapshot: render the same scene into an OffscreenBuffer (cleared transparent), then draw the
    // element it returns — which samples the offscreen — over a CLEAR background.
    let buffer = OffscreenBuffer::<VkTexture>::default();
    let (elem, _sync, _data) = buffer
        .render(&mut vk, Scale::from(1.0), &solid_scene())
        .expect("offscreen snapshot render");

    let mut snap_target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("snapshot offscreen");
    let snapshot = render_elements_into(&mut vk, &mut snap_target, std::slice::from_ref(&elem));

    // The offscreen is opaque where the solids cover it and transparent elsewhere, so re-sampling
    // it over CLEAR must match the direct render pixel-for-pixel.
    assert_close(&direct, &snapshot);
}

// --- Dual-kawase blur: a hard edge becomes a smooth ramp ----------------------------------------

/// The owned renderer's dual-kawase blur (`render_blur`, driving niri-vk's `BlurChain`) softens a
/// hard black|white split into a monotonic mid-gray ramp localized around the boundary. Structural
/// invariants (blur has no per-pixel oracle): the boundary column is intermediate gray, the profile
/// is monotonic left→right, and columns deep in each half keep their extreme (not washed uniform).
#[test]
fn vulkan_blur_smooths_edge() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_blur_smooths_edge: no Vulkan device ({e})");
            return;
        }
    };

    // A hard vertical split: left half black, right half white (opaque).
    let mut texels = Vec::with_capacity((W * H * 4) as usize);
    for _y in 0..H {
        for x in 0..W {
            let v = if x < W / 2 { 0 } else { 255 };
            texels.extend_from_slice(&[v, v, v, 255]);
        }
    }
    let source = vk
        .import_memory(&texels, Fourcc::Abgr8888, Size::from((W, H)), false)
        .expect("import source");

    let mut blurred = vk
        .render_blur(
            &source,
            BlurOptions {
                passes: 3,
                offset: 2.0,
            },
        )
        .expect("render blur");

    // Read the blurred output back (a sampleable offscreen).
    let fb = vk.bind(&mut blurred).expect("bind blurred");
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((W, H)));
    let mapping = vk
        .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
        .expect("copy_framebuffer");
    let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

    // Red channel along the mid-height scanline (grayscale, so r == g == b).
    let r = |x: i32| px(&pixels, x, H / 2)[0] as i32;

    // The boundary is a genuine blend, not the original hard step.
    assert!(
        (30..=225).contains(&r(W / 2)),
        "boundary should be mid-gray, got {}",
        r(W / 2),
    );
    // A step convolved with a symmetric kernel is a monotonic ramp: darker left of the split,
    // lighter right of it.
    assert!(
        r(W / 2 - 8) < r(W / 2) && r(W / 2) < r(W / 2 + 8),
        "expected a monotonic ramp across the split, got {} {} {}",
        r(W / 2 - 8),
        r(W / 2),
        r(W / 2 + 8),
    );
    // The blur is localized: columns deep in each half stay near their extreme rather than washing
    // out to uniform gray.
    assert!(
        r(4) < r(W / 2) && r(W - 4) > r(W / 2),
        "deep columns should keep their extreme (left {} < mid {} < right {})",
        r(4),
        r(W / 2),
        r(W - 4),
    );
}

// --- Postprocess-and-clip: sample + saturation + rounded-corner clip -----------------------------

/// The postprocess-and-clip material (`render_postprocess`, niri's clipped-surface /
/// framebuffer-effect shader) samples a texture, desaturates it, and cuts it to a rounded rect.
/// With an opaque red source, `saturation = 0.3`, and `corner_radius = 16` over the whole quad
/// (identity `input_to_geo`): a deep corner is clipped away (shows the CLEAR background) while the
/// interior is the source pulled toward its own luminance (red drops, green rises, the channel gap
/// shrinks).
#[test]
fn vulkan_postprocess_clips_and_desaturates() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_postprocess_clips_and_desaturates: no Vulkan device ({e})");
            return;
        }
    };

    // Opaque, strongly-saturated red source.
    let mut texels = Vec::with_capacity((W * H * 4) as usize);
    for _ in 0..(W * H) {
        texels.extend_from_slice(&[200, 40, 40, 255]);
    }
    let source = vk
        .import_memory(&texels, Fourcc::Abgr8888, Size::from((W, H)), false)
        .expect("import source");

    let size = Size::<i32, Physical>::from((W, H));
    let mut target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("offscreen");
    {
        let mut fb = vk.bind(&mut target).expect("bind");
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .clear(Color32F::from(CLEAR), &[Rectangle::from_size(size)])
            .expect("clear");
        let full_src = Rectangle::<f64, BufferCoord>::from_size(Size::from((W as f64, H as f64)));
        let full_dst = Rectangle::<i32, Physical>::from_size(size);
        let push = PostprocessPush {
            geo_size: [W as f32, H as f32],
            corner_radius: [16.0; 4],
            bg_color: [0.0; 4],
            // Identity mat3 (as columns): coords_geo == v_uv, so the geometry is the whole quad.
            input_to_geo: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
            ],
            // Identity: sample straight at v_uv (no output-transform remap).
            sample_transform: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
            ],
            niri_scale: 1.0,
            niri_alpha: 1.0,
            saturation: 0.3,
            noise: 0.0,
            // origin/size/target/src_rect are filled by render_postprocess.
            ..Default::default()
        };
        frame
            .render_postprocess(
                &source,
                full_src,
                full_dst,
                &[Rectangle::from_size(full_dst.size)],
                push,
            )
            .expect("render postprocess");
        let _sync = frame.finish().expect("finish");
    }

    let fb = vk.bind(&mut target).expect("rebind for readback");
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((W, H)));
    let mapping = vk
        .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
        .expect("copy_framebuffer");
    let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

    // Deep corner: clipped away, so the CLEAR background shows through (material contributed
    // nothing).
    assert!(
        close_px(px(&pixels, 2, 2), clear_u8(), 8),
        "corner should be clipped to the background, got {:?}",
        px(&pixels, 2, 2),
    );
    // Interior: the red source desaturated toward its luminance (≈(112, 64, 64) at sat 0.3). Red
    // drops from 200, green rises from 40, and the red/green gap shrinks from 160.
    let inner = px(&pixels, 32, 32);
    assert!(
        (95..=130).contains(&(inner[0] as i32)),
        "interior red should drop toward gray, got {inner:?}",
    );
    assert!(
        (50..=80).contains(&(inner[1] as i32)),
        "interior green should rise toward gray, got {inner:?}",
    );
    assert!(
        (inner[0] as i32) - (inner[1] as i32) < 80,
        "saturation should shrink the red/green gap, got {inner:?}",
    );
}

// --- M3 step 4: resize cross-fade through the owned Vulkan renderer ------------------------------

/// The resize cross-fade material (`render_resize`, niri's `ResizeRenderElement`) blends two window
/// snapshots (prev + next) by `clamped_progress`, then optionally clips/rounds to the current
/// geometry. With an opaque red "prev", blue "next", `clamped_progress = 0.5`, identity transforms,
/// and `corner_radius = 16` clipped to the whole quad: the interior is the 50/50 blend (purple),
/// and a deep corner is clipped away to the CLEAR background. Oracle-free structural invariants.
#[test]
fn vulkan_resize_crossfades() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_resize_crossfades: no Vulkan device ({e})");
            return;
        }
    };

    // Opaque red "prev" and blue "next" snapshots (pure primaries so a 50/50 blend is unambiguous).
    let red: Vec<u8> = [255u8, 0, 0, 255]
        .iter()
        .copied()
        .cycle()
        .take((W * H * 4) as usize)
        .collect();
    let blue: Vec<u8> = [0u8, 0, 255, 255]
        .iter()
        .copied()
        .cycle()
        .take((W * H * 4) as usize)
        .collect();
    let tex_prev = vk
        .import_memory(&red, Fourcc::Abgr8888, Size::from((W, H)), false)
        .expect("import prev");
    let tex_next = vk
        .import_memory(&blue, Fourcc::Abgr8888, Size::from((W, H)), false)
        .expect("import next");

    let size = Size::<i32, Physical>::from((W, H));
    let mut target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("offscreen");
    {
        let mut fb = vk.bind(&mut target).expect("bind");
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .clear(Color32F::from(CLEAR), &[Rectangle::from_size(size)])
            .expect("clear");
        let dst = Rectangle::<i32, Physical>::from_size(size);
        // Identity affine-diagonal transforms ([scale.xy, translate.xy]): coords_curr_geo == v_uv,
        // and each texture is sampled 1:1 across the whole geometry.
        let push = ResizePush {
            curr_geo_size: [W as f32, H as f32],
            input_to_curr_geo: [1.0, 1.0, 0.0, 0.0],
            geo_to_tex_prev: [1.0, 1.0, 0.0, 0.0],
            geo_to_tex_next: [1.0, 1.0, 0.0, 0.0],
            corner_radius: [16.0; 4],
            clamped_progress: 0.5,
            clip_to_geometry: 1.0,
            niri_scale: 1.0,
            niri_alpha: 1.0,
            // origin/size/target are filled by render_resize.
            ..Default::default()
        };
        frame
            .render_resize(
                &tex_prev,
                &tex_next,
                dst,
                &[Rectangle::from_size(dst.size)],
                push,
            )
            .expect("render resize");
        let _sync = frame.finish().expect("finish");
    }

    let fb = vk.bind(&mut target).expect("rebind for readback");
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((W, H)));
    let mapping = vk
        .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
        .expect("copy_framebuffer");
    let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

    // Interior: the 50/50 cross-fade of red and blue → purple (r ≈ b, little green).
    let inner = px(&pixels, 32, 32);
    assert!(
        inner[1] < 20,
        "cross-fade of pure red + pure blue should have ~no green, got {inner:?}",
    );
    assert!(
        (100..=155).contains(&(inner[0] as i32)) && (100..=155).contains(&(inner[2] as i32)),
        "interior should be a ~50/50 red/blue blend, got {inner:?}",
    );
    assert!(
        (inner[0] as i32 - inner[2] as i32).abs() < 20,
        "at progress 0.5 red and blue should be ~equal, got {inner:?}",
    );
    // Deep corner: rounded/clipped away to the CLEAR background.
    assert!(
        close_px(px(&pixels, 2, 2), clear_u8(), 8),
        "corner should be clipped to the background, got {:?}",
        px(&pixels, 2, 2),
    );
}

/// The live `ResizeRenderElement::new` constructor: it lowers the resize geometry to a
/// `ResizePush` and draws through `render_resize` (the path `tile.rs` takes on a Vulkan session,
/// replacing the red placeholder). A spatially-distinct 4-quadrant "prev" with identity geometry
/// must be reproduced **1:1 at progress 0** — proving the affine-diagonal transforms carry no flip
/// or axis-swap — a solid "next" must fully take over at progress 1, and 0.5 must blend them.
#[test]
fn vulkan_new_vulkan_resize_element_crossfades() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "skipping vulkan_new_vulkan_resize_element_crossfades: no Vulkan device ({e})"
            );
            return;
        }
    };

    // 4-quadrant "prev": TL red, TR green, BL blue, BR white (tight `[R,G,B,A]`).
    let mut prev = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            let c = match (x < W / 2, y < H / 2) {
                (true, true) => [255u8, 0, 0, 255],
                (false, true) => [0, 255, 0, 255],
                (true, false) => [0, 0, 255, 255],
                (false, false) => [255, 255, 255, 255],
            };
            prev.extend_from_slice(&c);
        }
    }
    let next = solid_texels([0, 0, 255, 255]);

    let full_logical = Rectangle::<f64, Logical>::from_size(Size::from((W as f64, H as f64)));
    let full_phys = Rectangle::<i32, Physical>::from_size(Size::from((W, H)));
    let sz = Size::<f64, Logical>::from((W as f64, H as f64));

    let render = |vk: &mut VulkanRenderer, progress: f32| -> Vec<u8> {
        let tex_prev = vk
            .import_memory(&prev, Fourcc::Abgr8888, Size::from((W, H)), false)
            .expect("import prev");
        let tex_next = vk
            .import_memory(&next, Fourcc::Abgr8888, Size::from((W, H)), false)
            .expect("import next");
        let elem = ResizeRenderElement::new(
            full_logical,
            Scale::from(1.0),
            (tex_prev, full_phys),
            sz,
            (tex_next, full_phys),
            sz,
            progress,
            progress,
            CornerRadius::default(),
            false,
            1.0,
            false, // built-in crossfade
        );
        render_to_vec(
            vk,
            Size::from((W, H)),
            Scale::from(1.0),
            Transform::Normal,
            Fourcc::Abgr8888,
            [elem].into_iter(),
        )
        .expect("render resize element")
    };

    let near = |p: [u8; 4], want: [u8; 4]| {
        p.iter()
            .zip(want)
            .all(|(a, b)| (i16::from(*a) - i16::from(b)).abs() < 24)
    };
    let (qx, qy) = (W / 4, H / 4);

    // progress 0 ⇒ pure prev, reproduced 1:1: the four quadrant colors in their correct corners.
    let p0 = render(&mut vk, 0.0);
    assert!(
        near(px(&p0, qx, qy), [255, 0, 0, 255]),
        "TL should be red, got {:?}",
        px(&p0, qx, qy)
    );
    assert!(
        near(px(&p0, 3 * qx, qy), [0, 255, 0, 255]),
        "TR should be green, got {:?}",
        px(&p0, 3 * qx, qy)
    );
    assert!(
        near(px(&p0, qx, 3 * qy), [0, 0, 255, 255]),
        "BL should be blue, got {:?}",
        px(&p0, qx, 3 * qy)
    );
    assert!(
        near(px(&p0, 3 * qx, 3 * qy), [255, 255, 255, 255]),
        "BR should be white, got {:?}",
        px(&p0, 3 * qx, 3 * qy)
    );

    // progress 1 ⇒ pure next (solid blue) everywhere.
    let p1 = render(&mut vk, 1.0);
    assert!(
        near(px(&p1, qx, qy), [0, 0, 255, 255]),
        "at progress 1 TL should be next=blue, got {:?}",
        px(&p1, qx, qy)
    );
    assert!(
        near(px(&p1, 3 * qx, 3 * qy), [0, 0, 255, 255]),
        "at progress 1 BR should be next=blue, got {:?}",
        px(&p1, 3 * qx, 3 * qy)
    );

    // progress 0.5 ⇒ blend. TL is prev-red + next-blue ⇒ purple: R≈B, little green.
    let ph = render(&mut vk, 0.5);
    let tl = px(&ph, qx, qy);
    assert!(tl[1] < 40, "TL blend should have little green, got {tl:?}");
    assert!(
        tl[0] > 60 && tl[2] > 60,
        "TL blend should mix red and blue, got {tl:?}"
    );
}

/// The live wiring routes a resize animation through the user's CUSTOM resize shader when one is
/// installed (`use_custom=true` → `render_custom_resize`) instead of the built-in crossfade. Also
/// covers the config-facing install roundtrip: `set_custom_resize_shader` compiles + arms the slot
/// (`has_custom_shader` true), and `None` clears it. With a solid-green snippet installed, the
/// whole resize quad is green at progress 0.5 — not the red/blue crossfade the built-in path
/// produces.
#[test]
fn vulkan_resize_element_uses_custom_shader_when_installed() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "skipping vulkan_resize_element_uses_custom_shader_when_installed: no Vulkan \
                 device ({e})"
            );
            return;
        }
    };

    // Install roundtrip via the config-facing API (the owned-renderer dual of
    // shaders::set_custom_resize_program).
    assert!(!vk.has_custom_shader(CustomShaderType::Resize));
    vk.set_custom_resize_shader(Some(
        "vec4 resize_color(vec3 coords_curr_geo, vec3 size_curr_geo) {\n\
         return vec4(0.0, 1.0, 0.0, 1.0);\n\
         }",
    ));
    assert!(
        vk.has_custom_shader(CustomShaderType::Resize),
        "set_custom_resize_shader should compile + arm the slot",
    );

    let prev = solid_texels([255, 0, 0, 255]);
    let next = solid_texels([0, 0, 255, 255]);
    let full_logical = Rectangle::<f64, Logical>::from_size(Size::from((W as f64, H as f64)));
    let full_phys = Rectangle::<i32, Physical>::from_size(Size::from((W, H)));
    let sz = Size::<f64, Logical>::from((W as f64, H as f64));

    let tex_prev = vk
        .import_memory(&prev, Fourcc::Abgr8888, Size::from((W, H)), false)
        .expect("import prev");
    let tex_next = vk
        .import_memory(&next, Fourcc::Abgr8888, Size::from((W, H)), false)
        .expect("import next");
    let elem = ResizeRenderElement::new(
        full_logical,
        Scale::from(1.0),
        (tex_prev, full_phys),
        sz,
        (tex_next, full_phys),
        sz,
        0.5,
        0.5,
        CornerRadius::default(),
        false,
        1.0,
        true, // custom shader
    );
    let pixels = render_to_vec(
        &mut vk,
        Size::from((W, H)),
        Scale::from(1.0),
        Transform::Normal,
        Fourcc::Abgr8888,
        [elem].into_iter(),
    )
    .expect("render custom resize element");

    // The custom snippet paints solid green, ignoring both snapshots — the built-in crossfade of
    // red/blue would instead be purple-ish (little green).
    let c = px(&pixels, W / 2, H / 2);
    assert!(
        c[1] > 150 && c[0] < 90 && c[2] < 90,
        "custom resize shader should paint green (not the built-in crossfade), got {c:?}",
    );

    // Clearing removes it.
    vk.set_custom_resize_shader(None);
    assert!(!vk.has_custom_shader(CustomShaderType::Resize));
}

/// The live open/close wiring's element: `CustomAnimRenderElement`'s Vulkan arm draws through the
/// installed custom `open` shader via `render_custom_anim`. Install a solid-green open snippet,
/// build the element over a red snapshot, and the result is green (the shader ran) — the path
/// `opening_window.rs::render_vulkan` takes when a custom open shader is configured.
#[test]
fn vulkan_custom_anim_element_draws_the_open_shader() {
    use crate::render_helpers::custom_anim::CustomAnimRenderElement;

    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_custom_anim_element_draws_the_open_shader: no Vulkan ({e})");
            return;
        }
    };

    assert!(!vk.has_custom_shader(CustomShaderType::Open));
    vk.set_custom_open_shader(Some(
        "vec4 open_color(vec3 coords_geo, vec3 size_geo) {\n\
         return vec4(0.0, 1.0, 0.0, 1.0);\n\
         }",
    ));
    assert!(
        vk.has_custom_shader(CustomShaderType::Open),
        "set_custom_open_shader should compile + arm the slot",
    );

    let snapshot = vk
        .import_memory(
            &solid_texels([200, 0, 0, 255]),
            Fourcc::Abgr8888,
            Size::from((W, H)),
            false,
        )
        .expect("import snapshot");
    let area = Rectangle::<f64, Logical>::from_size(Size::from((W as f64, H as f64)));
    let push = CustomAnimPush {
        geo_size: [W as f32, H as f32],
        input_to_geo: [1.0, 1.0, 0.0, 0.0],
        geo_to_tex: [1.0, 1.0, 0.0, 0.0],
        alpha: 1.0,
        scale: 1.0,
        ..Default::default()
    };
    let elem =
        CustomAnimRenderElement::new_vulkan_anim(CustomShaderType::Open, snapshot, area, 1.0, push);
    let pixels = render_to_vec(
        &mut vk,
        Size::from((W, H)),
        Scale::from(1.0),
        Transform::Normal,
        Fourcc::Abgr8888,
        [elem].into_iter(),
    )
    .expect("render custom open element");

    let c = px(&pixels, W / 2, H / 2);
    assert!(
        c[1] > 150 && c[0] < 90 && c[2] < 90,
        "custom open shader should paint green (not the red snapshot), got {c:?}",
    );
}

/// The close sibling of [`vulkan_custom_anim_element_draws_the_open_shader`]:
/// `CustomAnimRenderElement`'s Vulkan arm, built with `CustomShaderType::Close`, draws through the
/// installed custom `close` shader via `render_custom_anim`. This pins the element-level close
/// wiring that `closing_window.rs::render_vulkan` uses when a custom close shader is configured
/// (the snapshot is captured to a `MemoryBuffer` and re-uploaded to a `VkTexture` there; here we
/// import one directly).
#[test]
fn vulkan_custom_anim_element_draws_the_close_shader() {
    use crate::render_helpers::custom_anim::CustomAnimRenderElement;

    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "skipping vulkan_custom_anim_element_draws_the_close_shader: no Vulkan ({e})"
            );
            return;
        }
    };

    assert!(!vk.has_custom_shader(CustomShaderType::Close));
    vk.set_custom_close_shader(Some(
        "vec4 close_color(vec3 coords_geo, vec3 size_geo) {\n\
         return vec4(0.0, 1.0, 0.0, 1.0);\n\
         }",
    ));
    assert!(
        vk.has_custom_shader(CustomShaderType::Close),
        "set_custom_close_shader should compile + arm the slot",
    );

    let snapshot = vk
        .import_memory(
            &solid_texels([200, 0, 0, 255]),
            Fourcc::Abgr8888,
            Size::from((W, H)),
            false,
        )
        .expect("import snapshot");
    let area = Rectangle::<f64, Logical>::from_size(Size::from((W as f64, H as f64)));
    let push = CustomAnimPush {
        geo_size: [W as f32, H as f32],
        input_to_geo: [1.0, 1.0, 0.0, 0.0],
        geo_to_tex: [1.0, 1.0, 0.0, 0.0],
        alpha: 1.0,
        scale: 1.0,
        ..Default::default()
    };
    let elem = CustomAnimRenderElement::new_vulkan_anim(
        CustomShaderType::Close,
        snapshot,
        area,
        1.0,
        push,
    );
    let pixels = render_to_vec(
        &mut vk,
        Size::from((W, H)),
        Scale::from(1.0),
        Transform::Normal,
        Fourcc::Abgr8888,
        [elem].into_iter(),
    )
    .expect("render custom close element");

    let c = px(&pixels, W / 2, H / 2);
    assert!(
        c[1] > 150 && c[0] < 90 && c[2] < 90,
        "custom close shader should paint green (not the red snapshot), got {c:?}",
    );
}

// --- M3 step 5: custom runtime GLSL animation shaders -------------------------------------------

/// A W×H opaque solid, tight `[R,G,B,A]`, for the custom-shader tests.
fn solid_texels(rgba: [u8; 4]) -> Vec<u8> {
    rgba.iter()
        .copied()
        .cycle()
        .take((W * H * 4) as usize)
        .collect()
}

/// A user **resize** snippet, compiled from GLSL at runtime (glslangValidator) and drawn through
/// the owned Vulkan renderer, produces the crossfade + clip it describes. This exercises the whole
/// runtime custom-shader path at once: assemble → compile → cached two-texture pipeline → draw,
/// plus the `texture2D`→`texture` shim and the affine `mat3` reconstruction from packed `vec4`s.
/// Red "prev" + blue "next" at progress 0.5 ⇒ a purple interior; a deep corner is clipped away
/// (clip_to_geometry + corner rounding), exactly like the built-in `render_resize` material — but
/// here the shader came from a config-style snippet, not a compiled-in one.
#[test]
fn vulkan_custom_resize_crossfade() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_custom_resize_crossfade: no Vulkan device ({e})");
            return;
        }
    };

    let tex_prev = vk
        .import_memory(
            &solid_texels([255, 0, 0, 255]),
            Fourcc::Abgr8888,
            Size::from((W, H)),
            false,
        )
        .expect("import prev");
    let tex_next = vk
        .import_memory(
            &solid_texels([0, 0, 255, 255]),
            Fourcc::Abgr8888,
            Size::from((W, H)),
            false,
        )
        .expect("import next");

    // A user snippet using GLES-style texture2D and the niri_* uniform names, exactly as a config
    // custom shader would (this is niri's built-in resize body, supplied as if by the user).
    let snippet = "\
vec4 resize_color(vec3 coords_curr_geo, vec3 size_curr_geo) {
    vec4 prev = texture2D(niri_tex_prev, (niri_geo_to_tex_prev * coords_curr_geo).st);
    vec4 next = texture2D(niri_tex_next, (niri_geo_to_tex_next * coords_curr_geo).st);
    return mix(prev, next, niri_clamped_progress);
}";
    vk.set_custom_shader(CustomShaderType::Resize, Some(snippet))
        .expect("compile custom resize snippet");

    let size = Size::<i32, Physical>::from((W, H));
    let mut target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("offscreen");
    {
        let mut fb = vk.bind(&mut target).expect("bind");
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .clear(Color32F::from(CLEAR), &[Rectangle::from_size(size)])
            .expect("clear");
        let dst = Rectangle::<i32, Physical>::from_size(size);
        // Identity affine-diagonal transforms; the drawn quad is the geometry.
        let push = CustomResizePush {
            curr_geo_size: [W as f32, H as f32],
            input_to_curr_geo: [1.0, 1.0, 0.0, 0.0],
            geo_to_tex_prev: [1.0, 1.0, 0.0, 0.0],
            geo_to_tex_next: [1.0, 1.0, 0.0, 0.0],
            corner_radius: [16.0; 4],
            progress: 0.5,
            clamped_progress: 0.5,
            clip_to_geometry: 1.0,
            alpha: 1.0,
            scale: 1.0,
            ..Default::default()
        };
        frame
            .render_custom_resize(
                &tex_prev,
                &tex_next,
                dst,
                &[Rectangle::from_size(dst.size)],
                push,
            )
            .expect("draw custom resize");
        let _sync = frame.finish().expect("finish");
    }

    let fb = vk.bind(&mut target).expect("rebind for readback");
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((W, H)));
    let mapping = vk
        .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
        .expect("copy_framebuffer");
    let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

    let inner = px(&pixels, 32, 32);
    assert!(
        inner[1] < 20
            && (100..=155).contains(&(inner[0] as i32))
            && (100..=155).contains(&(inner[2] as i32)),
        "custom resize interior should be the 50/50 red/blue blend, got {inner:?}",
    );
    assert!(
        close_px(px(&pixels, 2, 2), clear_u8(), 8),
        "custom resize corner should be clipped to the background, got {:?}",
        px(&pixels, 2, 2),
    );
}

/// A user **close** snippet that returns its incoming `coords_geo` as color, drawn with a
/// non-identity affine transform (distinct per-axis scale + translation), lands the exact pixels
/// the packed-`vec4` → `mat3` reconstruction predicts. This is the one test that drives the affine
/// path with real, per-axis-distinct values (identity would hide a swapped or dropped coefficient),
/// and it exercises the single-texture close/open pipeline and the `close_color` entry point. The
/// transform is built the way the Stage-3 element glue will build it — a `glam::Mat3` from
/// scale∘translate, packed by [`pack_affine`].
#[test]
fn vulkan_custom_close_affine_reconstruction() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_custom_close_affine_reconstruction: no Vulkan device ({e})");
            return;
        }
    };

    // The snippet ignores the texture and reports coords_geo directly, so the readback IS the
    // transformed coordinate — a direct probe of the mat3 reconstruction.
    let snippet = "\
vec4 close_color(vec3 coords_geo, vec3 size_geo) {
    return vec4(coords_geo.x, coords_geo.y, 0.0, 1.0);
}";
    vk.set_custom_shader(CustomShaderType::Close, Some(snippet))
        .expect("compile custom close snippet");

    // input_to_geo = translate(0.1, 0.2) ∘ scale(0.5, 0.25): coords_geo = (0.5*u+0.1, 0.25*v+0.2).
    let input_to_geo = pack_affine(
        Mat3::from_translation(glam::Vec2::new(0.1, 0.2))
            * Mat3::from_scale(glam::Vec2::new(0.5, 0.25)),
    );
    assert_eq!(input_to_geo, [0.5, 0.25, 0.1, 0.2]);

    // A texture must be bound (the pipeline has one sampler set) even though the snippet ignores
    // it.
    let dummy = vk
        .import_memory(
            &solid_texels([10, 20, 30, 255]),
            Fourcc::Abgr8888,
            Size::from((W, H)),
            false,
        )
        .expect("import dummy");

    let size = Size::<i32, Physical>::from((W, H));
    let mut target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("offscreen");
    {
        let mut fb = vk.bind(&mut target).expect("bind");
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .clear(Color32F::from(CLEAR), &[Rectangle::from_size(size)])
            .expect("clear");
        let dst = Rectangle::<i32, Physical>::from_size(size);
        let push = CustomAnimPush {
            geo_size: [W as f32, H as f32],
            input_to_geo,
            geo_to_tex: [1.0, 1.0, 0.0, 0.0],
            alpha: 1.0,
            scale: 1.0,
            ..Default::default()
        };
        frame
            .render_custom_anim(
                CustomShaderType::Close,
                &dummy,
                dst,
                &[Rectangle::from_size(dst.size)],
                push,
            )
            .expect("draw custom close");
        let _sync = frame.finish().expect("finish");
    }

    let fb = vk.bind(&mut target).expect("rebind for readback");
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((W, H)));
    let mapping = vk
        .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
        .expect("copy_framebuffer");
    let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

    // At pixel (32, 32) the varying niri_v_coords ≈ (32.5/64, 32.5/64) = (0.5078, 0.5078).
    // coords_geo = (0.5*0.5078 + 0.1, 0.25*0.5078 + 0.2) = (0.3539, 0.3270) ⇒ R≈90, G≈83.
    let got = px(&pixels, 32, 32);
    let uv = 32.5 / W as f32;
    let expect_r = ((0.5 * uv + 0.1) * 255.0).round() as i32;
    let expect_g = ((0.25 * uv + 0.2) * 255.0).round() as i32;
    assert!(
        (got[0] as i32 - expect_r).abs() <= 3 && (got[1] as i32 - expect_g).abs() <= 3,
        "affine-reconstructed coords should read back as ~({expect_r},{expect_g},0), got {got:?}",
    );
}

/// The custom-shader vertex stage rotates placement into a rotated output via `pc.proj` (the same
/// `mat2(proj)` convention as `quad.vert`). Draw a solid-green close shader over the **logical
/// left-half** rect under every output transform: dimension-*preserving* transforms
/// (Normal/180/Flipped/Flipped180) keep it a LEFT|RIGHT (vertical) split, while
/// dimension-*swapping* ones (90/270 and their flips) turn the tall rect into a TOP|BOTTOM
/// (horizontal) split. Probing the four quadrant centers, the split axis must flip exactly for the
/// swapping transforms — which only holds if `proj` is applied. Without it, every transform would
/// keep the vertical split and the swapping-transform assertions fail. (Square target, so logical
/// == physical dims for all.)
#[test]
fn vulkan_custom_shader_placement_follows_output_transform() {
    use smithay::utils::Transform;

    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "skipping vulkan_custom_shader_placement_follows_output_transform: no Vulkan \
                 device ({e})"
            );
            return;
        }
    };

    // Ignores its inputs and paints solid green — placement is the whole point here.
    let snippet = "\
vec4 close_color(vec3 coords_geo, vec3 size_geo) {
    return vec4(0.0, 1.0, 0.0, 1.0);
}";
    vk.set_custom_shader(CustomShaderType::Close, Some(snippet))
        .expect("compile custom close snippet");

    // One sampler set must be bound even though the snippet ignores it.
    let dummy = vk
        .import_memory(
            &solid_texels([10, 20, 30, 255]),
            Fourcc::Abgr8888,
            Size::from((W, H)),
            false,
        )
        .expect("import dummy");

    let is_green = |p: [u8; 4]| p[1] > 150 && p[0] < 100 && p[2] < 100;

    let transforms = [
        (Transform::Normal, false),
        (Transform::_180, false),
        (Transform::Flipped, false),
        (Transform::Flipped180, false),
        (Transform::_90, true),
        (Transform::_270, true),
        (Transform::Flipped90, true),
        (Transform::Flipped270, true),
    ];

    for (transform, swaps) in transforms {
        let size = Size::<i32, Physical>::from((W, H));
        let mut target = vk
            .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
            .expect("offscreen");
        {
            let mut fb = vk.bind(&mut target).expect("bind");
            let mut frame = vk.render(&mut fb, size, transform).expect("render");
            frame
                .clear(Color32F::from(CLEAR), &[Rectangle::from_size(size)])
                .expect("clear");
            // Logical left-half: a tall (W/2 × H) rect anchored at the origin.
            let dst = Rectangle::<i32, Physical>::new((0, 0).into(), (W / 2, H).into());
            let push = CustomAnimPush {
                geo_size: [(W / 2) as f32, H as f32],
                input_to_geo: [1.0, 1.0, 0.0, 0.0],
                geo_to_tex: [1.0, 1.0, 0.0, 0.0],
                alpha: 1.0,
                scale: 1.0,
                ..Default::default()
            };
            frame
                .render_custom_anim(
                    CustomShaderType::Close,
                    &dummy,
                    dst,
                    &[Rectangle::from_size(dst.size)],
                    push,
                )
                .expect("draw custom close");
            let _sync = frame.finish().expect("finish");
        }

        let fb = vk.bind(&mut target).expect("rebind for readback");
        let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((W, H)));
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

        // Quadrant centers: A top-left, B top-right, C bottom-left, D bottom-right.
        let a = is_green(px(&pixels, W / 4, H / 4));
        let b = is_green(px(&pixels, 3 * W / 4, H / 4));
        let c = is_green(px(&pixels, W / 4, 3 * H / 4));
        let d = is_green(px(&pixels, 3 * W / 4, 3 * H / 4));

        if swaps {
            // Horizontal split: top row uniform, bottom row uniform, the two rows differ.
            assert!(
                a == b && c == d && a != c,
                "{transform:?}: swapping transform should give a TOP|BOTTOM split \
                 (A={a} B={b} C={c} D={d})",
            );
        } else {
            // Vertical split: left column uniform, right column uniform, the two columns differ.
            assert!(
                a == c && b == d && a != b,
                "{transform:?}: preserving transform should give a LEFT|RIGHT split \
                 (A={a} B={b} C={c} D={d})",
            );
        }
    }
}

/// A user **open** snippet: samples the snapshot texture and uses the *unclamped* `niri_progress`
/// (which can overshoot [0,1] under spring animation) distinctly from `niri_clamped_progress`.
/// Proves the open entry point, single-texture sampling through the shim, and that progress is
/// passed unclamped: with progress = 1.5 and clamped = 1.0, the red channel encodes 1.5 (⇒ ~191,
/// clamped on write), which a clamped value (1.0 ⇒ 128) could not produce.
#[test]
fn vulkan_custom_open_samples_and_unclamped_progress() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "skipping vulkan_custom_open_samples_and_unclamped_progress: no Vulkan device ({e})"
            );
            return;
        }
    };

    let snippet = "\
vec4 open_color(vec3 coords_geo, vec3 size_geo) {
    vec4 tex = texture2D(niri_tex, (niri_geo_to_tex * coords_geo).st);
    return vec4(niri_progress * 0.5, niri_clamped_progress * 0.5, tex.b, 1.0);
}";
    vk.set_custom_shader(CustomShaderType::Open, Some(snippet))
        .expect("compile custom open snippet");

    // Snapshot with a distinctive blue channel to prove the texture was sampled.
    let tex = vk
        .import_memory(
            &solid_texels([0, 0, 200, 255]),
            Fourcc::Abgr8888,
            Size::from((W, H)),
            false,
        )
        .expect("import snapshot");

    let size = Size::<i32, Physical>::from((W, H));
    let mut target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("offscreen");
    {
        let mut fb = vk.bind(&mut target).expect("bind");
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .clear(Color32F::from(CLEAR), &[Rectangle::from_size(size)])
            .expect("clear");
        let dst = Rectangle::<i32, Physical>::from_size(size);
        let push = CustomAnimPush {
            geo_size: [W as f32, H as f32],
            input_to_geo: [1.0, 1.0, 0.0, 0.0],
            geo_to_tex: [1.0, 1.0, 0.0, 0.0],
            progress: 1.5, // unclamped (spring overshoot)
            clamped_progress: 1.0,
            alpha: 1.0,
            scale: 1.0,
            ..Default::default()
        };
        frame
            .render_custom_anim(
                CustomShaderType::Open,
                &tex,
                dst,
                &[Rectangle::from_size(dst.size)],
                push,
            )
            .expect("draw custom open");
        let _sync = frame.finish().expect("finish");
    }

    let fb = vk.bind(&mut target).expect("rebind for readback");
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((W, H)));
    let mapping = vk
        .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
        .expect("copy_framebuffer");
    let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

    let got = px(&pixels, 32, 32);
    assert!(
        (got[0] as i32 - 191).abs() <= 4,
        "unclamped progress 1.5 should give red ~191 (clamped would be 128), got {got:?}",
    );
    assert!(
        (got[1] as i32 - 128).abs() <= 4,
        "clamped progress 1.0 should give green ~128, got {got:?}",
    );
    assert!(
        (got[2] as i32 - 200).abs() <= 4,
        "snapshot blue channel should be sampled through, got {got:?}",
    );
}

/// A syntactically-broken user snippet must degrade gracefully: `set_custom_shader` returns `Err`
/// (carrying the glslang log), never panics, leaves the slot empty, and a subsequent
/// `render_custom_anim` for that empty slot is a no-op that leaves the CLEAR background intact.
#[test]
fn vulkan_custom_bad_snippet_degrades() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_custom_bad_snippet_degrades: no Vulkan device ({e})");
            return;
        }
    };

    let err = vk.set_custom_shader(CustomShaderType::Close, Some("this is not valid glsl {{{"));
    assert!(
        err.is_err(),
        "a broken snippet should return an error, not compile",
    );

    // The slot is still empty, so drawing it is a no-op: the target stays cleared.
    let tex = vk
        .import_memory(
            &solid_texels([255, 255, 255, 255]),
            Fourcc::Abgr8888,
            Size::from((W, H)),
            false,
        )
        .expect("import");
    let size = Size::<i32, Physical>::from((W, H));
    let mut target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("offscreen");
    {
        let mut fb = vk.bind(&mut target).expect("bind");
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .clear(Color32F::from(CLEAR), &[Rectangle::from_size(size)])
            .expect("clear");
        let dst = Rectangle::<i32, Physical>::from_size(size);
        frame
            .render_custom_anim(
                CustomShaderType::Close,
                &tex,
                dst,
                &[Rectangle::from_size(dst.size)],
                CustomAnimPush::default(),
            )
            .expect("no-op draw for empty slot");
        let _sync = frame.finish().expect("finish");
    }

    let fb = vk.bind(&mut target).expect("rebind for readback");
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((W, H)));
    let mapping = vk
        .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
        .expect("copy_framebuffer");
    let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

    assert!(
        close_px(px(&pixels, 32, 32), clear_u8(), 2),
        "an unset custom shader should draw nothing (background stays CLEAR), got {:?}",
        px(&pixels, 32, 32),
    );
}

// --- shm client-buffer import: byte order + X-alpha swizzle --------------------------------------

/// Import a `TW*TH` texture from `data` in `fourcc` (via `import_memory`, the same path
/// `import_shm_buffer` funnels into), draw it at the origin over `clear`, and read back the frame.
fn import_and_draw(
    vk: &mut VulkanRenderer,
    data: &[u8],
    fourcc: Fourcc,
    clear: [f32; 4],
) -> Vec<u8> {
    let buffer = TextureBuffer::from_memory(
        vk,
        data,
        fourcc,
        (TW, TH),
        false,
        1.0,
        Transform::Normal,
        Vec::new(),
    )
    .expect("import client buffer");
    let texture = TextureRenderElement::from_texture_buffer(
        buffer,
        Point::<f64, _>::from((0.0, 0.0)),
        1.0,
        None,
        None,
        Kind::Unspecified,
    );

    let mut target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("offscreen");
    {
        let mut fb = vk.bind(&mut target).expect("bind");
        let size = Size::<i32, Physical>::from((W, H));
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .clear(Color32F::from(clear), &[Rectangle::from_size(size)])
            .expect("clear");
        let geo = Element::geometry(&texture, Scale::<f64>::from(1.0));
        RenderElement::<VulkanRenderer>::draw(
            &texture,
            &mut frame,
            Element::src(&texture),
            geo,
            &[Rectangle::from_size(geo.size)],
            &[],
            None,
        )
        .expect("draw texture");
        let _sync = frame.finish().expect("finish");
    }

    let fb = vk.bind(&mut target).expect("rebind for readback");
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((W, H)));
    let mapping = vk
        .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
        .expect("copy_framebuffer");
    vk.map_texture(&mapping).expect("map_texture").to_vec()
}

/// wl_shm clients send pixels in ARGB/XRGB (BGRA byte order) and XBGR (with an undefined X byte).
/// `import_memory` must map each fourcc to the VkFormat that samples back correct RGBA, and force
/// the X byte to alpha 1.0. A channel-asymmetric color makes any byte swap unmistakable; the
/// X-format case draws over a DISTINCT clear color so a missing alpha-1 swizzle (which would leave
/// alpha 0 and show the background) is caught by the blend, not hidden.
#[test]
fn vulkan_import_respects_byte_order_and_x_alpha() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "skipping vulkan_import_respects_byte_order_and_x_alpha: no Vulkan device ({e})"
            );
            return;
        }
    };

    let (r, g, b) = (0x20u8, 0x40u8, 0x80u8);

    // ARGB8888 = memory bytes [B, G, R, A], opaque. The sampler must return (R, G, B, A).
    let argb = solid_texels([b, g, r, 0xFF]);
    let out = import_and_draw(&mut vk, &argb, Fourcc::Argb8888, CLEAR);
    assert!(
        close_px(px(&out, TW / 2, TH / 2), [r, g, b, 255], 2),
        "ARGB8888 byte order wrong: got {:?}, want {:?}",
        px(&out, TW / 2, TH / 2),
        [r, g, b, 255],
    );

    // XBGR8888 = memory bytes [R, G, B, X] with X = 0. Correct swizzle forces alpha to 1.0, so the
    // texture fully covers a distinct red clear; a swizzle bug would leave alpha 0 and show the
    // red.
    let distinct = [0.85, 0.05, 0.05, 1.0];
    let xbgr = solid_texels([r, g, b, 0x00]);
    let out = import_and_draw(&mut vk, &xbgr, Fourcc::Xbgr8888, distinct);
    assert!(
        close_px(px(&out, TW / 2, TH / 2), [r, g, b, 255], 2),
        "XBGR8888 X byte must sample as alpha=1 (covering the clear), got {:?}",
        px(&out, TW / 2, TH / 2),
    );
}

// --- production render_helpers path through Vulkan (Brick 2)
// --------------------------------------

/// The generic `render_helpers::render_to_vec` (the same entry `Niri::screenshot` uses) composites
/// a real `TextureRenderElement` through the owned Vulkan renderer and reads it back — proving the
/// production render path (create offscreen → bind → draw elements → ExportMem readback) is now
/// renderer-agnostic, not GLES-only. Uses an imported client-style texture, so the whole
/// import→composite→download chain runs on Vulkan.
#[test]
fn vulkan_render_to_vec_composites_a_texture() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_render_to_vec_composites_a_texture: no Vulkan device ({e})");
            return;
        }
    };

    // A client-style ARGB buffer (BGRA bytes) imported as a texture element at the origin.
    let (r, g, b) = (0x20u8, 0x40u8, 0x80u8);
    let buffer = TextureBuffer::from_memory(
        &mut vk,
        &solid_texels([b, g, r, 0xFF]),
        Fourcc::Argb8888,
        (TW, TH),
        false,
        1.0,
        Transform::Normal,
        Vec::new(),
    )
    .expect("import client buffer");
    let element = TextureRenderElement::from_texture_buffer(
        buffer,
        Point::<f64, _>::from((0.0, 0.0)),
        1.0,
        None,
        None,
        Kind::Unspecified,
    );

    // The production helper: no hand-rolled bind/render/readback here.
    let pixels = crate::render_helpers::render_to_vec(
        &mut vk,
        Size::<i32, Physical>::from((W, H)),
        Scale::<f64>::from(1.0),
        Transform::Normal,
        Fourcc::Abgr8888,
        [element].into_iter(),
    )
    .expect("render_to_vec through Vulkan");

    // The texture shows where it was placed...
    assert!(
        close_px(px(&pixels, TW / 2, TH / 2), [r, g, b, 255], 2),
        "composited texture wrong: got {:?}, want {:?}",
        px(&pixels, TW / 2, TH / 2),
        [r, g, b, 255],
    );
    // ...and the rest of the frame is the transparent clear render_to_vec uses.
    assert!(
        close_px(px(&pixels, W - 8, H - 8), [0, 0, 0, 0], 2),
        "outside the texture should be transparent, got {:?}",
        px(&pixels, W - 8, H - 8),
    );
}

// --- Stage 3 / Brick A: render into a GBM-allocated dmabuf (the KMS-scanout target) ------------

/// The KMS-scanout foundation: `VulkanRenderer` must be able to bind a **GBM-allocated dmabuf** as
/// a render target and composite straight into its memory, so a display controller can scan out the
/// result. This is the render-*into*-dmabuf half of the residual Stage-3 risk (the actual page-flip
/// is a live/gsrs check). We allocate a buffer the same way the tty backend does (a `GbmAllocator`
/// on the render node), export it as a Smithay `Dmabuf`, render a recognizable scene into it via
/// the real `Bind<Dmabuf>`, and read it back through the same imported image — proving the pixels
/// landed in the dmabuf's own memory. Skips when there is no Vulkan device or no usable render
/// node.
#[test]
fn vulkan_renders_into_a_gbm_dmabuf() {
    use std::fs::File;

    use smithay::backend::allocator::dmabuf::AsDmabuf;
    use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
    use smithay::backend::allocator::{Allocator, Buffer as _, Modifier};

    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_renders_into_a_gbm_dmabuf: no Vulkan device ({e})");
            return;
        }
    };

    let file = match File::options()
        .read(true)
        .write(true)
        .open("/dev/dri/renderD128")
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("skipping vulkan_renders_into_a_gbm_dmabuf: no render node ({e})");
            return;
        }
    };
    let gbm = match GbmDevice::new(file) {
        Ok(d) => d,
        Err(e) => {
            // e.g. under VK_DRIVER_FILES=lvp, Mesa GBM can't pick a device (Zink). GBM needs the
            // real virtio-gpu stack, so this path is a Venus-only test.
            eprintln!("skipping vulkan_renders_into_a_gbm_dmabuf: no GBM device ({e})");
            return;
        }
    };
    let mut alloc = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);
    let bo = match alloc.create_buffer(W as u32, H as u32, Fourcc::Abgr8888, &[Modifier::Linear]) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "skipping vulkan_renders_into_a_gbm_dmabuf: GBM cannot allocate Abgr8888 LINEAR \
                 ({e})"
            );
            return;
        }
    };
    let mut dmabuf = bo.export().expect("export dmabuf");
    eprintln!(
        "dmabuf: {:?} {}x{} modifier {:?} on {}",
        dmabuf.format().code,
        dmabuf.width(),
        dmabuf.height(),
        dmabuf.format().modifier,
        vk.device_name(),
    );

    let elements = solid_scene();
    let size = Size::<i32, Physical>::from((W, H));
    let scale = Scale::<f64>::from(1.0);

    // Bind the dmabuf as the render target and composite into it.
    let mut fb = vk.bind(&mut dmabuf).expect("bind dmabuf as render target");
    {
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .clear(Color32F::from(CLEAR), &[Rectangle::from_size(size)])
            .expect("clear");
        for e in &elements {
            let geo = Element::geometry(e, scale);
            RenderElement::<VulkanRenderer>::draw(
                e,
                &mut frame,
                Element::src(e),
                geo,
                &[Rectangle::from_size(geo.size)],
                &[],
                None,
            )
            .expect("draw solid");
        }
        // finish() submits + fence-waits, leaving the dmabuf image in TRANSFER_SRC for readback.
        let _sync = frame.finish().expect("finish");
    }

    // Read back through the same imported image: the memory is the dmabuf's, so correct pixels here
    // prove the render targeted the dmabuf.
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((W, H)));
    let mapping = vk
        .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
        .expect("copy_framebuffer");
    let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

    // solid_scene(): red over the left half, green over the top-right quadrant, CLEAR (grey) in the
    // bottom-right. Sample one representative pixel of each region.
    let red = [204, 26, 26, 255];
    let green = [26, 179, 51, 255];
    let grey = [64, 64, 64, 255];
    assert!(
        close_px(px(&pixels, W / 4, H / 2), red, 3),
        "left half should be red, got {:?}",
        px(&pixels, W / 4, H / 2),
    );
    assert!(
        close_px(px(&pixels, 3 * W / 4, H / 4), green, 3),
        "top-right should be green, got {:?}",
        px(&pixels, 3 * W / 4, H / 4),
    );
    assert!(
        close_px(px(&pixels, 3 * W / 4, 3 * H / 4), grey, 3),
        "bottom-right should be the clear color, got {:?}",
        px(&pixels, 3 * W / 4, 3 * H / 4),
    );
}

// --- client dmabuf import cache: reuse the imported image across commits, evict freed buffers ----

/// The client-dmabuf import cache (`import_dmabuf_as_texture`) keeps a client's imported
/// `VkTexture` keyed by buffer identity, so an animating dmabuf client (e.g. a WebGL page) does not
/// re-run the full `import_dmabuf_sampled` — a fresh `vkAllocateMemory` import +
/// image/view/sampler/descriptor set + fenced acquire barrier — on every commit (the per-frame
/// Venus host-resource churn that wedges the guest↔host ring; see `dmabuf_import_cache`). Pin the
/// bookkeeping: re-importing the same buffer reuses its entry and returns the *same* image (only
/// re-acquiring), a distinct buffer adds a distinct entry, and a freed buffer's entry is evicted on
/// the next lookup. Needs a Venus + GBM stack (real client dmabufs); skips on lavapipe / no GBM.
#[test]
fn vulkan_dmabuf_import_cache_reuses_and_evicts() {
    use std::fs::File;

    use smithay::backend::allocator::dmabuf::AsDmabuf;
    use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
    use smithay::backend::allocator::{Allocator, Modifier};

    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "skipping vulkan_dmabuf_import_cache_reuses_and_evicts: no Vulkan device ({e})"
            );
            return;
        }
    };
    let file = match File::options()
        .read(true)
        .write(true)
        .open("/dev/dri/renderD128")
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "skipping vulkan_dmabuf_import_cache_reuses_and_evicts: no render node ({e})"
            );
            return;
        }
    };
    let gbm = match GbmDevice::new(file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skipping vulkan_dmabuf_import_cache_reuses_and_evicts: no GBM device ({e})");
            return;
        }
    };
    let mut alloc = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);
    let d1 = match alloc.create_buffer(W as u32, H as u32, Fourcc::Abgr8888, &[Modifier::Linear]) {
        Ok(bo) => bo.export().expect("export d1"),
        Err(e) => {
            eprintln!(
                "skipping vulkan_dmabuf_import_cache_reuses_and_evicts: GBM cannot allocate \
                 Abgr8888 LINEAR ({e})"
            );
            return;
        }
    };
    let d2 = alloc
        .create_buffer(W as u32, H as u32, Fourcc::Abgr8888, &[Modifier::Linear])
        .expect("second GBM buffer")
        .export()
        .expect("export d2");

    // First import of d1: a miss populates the cache.
    let t1 = vk.import_dmabuf_as_texture(&d1).expect("import d1");
    assert_eq!(
        vk.dmabuf_import_cache_len(),
        1,
        "first import populates the cache",
    );

    // Re-import d1 (a recycled commit): a hit reuses the entry and returns the same image.
    let t1b = vk.import_dmabuf_as_texture(&d1).expect("re-import d1");
    assert_eq!(
        vk.dmabuf_import_cache_len(),
        1,
        "re-importing the same buffer must not grow the cache",
    );
    assert!(
        t1.same_image(&t1b),
        "a cache hit must return the reused image, not a fresh import",
    );

    // A distinct buffer imports a distinct image and adds an entry.
    let t2 = vk.import_dmabuf_as_texture(&d2).expect("import d2");
    assert_eq!(
        vk.dmabuf_import_cache_len(),
        2,
        "a distinct buffer adds a distinct entry",
    );
    assert!(
        !t1.same_image(&t2),
        "distinct buffers must import distinct images",
    );

    // Free d1 entirely: its WeakDmabuf key goes gone, so the next lookup evicts the stale entry.
    drop(t1);
    drop(t1b);
    drop(d1);
    let _t2b = vk
        .import_dmabuf_as_texture(&d2)
        .expect("re-import d2 after freeing d1");
    assert_eq!(
        vk.dmabuf_import_cache_len(),
        1,
        "a freed buffer's entry must be evicted on the next lookup",
    );
}

/// Part 2 of the client dmabuf-import cache: a cache HIT no longer runs the re-acquire barrier on
/// its own submit — it queues the texture, and the next `VulkanFrame::begin` folds the barrier into
/// the frame's command buffer (riding the frame submit, so there is no per-commit standalone
/// submit/fence-wait, the Venus ring pressure this path exists to reduce). Prove the mechanism
/// end-to-end: a miss queues nothing (its full import runs an internal barrier), a hit queues
/// exactly one deferred acquire, `begin()` drains it, and the frame samples the *new* producer
/// content written into the *same* shared dmabuf between commits. The `pending_*_len` assertions
/// are the mechanism proof (a disabled drain fails them — mutation-checked); the green→red content
/// check guards against gross sampling regressions (it does not, on this CPU-coherent LINEAR path,
/// by itself prove the barrier's placement). Needs a Venus + GBM stack (real client dmabufs,
/// CPU-writable LINEAR); skips on lavapipe / no GBM.
#[test]
fn vulkan_dmabuf_import_cache_defers_reacquire_into_the_frame() {
    use niri_vk::dmabuf::ForeignBuffer;
    use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
    use smithay::backend::allocator::Modifier;

    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "skipping vulkan_dmabuf_import_cache_defers_reacquire: no Vulkan device ({e})"
            );
            return;
        }
    };

    // Producer frame 1: a solid-green LINEAR client buffer.
    let mut fb = match ForeignBuffer::allocate_filled(W as u32, H as u32, [[0, 255, 0, 255]; 4]) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "skipping vulkan_dmabuf_import_cache_defers_reacquire: GBM cannot allocate ({e})"
            );
            return;
        }
    };
    // One Dmabuf, reused across both commits: re-importing the *same* buffer is a cache hit.
    let mut builder = Dmabuf::builder(
        (W, H),
        Fourcc::Argb8888,
        Modifier::Linear,
        DmabufFlags::empty(),
    );
    assert!(builder.add_plane(
        fb.fd().try_clone_to_owned().expect("dup fd"),
        0,
        fb.offset,
        fb.stride,
    ));
    let dmabuf = builder.build().expect("build dmabuf");

    // Renders `tex` 1:1 into a W×H offscreen and reads back tight Abgr8888 (`[R,G,B,A]`). Each call
    // is one frame → one `VulkanFrame::begin`, which drains any queued deferred acquire.
    fn render_client(vk: &mut VulkanRenderer, tex: VkTexture) -> Vec<u8> {
        let size = Size::<i32, Physical>::from((W, H));
        let buffer = TextureBuffer::from_texture(&*vk, tex, 1.0, Transform::Normal, Vec::new());
        let element = TextureRenderElement::from_texture_buffer(
            buffer,
            Point::from((0.0, 0.0)),
            1.0,
            None,
            None,
            Kind::Unspecified,
        );
        render_to_vec(
            vk,
            size,
            Scale::from(1.0),
            Transform::Normal,
            Fourcc::Abgr8888,
            [element].into_iter(),
        )
        .expect("render client dmabuf")
    }

    // MISS: the full import populates the cache and runs its OWN acquire barrier — nothing queued.
    let t1 = vk.import_dmabuf_as_texture(&dmabuf).expect("import (miss)");
    let cached = t1.clone();
    assert_eq!(
        vk.dmabuf_import_cache_len(),
        1,
        "a miss populates the cache"
    );
    assert_eq!(
        vk.pending_dmabuf_acquires_len(),
        0,
        "a miss runs its own import barrier and queues no deferred acquire",
    );

    // Frame 1 samples the imported buffer → green.
    let f1 = render_client(&mut vk, t1);
    let c1 = px(&f1, W / 2, H / 2);
    assert!(
        close_px(c1, [0, 255, 0, 255], 40),
        "frame 1 should sample green, got {c1:?}"
    );
    assert_eq!(
        vk.pending_dmabuf_acquires_len(),
        0,
        "no deferred acquire outstanding after the miss frame",
    );

    // Producer frame 2: rewrite the SAME dmabuf to solid red, then re-import — a cache HIT that
    // queues exactly one deferred re-acquire (not run here).
    fb.refill([[255, 0, 0, 255]; 4]).expect("refill red");
    let t2 = vk
        .import_dmabuf_as_texture(&dmabuf)
        .expect("re-import (hit)");
    assert!(
        cached.same_image(&t2),
        "a recycled buffer must hit the cache (same image)"
    );
    assert_eq!(
        vk.dmabuf_import_cache_len(),
        1,
        "a hit must not grow the cache"
    );
    assert_eq!(
        vk.pending_dmabuf_acquires_len(),
        1,
        "a hit queues exactly one deferred re-acquire",
    );

    // Frame 2: `begin()` folds the deferred acquire into the frame's command buffer before the
    // render pass, so the sampler sees the re-committed RED content — and the queue is drained.
    let f2 = render_client(&mut vk, t2);
    let c2 = px(&f2, W / 2, H / 2);
    assert!(
        close_px(c2, [255, 0, 0, 255], 40),
        "frame 2 should sample the re-committed red content, got {c2:?}",
    );
    assert_eq!(
        vk.pending_dmabuf_acquires_len(),
        0,
        "VulkanFrame::begin must drain the deferred acquire",
    );
    eprintln!(
        "vulkan_dmabuf_import_cache_defers_reacquire_into_the_frame: frame1 center={c1:?} \
         frame2 center={c2:?}",
    );
}

// --- shm per-surface cache: an in-place re-upload overwrites the reused VkImage ------------------

/// The shm cache (`import_shm_buffer`) keeps a client's `VkTexture` across commits and re-uploads
/// the new contents *in place* via `reupload_shm` — grow-only staging feeding `Texture::
/// reupload_full`'s layout dance (UNDEFINED→TRANSFER_DST, full buffer→image copy, →SHADER_READ) —
/// instead of allocating a fresh image every frame. Pin that the in-place re-upload actually lands
/// the new pixels *and* leaves the same image sampleable: import an opaque-red source, re-upload it
/// green into the very same `VkTexture`, then sample it 1:1 into an offscreen and read back green.
/// A no-op or stale re-upload (or a botched layout transition) would read back the original red.
#[test]
fn vulkan_shm_reupload_overwrites_in_place() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_shm_reupload_overwrites_in_place: no Vulkan device ({e})");
            return;
        }
    };

    const RED: [u8; 4] = [220, 30, 30, 255];
    const GREEN: [u8; 4] = [30, 200, 60, 255];

    // Import an opaque-red source; `import_memory` leaves it in SHADER_READ_ONLY_OPTIMAL, directly
    // sampleable — the same state a cached shm texture sits in between commits.
    let tex = vk
        .import_memory(
            &solid_texels(RED),
            Fourcc::Abgr8888,
            Size::from((W, H)),
            false,
        )
        .expect("import red source");

    // Re-upload green into the SAME VkImage (the cache-reuse path), allocating no new image.
    vk.reupload_shm(&tex, &solid_texels(GREEN))
        .expect("reupload green");

    // Sample the re-uploaded texture 1:1 into an offscreen and read it back.
    let size = Size::<i32, Physical>::from((W, H));
    let mut target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((W, H)))
        .expect("offscreen");
    {
        let mut fb = vk.bind(&mut target).expect("bind");
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .clear(Color32F::from(CLEAR), &[Rectangle::from_size(size)])
            .expect("clear");
        let full_src = Rectangle::<f64, BufferCoord>::from_size(Size::from((W as f64, H as f64)));
        let full_dst = Rectangle::<i32, Physical>::from_size(size);
        frame
            .render_texture_from_to(
                &tex,
                full_src,
                full_dst,
                &[full_dst],
                &[],
                Transform::Normal,
                1.0,
            )
            .expect("sample re-uploaded texture");
        let _sync = frame.finish().expect("finish");
    }

    let fb = vk.bind(&mut target).expect("rebind for readback");
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((W, H)));
    let mapping = vk
        .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
        .expect("copy_framebuffer");
    let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

    // The whole quad must be the re-uploaded green, and demonstrably not the original red.
    let center = px(&pixels, W / 2, H / 2);
    assert!(
        close_px(center, GREEN, 3),
        "re-upload should overwrite the image with green, got {center:?}",
    );
    assert!(
        !close_px(center, RED, 40),
        "re-upload must not leave the original red behind, got {center:?}",
    );
}

/// Binding two **differently-sized** `Argb8888` targets in the same frame must not reallocate the
/// present-blit shadow each time. Argb/Xrgb targets do not match the R8G8B8A8 render pass, so they
/// render into a shadow that is blitted into the dmabuf on `finish`; the shadow used to live in a
/// single size-keyed slot, which is only safe while exactly one such size is ever bound.
///
/// A live session binds several: each output's scanout buffer, a screencast buffer (a window cast
/// is sized to the window's bbox, a rotated output's cast is transform-sized), and any screencopy
/// region. Alternating those through one slot reallocates a full target-sized device image on
/// *every* bind — on Venus that per-frame allocation churn exhausts the host blob pool, poisons the
/// guest↔host ring and `abort()`s the session.
///
/// Pixels cannot see this (every frame renders correctly either way — it just leaks work), so the
/// invariant is pinned by counting allocations: after each size has been bound once, the count must
/// stop growing. Venus-only (needs GBM).
#[test]
fn vulkan_alternating_present_blit_sizes_reuse_shadows() {
    use std::fs::File;

    use smithay::backend::allocator::dmabuf::AsDmabuf;
    use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
    use smithay::backend::allocator::{Allocator, Modifier};

    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_alternating_present_blit_sizes: no Vulkan device ({e})");
            return;
        }
    };
    let file = match File::options()
        .read(true)
        .write(true)
        .open("/dev/dri/renderD128")
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("skipping vulkan_alternating_present_blit_sizes: no render node ({e})");
            return;
        }
    };
    let gbm = match GbmDevice::new(file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skipping vulkan_alternating_present_blit_sizes: no GBM device ({e})");
            return;
        }
    };
    let mut alloc = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);

    // Two sizes, as a scanout buffer and a window-sized screencast buffer would be. Argb8888 (the
    // KMS primary-plane byte order) is what takes the present-blit path.
    let mut make = |w: i32, h: i32| {
        alloc
            .create_buffer(w as u32, h as u32, Fourcc::Argb8888, &[Modifier::Linear])
            .map(|bo| bo.export().expect("export dmabuf"))
    };
    let (big, small) = match (make(256, 128), make(96, 64)) {
        (Ok(big), Ok(small)) => (big, small),
        _ => {
            eprintln!(
                "skipping vulkan_alternating_present_blit_sizes: GBM cannot allocate Argb8888 \
                 LINEAR"
            );
            return;
        }
    };
    let mut targets = [
        (big, Size::<i32, Physical>::from((256, 128))),
        (small, Size::<i32, Physical>::from((96, 64))),
    ];

    // Frame 1 binds each size once: two shadows, two allocations. Every frame after reuses them.
    const FRAMES: usize = 6;
    for frame_no in 0..FRAMES {
        for (dmabuf, size) in &mut targets {
            let mut fb = vk.bind(dmabuf).expect("bind");
            let mut frame = vk
                .render(&mut fb, *size, Transform::Normal)
                .expect("render");
            frame
                .clear(
                    Color32F::from([0., 0., 1., 1.]),
                    &[Rectangle::from_size(*size)],
                )
                .expect("clear");
            let _ = frame.finish().expect("finish");
        }
        assert_eq!(
            vk.present_blit_shadow_allocs(),
            2,
            "frame {frame_no}: each bound size must allocate its shadow once and reuse it \
             thereafter; a growing count is the per-frame churn that aborts Venus",
        );
    }
}

/// The `Xrgb8888` shm pool that `render_to_shm` fills wants BGRA-order bytes, but the owned
/// renderer renders `R8G8B8A8` and cannot produce a BGRA-order offscreen at all — the render pass
/// is hard-wired to that format, so a BGRA attachment is not a legal framebuffer.
///
/// The conversion therefore happens on the way *out*: `copy_framebuffer` blits through a staging
/// image of the requested format and `vkCmdBlitImage` reorders the channels on the GPU. Pin both
/// halves: that a BGRA offscreen really is still rejected (so nobody "fixes" it the wrong way), and
/// that asking for BGRA bytes yields them — with no CPU pass over the pixels.
///
/// Red is the discriminator: it is the channel a red/blue swap moves.
#[test]
fn vulkan_shm_readback_converts_to_bgra() {
    use crate::render_helpers::create_texture;

    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_shm_readback_converts_to_bgra: no Vulkan device ({e})");
            return;
        }
    };
    let size = Size::<i32, Physical>::from((W, H));

    // The BGRA-order offscreen the shm pool's format would ask for: unsupported, by construction.
    assert!(
        Offscreen::<VkTexture>::create_buffer(
            &mut vk,
            Fourcc::Xrgb8888,
            size.to_logical(1).to_buffer(1, Transform::Normal),
        )
        .is_err(),
        "a BGRA-order offscreen must stay rejected: the shared render pass is R8G8B8A8, so such an \
         attachment is not legal. The conversion belongs in the readback, not the render target.",
    );

    // The RGBA-order one we actually use. Clear it red: red is what a red/blue swap moves.
    let mut texture: VkTexture =
        create_texture(&mut vk, size, Fourcc::Abgr8888).expect("Abgr8888 offscreen");
    {
        let mut fb = vk.bind(&mut texture).expect("bind");
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .clear(
                Color32F::from([1., 0., 0., 1.]),
                &[Rectangle::from_size(size)],
            )
            .expect("clear");
        let _ = frame.finish().expect("finish");
    }
    let fb = vk.bind(&mut texture).expect("rebind for readback");

    // Negative control for the conversion: asking for the source's own order must NOT convert.
    let mapping = vk
        .copy_framebuffer(&fb, Rectangle::from_size((W, H).into()), Fourcc::Abgr8888)
        .expect("copy_framebuffer");
    let rgba = vk.map_texture(&mapping).expect("map_texture").to_vec();
    assert_eq!(
        &rgba[..4],
        &[255, 0, 0, 255],
        "reading the source's own order must come back raw, not double-swapped",
    );

    // ...and asking for the shm pool's order converts, on the GPU.
    let mapping = vk
        .copy_framebuffer(&fb, Rectangle::from_size((W, H).into()), Fourcc::Xrgb8888)
        .expect("copy_framebuffer as Xrgb8888");
    let bgra = vk.map_texture(&mapping).expect("map_texture").to_vec();
    assert_eq!(
        &bgra[..4],
        &[0, 0, 255, 255],
        "a BGRA-order readback of red must have red in the third byte",
    );
}

/// A converting readback must not allocate a staging image per call.
///
/// shm screencopy fires every frame, and on Venus a `VkImage` allocated per frame exhausts the host
/// blob pool and aborts the session. The cache hit is invisible to any pixel assertion, so the
/// allocation counter is the only thing that can pin this. Sibling of
/// `vulkan_alternating_present_blit_sizes_reuse_shadows`.
#[test]
fn vulkan_repeated_converting_readbacks_reuse_staging() {
    use crate::render_helpers::create_texture;

    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "skipping vulkan_repeated_converting_readbacks_reuse_staging: no Vulkan ({e})"
            );
            return;
        }
    };
    let size = Size::<i32, Physical>::from((W, H));
    let mut texture: VkTexture =
        create_texture(&mut vk, size, Fourcc::Abgr8888).expect("Abgr8888 offscreen");

    for _ in 0..8 {
        let fb = vk.bind(&mut texture).expect("bind");
        let mapping = vk
            .copy_framebuffer(&fb, Rectangle::from_size((W, H).into()), Fourcc::Xrgb8888)
            .expect("converting copy_framebuffer");
        let _ = vk.map_texture(&mapping).expect("map_texture");
    }

    assert_eq!(
        vk.readback_staging_allocs(),
        1,
        "a converting readback of one size must allocate its staging image once, not per call \
         (this is what aborts Venus)",
    );

    // The host-visible buffer the pixels land in is the *other* per-call allocation, and the more
    // dangerous one: on Venus host-visible memory is a mappable blob.
    assert_eq!(
        vk.readback_buffer_allocs(),
        1,
        "the host readback buffer must be allocated once and reused, not per call",
    );
}

/// The host readback buffer grows on demand and is never reallocated for a size it already covers —
/// so alternating a big and a small readback (a full-screen screencopy and a cursor bitmap, say)
/// must not churn a mappable blob per frame.
#[test]
fn vulkan_readback_host_buffer_grows_then_reuses() {
    use crate::render_helpers::create_texture;

    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_readback_host_buffer_grows_then_reuses: no Vulkan ({e})");
            return;
        }
    };

    let read = |vk: &mut VulkanRenderer, w: i32, h: i32| {
        let size = Size::<i32, Physical>::from((w, h));
        let mut texture: VkTexture = create_texture(vk, size, Fourcc::Abgr8888).expect("offscreen");
        let fb = vk.bind(&mut texture).expect("bind");
        let mapping = vk
            .copy_framebuffer(&fb, Rectangle::from_size((w, h).into()), Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let _ = vk.map_texture(&mapping).expect("map_texture");
    };

    read(&mut vk, 32, 32);
    assert_eq!(vk.readback_buffer_allocs(), 1, "first read allocates");

    read(&mut vk, 32, 32);
    assert_eq!(vk.readback_buffer_allocs(), 1, "same size must reuse");

    read(&mut vk, 128, 128);
    assert_eq!(
        vk.readback_buffer_allocs(),
        2,
        "a larger read must grow it once"
    );

    // The small read now fits in the grown buffer, and the large one is already covered: neither
    // may reallocate again, however many times they alternate.
    for _ in 0..4 {
        read(&mut vk, 32, 32);
        read(&mut vk, 128, 128);
    }
    assert_eq!(
        vk.readback_buffer_allocs(),
        2,
        "alternating sizes must reuse the grown buffer, not churn a blob per frame",
    );
}
