//! A/B equivalence test: render the same scene (clear + a solid quad + a memory texture) through
//! both the [`VulkanRenderer`] and Smithay's CPU `PixmanRenderer`, offscreen, and assert the
//! read-back pixels match within tolerance.
//!
//! Pixman is a deterministic, GPU-free reference implementation of the exact renderer traits, so
//! it makes an ideal oracle: the Pixman side needs no device, and the Vulkan side guard-skips when
//! no Vulkan device is present. Runs on Venus (real target) and lavapipe (deterministic CPU).

use niri_config::{Color, CornerRadius, GradientInterpolation};
use niri_vk::render::PostprocessPush;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element, Kind, RenderElement};
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::renderer::{
    Bind, Color32F, ExportMem, Frame, ImportMem, Offscreen, Renderer,
};
use smithay::utils::{
    Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Scale, Size, Transform,
};

use super::{VkTexture, VulkanRenderer};
use crate::niri::OutputRenderElements;
use crate::render_helpers::blur::BlurOptions;
use crate::render_helpers::border::BorderRenderElement;
use crate::render_helpers::gradient_fade_texture::GradientFadeTextureRenderElement;
use crate::render_helpers::offscreen::OffscreenBuffer;
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
            niri_scale: 1.0,
            niri_alpha: 1.0,
            saturation: 0.3,
            noise: 0.0,
            // origin/size/target/src_rect are filled by render_postprocess.
            ..Default::default()
        };
        frame
            .render_postprocess(&source, full_src, full_dst, push)
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
