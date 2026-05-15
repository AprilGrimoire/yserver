use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    sync::Arc,
};

use crate::kms::cpu_types::{PictTransform, Rectangle16, Repeat};
use yserver_core::{
    backend::{
        AnyHandle, Backend, ClipState, CursorHandle, DrawState, FillState, FontHandle, GcFunction,
        GlyphSetHandle, OriginContext, PictureHandle, PixmapHandle, WindowHandle,
    },
    host_x11::{
        HostKeyEvent, HostPointerEvent, HostSubwindowConfig, HostSubwindowVisual, HostXidMap,
        PointerEventKind, PointerPosition,
    },
};
use yserver_protocol::x11::{
    CharInfo as ProtocolCharInfo, ClipRectangles, FontMetrics, RENDER_FMT_A1, RENDER_FMT_A8,
    RENDER_FMT_ARGB32, ResourceId, xfixes,
};

use crate::drm;

/// Newtype wrapper around `freetype::Face`.
/// `repr(transparent)` is required so `RefCell::as_ptr` can be safely cast
/// from `*mut FreetypeFace` to `*mut freetype::Face` in `render_text_string`.
/// SAFETY: All access is on the single-threaded core thread.
/// Single-threaded context makes this sound. `Face` contains raw pointers
/// and `Rc<Vec<u8>>` by default, both `!Send`.
#[repr(transparent)]
pub struct FreetypeFace(#[allow(dead_code)] pub freetype::Face);
unsafe impl Send for FreetypeFace {}

/// Newtype wrapper around `xkb::Context`.
/// SAFETY: All access is on the single-threaded core thread.
/// The raw pointer in xkbcommon is not `Send`, but the C library is thread-safe.
pub struct XkbContext(pub xkbcommon::xkb::Context);
unsafe impl Send for XkbContext {}

/// Newtype wrapper around `xkb::Keymap`.
/// SAFETY: All access is on the single-threaded core thread.
pub struct XkbKeymap(pub xkbcommon::xkb::Keymap);
unsafe impl Send for XkbKeymap {}

/// Newtype wrapper around `xkb::State`.
/// SAFETY: All access is on the single-threaded core thread.
pub struct XkbState(pub xkbcommon::xkb::State);
unsafe impl Send for XkbState {}

/// Owning snapshot of a drawable's mirror pixels, produced by
/// [`KmsBackend::read_mirror_pixels`]. Tightly packed (no row pad),
/// `bytes_per_pixel` ∈ {1, 4} matching the mirror's Vulkan format
/// (`R8_UNORM` for depth 1/8, `B8G8R8A8_UNORM` for depth 24/32).
struct MirrorReadback {
    width: u32,
    height: u32,
    bytes_per_pixel: u32,
    bytes: Vec<u8>,
}

/// One entry in the [`AliasRegistry`]: tracks the refcount on a
/// `Composite::NameWindowPixmap` backing pixmap, plus the
/// width/height/depth snapshot the alias was created against.
///
/// Refcount sources, per the L2 spec:
///   1. Active redirect on the matching window
///      (`Window.redirected_backing`).
///   2. Each live `NameWindowPixmap` alias on this backing.
/// The backing drops when refcount reaches zero — Unredirect
/// without aliases (1 → 0), DestroyWindow with surviving aliases
/// (1 → 0 from owner; aliases keep it alive), final FreePixmap of
/// the last alias (after Unredirect or Destroy).
#[allow(dead_code, reason = "width/height/depth consumed by B.6d on resize")]
#[derive(Clone, Copy, Debug)]
pub struct AliasEntry {
    pub refcount: u32,
    pub width: u16,
    pub height: u16,
    pub depth: u8,
}

/// Refcounted backing-pixmap registry on the KMS backend. Keyed by
/// the backing's `host_xid` (the `PixmapHandle` the protocol layer
/// sees through `Window.redirected_backing.host_pixmap`).
/// L2 plan B.3.
#[derive(Default, Debug)]
pub struct AliasRegistry {
    entries: std::collections::HashMap<u32, AliasEntry>,
}

#[allow(
    dead_code,
    reason = "get/decref/len/is_empty consumed by B.6c (Unredirect teardown) + B.10c (overlay-demote quiescence); B.6d uses the width/height fields for resize check"
)]
impl AliasRegistry {
    /// Add the initial entry for a backing pixmap. Caller seeds the
    /// `refcount` (typically 1 for the redirect-activation hold).
    pub fn insert(&mut self, host_pixmap: PixmapHandle, entry: AliasEntry) {
        self.entries.insert(host_pixmap.as_raw(), entry);
    }

    /// Refcount-only lookup; returns `None` if the backing isn't
    /// tracked (e.g. the redirect was torn down).
    #[must_use]
    pub fn get(&self, host_pixmap: PixmapHandle) -> Option<&AliasEntry> {
        self.entries.get(&host_pixmap.as_raw())
    }

    /// Bump the refcount. Silently no-ops on an unknown handle — the
    /// caller is expected to have inserted the entry first
    /// (`Composite::NameWindowPixmap`'s ordering guarantees that).
    pub fn incref(&mut self, host_pixmap: PixmapHandle) {
        if let Some(e) = self.entries.get_mut(&host_pixmap.as_raw()) {
            e.refcount = e.refcount.saturating_add(1);
        }
    }

    /// Drop one reference. Returns `true` if the entry was removed
    /// (refcount reached zero). Returns `false` on an unknown handle
    /// or when refs remain — caller uses the return value to decide
    /// whether to release the underlying Vulkan image.
    pub fn decref(&mut self, host_pixmap: PixmapHandle) -> bool {
        let key = host_pixmap.as_raw();
        let Some(entry) = self.entries.get_mut(&key) else {
            return false;
        };
        entry.refcount = entry.refcount.saturating_sub(1);
        if entry.refcount == 0 {
            self.entries.remove(&key);
            true
        } else {
            false
        }
    }

    /// Number of tracked backings. Mostly for test introspection.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True iff no backings are tracked. Used by overlay-demotion
    /// quiescence checks (B.10c).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Axis-aligned overlap test for two rects of the same size in
/// the same coordinate space. `vkCmdCopyImage` is UB when src and
/// dst rects on the same image overlap; the
/// [`KmsBackend::try_vk_copy_area`] path uses this to decide
/// whether to take the same-image fast path or fall back to
/// pixman (the staging-image path is a 4.1.4.2 follow-up).
fn rects_overlap_axis_aligned(
    src_x: i32,
    src_y: i32,
    dst_x: i32,
    dst_y: i32,
    width: i32,
    height: i32,
) -> bool {
    let sx0 = src_x;
    let sy0 = src_y;
    let sx1 = src_x.saturating_add(width);
    let sy1 = src_y.saturating_add(height);
    let dx0 = dst_x;
    let dy0 = dst_y;
    let dx1 = dst_x.saturating_add(width);
    let dy1 = dst_y.saturating_add(height);
    sx0 < dx1 && dx0 < sx1 && sy0 < dy1 && dy0 < sy1
}

/// Translate a list of GC-clipped dst sub-rects into
/// `VkImageCopy` regions. Each sub-rect's src offset shifts by
/// the same amount the dst was clipped (mirrors the existing
/// pixman per-sub-rect path). Sub-rects clipped to either image's
/// extent.
fn build_image_copy_regions(
    sub_rects: &[Rectangle16],
    orig_src_x: i16,
    orig_src_y: i16,
    orig_dst_x: i16,
    orig_dst_y: i16,
    src_extent: ash::vk::Extent2D,
    dst_extent: ash::vk::Extent2D,
) -> Vec<ash::vk::ImageCopy> {
    let mut out = Vec::with_capacity(sub_rects.len());
    let subresource = ash::vk::ImageSubresourceLayers::default()
        .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
        .layer_count(1);
    for r in sub_rects {
        let dx_shift = i32::from(r.x) - i32::from(orig_dst_x);
        let dy_shift = i32::from(r.y) - i32::from(orig_dst_y);
        let src_off_x = i32::from(orig_src_x) + dx_shift;
        let src_off_y = i32::from(orig_src_y) + dy_shift;
        let dst_off_x = i32::from(r.x);
        let dst_off_y = i32::from(r.y);

        // Clip widths/heights to fit both src and dst extents.
        let max_w_src = i32::try_from(src_extent.width).unwrap_or(i32::MAX) - src_off_x.max(0);
        let max_h_src = i32::try_from(src_extent.height).unwrap_or(i32::MAX) - src_off_y.max(0);
        let max_w_dst = i32::try_from(dst_extent.width).unwrap_or(i32::MAX) - dst_off_x.max(0);
        let max_h_dst = i32::try_from(dst_extent.height).unwrap_or(i32::MAX) - dst_off_y.max(0);

        let req_w = i32::from(r.width);
        let req_h = i32::from(r.height);
        let w = req_w.min(max_w_src).min(max_w_dst);
        let h = req_h.min(max_h_src).min(max_h_dst);
        if w <= 0 || h <= 0 || src_off_x < 0 || src_off_y < 0 || dst_off_x < 0 || dst_off_y < 0 {
            continue;
        }
        out.push(
            ash::vk::ImageCopy::default()
                .src_subresource(subresource)
                .src_offset(ash::vk::Offset3D {
                    x: src_off_x,
                    y: src_off_y,
                    z: 0,
                })
                .dst_subresource(subresource)
                .dst_offset(ash::vk::Offset3D {
                    x: dst_off_x,
                    y: dst_off_y,
                    z: 0,
                })
                .extent(ash::vk::Extent3D {
                    width: w as u32,
                    height: h as u32,
                    depth: 1,
                }),
        );
    }
    out
}

fn read_mirror_pixel_for_plane(rb: &MirrorReadback, depth: u8, x: usize, y: usize) -> u32 {
    let w = rb.width as usize;
    let h = rb.height as usize;
    if x >= w || y >= h {
        return 0;
    }
    let idx = y * w + x;
    match (depth, rb.bytes_per_pixel) {
        // Depth-1 sources land in `R8_UNORM` with one byte per pixel
        // (0xFF or 0x00 — see the depth-1 branch of `try_vk_put_image`).
        // Any non-zero counts as set.
        (1, 1) => u32::from(rb.bytes[idx] != 0),
        (8, 1) => u32::from(rb.bytes[idx]),
        // BGRA mirror in memory order [B, G, R, A] reads as the same
        // little-endian u32 word that the legacy pixman path returned.
        (24 | 32, 4) => {
            let off = idx * 4;
            u32::from_le_bytes([
                rb.bytes[off],
                rb.bytes[off + 1],
                rb.bytes[off + 2],
                rb.bytes[off + 3],
            ])
        }
        _ => 0,
    }
}

/// Convert an X11 24-bit pixel (0xRRGGBB) to a Pixman Color.
/// Append 1×1 rects covering a Bresenham line from (x0,y0) to (x1,y1).
fn bresenham_segment(x0: i32, y0: i32, x1: i32, y1: i32, out: &mut Vec<Rectangle16>) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let (mut x, mut y) = (x0, y0);
    loop {
        out.push(Rectangle16 {
            x: x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            y: y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            width: 1,
            height: 1,
        });
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// Scanline fill a polygon (even-odd rule).  Edges are pairs of i32
/// vertices.  Output is a Vec of 1-pixel-tall horizontal Rectangle16 spans.
fn scanline_fill_polygon(verts: &[(i32, i32)], out: &mut Vec<Rectangle16>) {
    if verts.len() < 3 {
        return;
    }
    let y_min = verts.iter().map(|&(_, y)| y).min().unwrap();
    let y_max = verts.iter().map(|&(_, y)| y).max().unwrap();
    let mut crossings: Vec<i32> = Vec::with_capacity(verts.len());
    for y in y_min..=y_max {
        crossings.clear();
        for i in 0..verts.len() {
            let (x0, y0) = verts[i];
            let (x1, y1) = verts[(i + 1) % verts.len()];
            // Skip horizontal edges; use half-open [min_y, max_y) so
            // shared vertices contribute exactly once across two edges.
            let (ya, yb) = if y0 < y1 { (y0, y1) } else { (y1, y0) };
            if ya == yb || y < ya || y >= yb {
                continue;
            }
            // Linear interpolation: x at scanline y.
            let x = x0 as i64 + ((y - y0) as i64 * (x1 - x0) as i64) / (y1 - y0) as i64;
            crossings.push(x as i32);
        }
        crossings.sort_unstable();
        let mut i = 0;
        while i + 1 < crossings.len() {
            let x_start = crossings[i];
            let x_end = crossings[i + 1];
            if x_end > x_start {
                let w = (x_end - x_start) as i64;
                out.push(Rectangle16 {
                    x: x_start.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    y: y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    width: w.min(u16::MAX as i64) as u16,
                    height: 1,
                });
            }
            i += 2;
        }
    }
}

/// Clip a list of `Rectangle16` to the bounds `[0, iw) × [0, ih)` and drop
/// rects that fall entirely outside.  Pixman's `fill_rectangles` is supposed
/// to clip on its own but in our build a partially-out-of-bounds rect
/// (especially with negative x/y) can segfault; pre-clipping is the cheap
/// defensive workaround.
fn clip_rects_to_image(rects: &[Rectangle16], iw: i32, ih: i32) -> Vec<Rectangle16> {
    let mut out = Vec::with_capacity(rects.len());
    for r in rects {
        let x1 = (r.x as i32).max(0);
        let y1 = (r.y as i32).max(0);
        let x2 = ((r.x as i32) + r.width as i32).min(iw);
        let y2 = ((r.y as i32) + r.height as i32).min(ih);
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        out.push(Rectangle16 {
            x: x1 as i16,
            y: y1 as i16,
            width: (x2 - x1) as u16,
            height: (y2 - y1) as u16,
        });
    }
    out
}

/// Resolved RENDER picture for the 4.1.4.6 Vulkan path. Either a
/// drawable mirror, a 1×1 SolidFill colour (already premul), or
/// "no picture" (only valid for the mask slot — collapses to the
/// backend-shared white-mask scratch in the recorder).
#[derive(Debug, Clone, Copy)]
enum RenderPic {
    Drawable(u32),
    Solid([f32; 4]),
    Gradient(u32),
    None,
}

/// Resolve a RENDER picture into a [`RenderPic`] for the Vulkan
/// composite path. `component_alpha` is supported on the mask side
/// via the dual-source-blend pipeline (see `render.frag.glsl`
/// `COMPONENT_ALPHA = 1`); the caller picks up the mask picture's
/// `component_alpha` flag separately. `alpha_map` is still
/// unsupported — falls back to pixman.
fn resolve_render_pic(picture: Option<&PictureState>) -> Option<RenderPic> {
    match picture {
        Some(PictureState::Drawable {
            host_xid,
            alpha_map: None,
            ..
        }) => Some(RenderPic::Drawable(*host_xid)),
        Some(PictureState::SolidFill { premul, .. }) => Some(RenderPic::Solid(*premul)),
        // Gradients carry their host_xid in `RenderPic::Gradient` so
        // the Vk path can look up the `GradientPicture` from
        // `self.pictures`. We can't store a borrowed reference here
        // because `RenderPic` outlives the `pictures.get(&xid)` borrow.
        _ => None,
    }
}

/// Resolve a `RenderPic` plus its picture XID. Used by the gradient
/// branch which needs to look the picture back up after dropping the
/// pictures borrow.
fn resolve_render_pic_with_gradient_xid(
    pictures: &HashMap<u32, PictureState>,
    pic_xid: u32,
) -> Option<RenderPic> {
    match pictures.get(&pic_xid) {
        Some(PictureState::Gradient { .. }) => Some(RenderPic::Gradient(pic_xid)),
        other => resolve_render_pic(other),
    }
}

/// Translate a [`Repeat`] enum value to the integer constant the
/// `render.frag.glsl` shader expects (matches the protocol numbering;
/// see `render_pipeline::REPEAT_*`).
fn repeat_to_shader_const(repeat: Repeat) -> i32 {
    use crate::kms::vk::render_pipeline::{REPEAT_NONE, REPEAT_NORMAL, REPEAT_PAD, REPEAT_REFLECT};
    match repeat {
        Repeat::None => REPEAT_NONE,
        Repeat::Normal => REPEAT_NORMAL,
        Repeat::Pad => REPEAT_PAD,
        Repeat::Reflect => REPEAT_REFLECT,
    }
}

/// Convert an X11 RENDER pixman 3×3 transform (16.16 fixed-point) into
/// the affine 2×3 form the `render.frag.glsl` shader uses. RENDER's
/// transform maps the *destination*-relative source coordinate to the
/// pre-sample source pixel:
///
/// ```text
///   src_pixel = M * (src_origin + dst_offset, 1)
/// ```
///
/// We assume affine — the bottom row is `[0, 0, 1]`. Real X11 clients
/// use affine transforms in practice; projective transforms (the rare
/// case) round-trip through the affine portion only and produce wrong
/// pixels at the perspective-divide corners. That trade-off is
/// documented in `feedback_phase4_1_4_decisions.md` § component-alpha
/// and matches the per-family-port strict-acceptance relaxation.
/// Compose two affine 2×3 transforms. The result satisfies
/// `compose(A, B) * v == A * (B * v)` when `v` is `(x, y, 1)`.
fn compose_affines(
    a: crate::kms::vk::ops::render::AffineXform,
    b: crate::kms::vk::ops::render::AffineXform,
) -> crate::kms::vk::ops::render::AffineXform {
    use crate::kms::vk::ops::render::AffineXform;
    // a.row0 = (a00, a01, a02), a.row1 = (a10, a11, a12). Bottom row
    // implicit `[0, 0, 1]`. Same for b.
    let a00 = a.row0[0];
    let a01 = a.row0[1];
    let a02 = a.row0[2];
    let a10 = a.row1[0];
    let a11 = a.row1[1];
    let a12 = a.row1[2];
    let b00 = b.row0[0];
    let b01 = b.row0[1];
    let b02 = b.row0[2];
    let b10 = b.row1[0];
    let b11 = b.row1[1];
    let b12 = b.row1[2];
    AffineXform {
        row0: [
            a00 * b00 + a01 * b10,
            a00 * b01 + a01 * b11,
            a00 * b02 + a01 * b12 + a02,
            0.0,
        ],
        row1: [
            a10 * b00 + a11 * b10,
            a10 * b01 + a11 * b11,
            a10 * b02 + a11 * b12 + a12,
            0.0,
        ],
    }
}

fn pixman_transform_to_affine(
    transform: Option<&PictTransform>,
    _src_extent: ash::vk::Extent2D,
) -> crate::kms::vk::ops::render::AffineXform {
    use crate::kms::vk::ops::render::AffineXform;
    let Some(t) = transform else {
        return AffineXform::IDENTITY;
    };
    // pixman_transform stores 9 fixed-point i32 values in row-major
    // order. matrix[row][col] in 16.16 fixed point.
    let m = t.matrix;
    let to_f = |v: i32| (v as f32) / 65536.0;
    let mut a = to_f(m[0][0]);
    let mut b = to_f(m[0][1]);
    let mut tx = to_f(m[0][2]);
    let mut c = to_f(m[1][0]);
    let mut d = to_f(m[1][1]);
    let mut ty = to_f(m[1][2]);
    // Constant-divisor projective transforms (matrix row 2 = `[0, 0, w]`
    // with w ≠ 1) collapse to a uniform 1/w scale on the affine portion.
    // Rendercheck's tscoords/tmcoords cases use this form to scale a 5×5
    // src 8×; pixman handles it the same way. Non-constant projective
    // transforms (m[2][0] or m[2][1] non-zero) genuinely vary per-pixel
    // and we don't model them — the affine portion is used as-is, which
    // matches the strict-acceptance relaxation in
    // `feedback_phase4_1_4_decisions.md`.
    let m20 = to_f(m[2][0]);
    let m21 = to_f(m[2][1]);
    let m22 = to_f(m[2][2]);
    if m20 == 0.0 && m21 == 0.0 && m22 != 0.0 && m22 != 1.0 {
        let inv = 1.0 / m22;
        a *= inv;
        b *= inv;
        tx *= inv;
        c *= inv;
        d *= inv;
        ty *= inv;
    }
    AffineXform {
        row0: [a, b, tx, 0.0],
        row1: [c, d, ty, 0.0],
    }
}

/// One glyph's CPU-side rasterisation result, ready for either the
/// pixman compositor or the Phase 4.1.4.5 Vulkan text-run path.
/// `pixels` is row-major, tightly packed (w × h alpha bytes), the
/// FreeType `BITMAP_GRAY` layout.
struct RenderedGlyph {
    dst_x: i32,
    dst_y: i32,
    w: usize,
    h: usize,
    pixels: Vec<u8>,
    /// X11 character advance from the font's char_info_cache.
    /// Currently only used by callers' pen tracking (which lives
    /// outside this struct); kept here for future per-glyph
    /// kerning if it lands.
    #[allow(dead_code)]
    advance: i32,
    /// Unicode codepoint — used as part of the glyph atlas key.
    codepoint: u32,
}

// `fill_rects_with_gc_function` (pixman per-pixel GC-function fill)
// deleted in 4.1.5. Every callsite now routes through
// `KmsBackend::try_vk_fill_with_function`, which uses the
// `LogicFillPipelineCache` for non-Copy functions.

/// Parse a packed pair of i16 values (2 bytes each) from a byte slice.
fn read_i16_pair(data: &[u8], offset: usize) -> Option<(i16, i16)> {
    if offset + 4 > data.len() {
        return None;
    }
    let x = i16::from_le_bytes([data[offset], data[offset + 1]]);
    let y = i16::from_le_bytes([data[offset + 2], data[offset + 3]]);
    Some((x, y))
}

/// Parse a packed rectangle (x:i16, y:i16, w:u16, h:u16) from a byte slice.
fn read_rect(data: &[u8], offset: usize) -> Option<Rectangle16> {
    if offset + 8 > data.len() {
        return None;
    }
    let x = i16::from_le_bytes([data[offset], data[offset + 1]]);
    let y = i16::from_le_bytes([data[offset + 2], data[offset + 3]]);
    let w = u16::from_le_bytes([data[offset + 4], data[offset + 5]]);
    let h = u16::from_le_bytes([data[offset + 6], data[offset + 7]]);
    Some(Rectangle16 {
        x,
        y,
        width: w,
        height: h,
    })
}

/// A simple integer rectangle in virtual-screen coordinates.
///
/// Used by `OutputLayout::rect()` to support the per-output window
/// pre-filter introduced in Step 4 of the multi-monitor work. We use
/// `i32` widths here (not `u16`) because the virtual-screen extent can
/// exceed `u16::MAX` for large multi-output setups.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// A single DRM output and its dedicated swapchain, positioned in the
/// virtual screen. `KmsBackend` owns one of these per discovered
/// output. `fb_w` / `fb_h` on the backend describe the virtual-screen
/// extent (max(x + width), max(y + height)); per-output dimensions
/// live here.
pub(crate) struct OutputLayout {
    pub output: crate::drm::modeset::Output,
    pub swapchain: crate::drm::Swapchain,
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    pub damage: crate::kms::scheduler::damage::OutputDamageState,
    /// Per-output ring of composite descriptor pools. Lazy-init on
    /// first composite for this output (requires `vk` + the
    /// compositor pipeline's `descriptor_set_layout`). `None` on
    /// the `for_tests` path (no Vulkan).
    pub composite_pools: Option<crate::kms::scheduler::composite_pool_ring::CompositePoolRing>,
}

impl OutputLayout {
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.x,
            y: self.y,
            w: i32::from(self.width),
            h: i32::from(self.height),
        }
    }
}

/// Per-tick aggregation of "composite deferred" events. The composite
/// path can defer a frame for two backpressure reasons: the descriptor
/// pool ring is exhausted, or no scanout BO is Free. Both happen
/// frequently in steady-state vsync (especially during startup bursts
/// like MATE session bring-up) and used to log a `warn!` per
/// occurrence — ~hundreds per second under load, drowning out real
/// signal. This struct counts them and emits one `info!` line every
/// `FLUSH_INTERVAL`. Individual events still log at `debug!`.
#[derive(Debug, Default)]
struct CompositeDeferStats {
    pool_ring_exhausted: u64,
    no_free_bo: u64,
    /// Connector name of the most recent defer; used only for the
    /// info-level summary line. Stable across the interval since
    /// MATE usually only saturates the primary output, but if multiple
    /// outputs defer the last-seen wins.
    last_output: Option<String>,
    last_flush: Option<std::time::Instant>,
}

impl CompositeDeferStats {
    const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

    fn note(&mut self, kind: CompositeDeferKind, output_name: &str) {
        match kind {
            CompositeDeferKind::PoolRingExhausted => self.pool_ring_exhausted += 1,
            CompositeDeferKind::NoFreeBo => self.no_free_bo += 1,
        }
        if self.last_output.as_deref() != Some(output_name) {
            self.last_output = Some(output_name.to_owned());
        }
    }

    /// Returns Some(summary) if at least one event has been counted
    /// and `FLUSH_INTERVAL` has elapsed since the last flush. Resets
    /// counters on emit.
    fn maybe_flush(&mut self) -> Option<String> {
        let now = std::time::Instant::now();
        let due = match self.last_flush {
            None => self.pool_ring_exhausted + self.no_free_bo > 0,
            Some(t) => {
                now.duration_since(t) >= Self::FLUSH_INTERVAL
                    && self.pool_ring_exhausted + self.no_free_bo > 0
            }
        };
        if !due {
            return None;
        }
        let summary = format!(
            "vk composite: deferred frames in last {:?}: pool_ring_exhausted={} no_free_bo={} (last_output={})",
            now.duration_since(self.last_flush.unwrap_or(now)),
            self.pool_ring_exhausted,
            self.no_free_bo,
            self.last_output.as_deref().unwrap_or("?"),
        );
        self.pool_ring_exhausted = 0;
        self.no_free_bo = 0;
        self.last_flush = Some(now);
        Some(summary)
    }
}

#[derive(Copy, Clone, Debug)]
enum CompositeDeferKind {
    PoolRingExhausted,
    NoFreeBo,
}

pub struct KmsBackend {
    // DRM (Phase 6.1 reuse)
    device: Arc<drm::Device>,
    /// Long-lived render-node fd held by the server (Phase 4.2, Task
    /// 6). Sibling of `device` on single-GPU; resolved at backend init
    /// via sysfs walk (`/sys/dev/char/<major>:<minor>/device/drm`)
    /// with a `/dev/dri/renderD*` enumeration fallback. None on the
    /// `for_tests` path. Used only as a sentinel for DRI3 availability
    /// and to keep the device referenced; `Backend::dri3_open` opens a
    /// **fresh** fd at `render_node_path` per client (libdrm_amdgpu
    /// state is per-struct-file).
    #[allow(dead_code)]
    pub(crate) render_node_fd: Option<std::os::fd::OwnedFd>,
    /// Filesystem path of the render node held in `render_node_fd`.
    /// Per-client opens in `Backend::dri3_open` go through this path
    /// so each client gets its own kernel struct file.
    pub(crate) render_node_path: Option<std::path::PathBuf>,
    /// XSync `Fence` and DRI3 `Syncobj` resources backed by
    /// `VkSemaphore` (Phase 4.2.2 Tasks 19, 20). Keyed by client XID.
    /// Each entry was imported via `kms::vk::sync::import_sync_file`
    /// (binary semaphore from a `sync_file` fd) or
    /// `import_drm_syncobj` (timeline semaphore from a DRM_SYNCOBJ
    /// fd). Dropped on DestroyFence / FreeSyncobj.
    pub(crate) dri3_sync_resources: HashMap<u32, ash::vk::Semaphore>,
    /// xshmfence-backed fences keyed by XID. Mesa's loader_dri3 uses
    /// xshmfence for `FenceFromFD` — a memfd + futex protocol,
    /// disjoint from sync_file. Vulkan can't import these, so we
    /// fall back to mmaping the fd and calling
    /// `xshmfence_trigger` directly when the X side wants to
    /// signal idle.
    pub(crate) dri3_xshmfences: HashMap<u32, crate::kms::xshmfence::FenceMapping>,
    outputs: Vec<OutputLayout>,
    fb_w: u16,
    fb_h: u16,

    // Window tracking: nested window resource ID -> local window state
    windows: HashMap<u32, WindowState>,
    next_host_xid: u32, // Monotonic counter, starts at 0x00400000

    // Stacking order for top-level windows (direct children of the root
    // container). Bottom-to-top: the last entry is on top. Updated by
    // create / destroy / reparent / configure_subwindow with stack_mode.
    // The compositor iterates this list in order; HashMap iteration is
    // unordered and doesn't preserve X11 stacking semantics.
    top_level_order: Vec<u32>,

    // Backend trait state
    window_id: u32,
    root_visual_xid: u32,
    xid_map: HostXidMap,

    // xkbcommon
    #[allow(dead_code)]
    xkb_context: XkbContext,
    xkb_keymap: XkbKeymap,
    xkb_state: XkbState,

    // libinput
    input_ctx: Option<crate::input::SendContext>,

    // Vulkan context (Phase 4.1.1: initialised but idle; pixman still
    // owns drawing). Held as an Arc so future scene-graph code can
    // share it across helpers without threading lifetime parameters.
    // Optional only so unit tests in `mod tests` can construct a
    // KmsBackend without a real Vulkan device — production paths
    // always set it via `KmsBackend::open_with_commit`.
    #[allow(dead_code)]
    pub(crate) vk: Option<Arc<crate::kms::vk::device::VkContext>>,

    // One scanout-bo pool per output (parallel to `outputs`). `None`
    // entries mean "fall back to the dumb-buffer scanout for this
    // output" — happens when Vulkan-first allocation fails for that
    // output's dimensions.
    pub(crate) scanout_pools: Vec<Option<crate::kms::vk::scanout::ScanoutBoPool>>,

    // Graphics pipeline used by the per-window composite pass
    // (sub-phase 4.1.3.4). Built once at backend init; reused every
    // frame. `None` when Vulkan didn't come up.
    #[allow(dead_code)]
    pub(crate) compositor_pipeline: Option<crate::kms::vk::pipeline::CompositorPipeline>,

    /// Frame-ownership and scheduling state. `PaintBatch` holds a
    /// `VkCommandBuffer` allocated from `ops_command_pool` below, so
    /// this field MUST be declared (and therefore dropped) BEFORE
    /// `ops_command_pool` — otherwise `OpsCommandPool::Drop` runs
    /// `destroy_command_pool` first, and the subsequent
    /// `PaintBatch::Drop`'s `free_command_buffers` call hits a
    /// dangling pool handle (driver-dependent: AMD radv panics).
    pub(crate) scheduler: crate::kms::scheduler::RenderScheduler,

    /// Backend-owned recycle pool for server-owned pixmap-backing
    /// `(VkImage, VkImageView, VkDeviceMemory)` triples (pixmap-pool
    /// T1). `free_pixmap` returns mirrors here via
    /// `defer_resource_release` adopting a `PooledPixmapReturn`; the
    /// next `CreatePixmap` of a matching `(width, height, format)`
    /// hits the pool instead of round-tripping the kernel.
    ///
    /// `None` when Vulkan didn't come up.
    ///
    /// Drop order: declared AFTER `scheduler` so any
    /// `BatchResource::release` still in-flight on the scheduler's
    /// retire path can observe a live pool. Declared BEFORE
    /// `ops_command_pool` so the pool's defensive `queue_wait_idle`
    /// in `Drop` sees a live device queue.
    pub(crate) pixmap_pool: Option<Arc<crate::kms::vk::pixmap_pool::PixmapPool>>,

    /// GPU rasterization pipeline for RENDER `Trapezoids` (gpu-trap
    /// T1/T2; triangles wired in T3). Replaces the CPU 4×4
    /// supersampled rasterizer in
    /// [`KmsBackend::try_vk_render_trapezoids_path`]; `None` when
    /// Vulkan didn't come up or the pipeline build failed at
    /// backend init (graceful: traps fall back to pixman).
    ///
    /// Drop order: declared AFTER `scheduler` and `pixmap_pool` so
    /// in-flight `PaintBatch`es that bound this pipeline see live
    /// handles until they retire; declared BEFORE `ops_command_pool`
    /// (and therefore the `VkContext` below) so the pipeline's `Drop`
    /// `destroy_pipeline` runs against a live device.
    pub(crate) trap_pipeline: Option<crate::kms::vk::trap_pipeline::TrapPipeline>,

    // Drawing-op command pool (sub-phase 4.1.4). Separate from
    // `MirrorUploader`'s transfer pool — drawing ops emit graphics
    // workload (begin_rendering / clear_attachments / draws), the
    // uploader emits transfer workload. Sharing pools risks
    // lifetime tangles when both run in the same frame. `None`
    // when Vulkan didn't come up.
    //
    // Drop order: comes AFTER `scheduler` above so `PaintBatch::Drop`
    // can free its CB against a still-valid pool. See `scheduler`
    // doc for details.
    pub(crate) ops_command_pool: Option<crate::kms::vk::ops::OpsCommandPool>,

    /// Set by `flush_if_needed` when `PaintBatch::submit_and_wait`
    /// returns a Vk error. Once true, every paint entry point
    /// (`record_paint_op{,_batch_op}`, `flush_if_needed`,
    /// `composite_and_flip`, `try_vulkan_composite_flip`) is a
    /// no-op or early-Err. The renderer is unrecoverable in-process;
    /// an external supervisor restarts yserver to recover.
    ///
    /// See `docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase3.md`
    /// "Renderer-disabled design" section.
    pub(crate) renderer_failed: bool,

    /// Set by `disable_output` so the rest of teardown can run
    /// without `composite_and_flip` racing in and resubmitting a
    /// new frame. Latched once; never cleared.
    ///
    /// Distinct from `renderer_failed` (which models in-process
    /// Vk failure that an external supervisor could in principle
    /// restart around): `shutting_down` is terminal-by-design,
    /// triggered when `lib.rs` is unwinding the backend.
    pub(crate) shutting_down: bool,

    /// Aggregated counts of composite-defer events (descriptor pool
    /// ring exhausted, no Free scanout BO). Flushed as a single
    /// `info!` line every `CompositeDeferStats::FLUSH_INTERVAL` from
    /// `composite_and_flip` so steady-state backpressure doesn't
    /// drown the log in per-frame warns. Per-occurrence is still
    /// logged at `debug!`.
    composite_defer_stats: CompositeDeferStats,

    // Per-backend host-mapped staging buffer used by image-transfer
    // ops (PutImage / GetImage / MIT-SHM PutImage / MIT-SHM GetImage /
    // MitShmCreatePixmap). Reused across ops, grows on demand. `None`
    // when Vulkan didn't come up.
    pub(crate) ops_staging: Option<crate::kms::vk::ops::OpsStaging>,

    // Glyph atlas + text-render pipeline (sub-phase 4.1.4.5). The
    // atlas owns the shared R8 image; the pipeline binds it via a
    // single combined-image-sampler descriptor. Both `None` when
    // Vulkan didn't come up. Initialised eagerly alongside
    // `compositor_pipeline`.
    pub(crate) glyph_atlas: Option<crate::kms::vk::glyph::GlyphAtlas>,
    pub(crate) text_pipeline: Option<crate::kms::vk::text_pipeline::TextPipeline>,

    // RENDER `Composite` + `FillRectangles` pipeline cache + solid
    // colour scratch images (sub-phase 4.1.4.6). All `None` when
    // Vulkan didn't come up.
    //
    // `solid_src_image` / `solid_mask_image`: 1×1 BGRA scratches
    // rewritten via `cmd_clear_color_image` per call when the
    // corresponding picture is a `SolidFill`. `white_mask_image`:
    // 1×1 BGRA pre-cleared to opaque white at backend init, used
    // as the mask binding for `Composite` calls without a mask
    // (the multiplication by `mask.a == 1.0` is a no-op).
    pub(crate) render_pipelines: Option<crate::kms::vk::render_pipeline::RenderPipelineCache>,
    /// Single-pass arbitrary-kernel convolution pipeline for X RENDER
    /// `SetPictureFilter convolution` (xfwm4 / marco / mutter shadow
    /// blur). Built once at backend init; one pipeline keyed on
    /// `(Over, B8G8R8A8_UNORM)` for phase 2. `None` when Vulkan
    /// didn't come up — `try_vk_render_composite` falls back to the
    /// standard render pipeline (= ignores the convolution kernel,
    /// matching pre-phase-2 behaviour).
    pub(crate) convolution_pipeline:
        Option<crate::kms::vk::convolution_pipeline::ConvolutionPipeline>,
    pub(crate) solid_src_image: Option<crate::kms::vk::render_pipeline::SolidColorImage>,
    pub(crate) solid_mask_image: Option<crate::kms::vk::render_pipeline::SolidColorImage>,
    pub(crate) white_mask_image: Option<crate::kms::vk::render_pipeline::SolidColorImage>,
    /// CPU-rasterised mask scratch for ops without a per-X-resource
    /// mask source (Trapezoids, Triangles). One R8 image, grow-on-
    /// demand. Phase 4.1.4.7.
    pub(crate) mask_scratch: Option<crate::kms::vk::mask_scratch::MaskScratch>,
    /// BGRA staging image for same-target overlap copies (xterm
    /// scrollback) and similar src/dst aliasing. Grow-on-demand.
    pub(crate) copy_scratch: Option<crate::kms::vk::copy_scratch::CopyScratch>,
    /// Sampleable dst-pixel readback scratch for Disjoint/Conjoint
    /// RENDER ops (per-format, grow-on-demand). The shader's manual
    /// blend (`render.frag.glsl` `MODE=1`) needs to read the existing
    /// dst pixel; this scratch is the copy target.
    pub(crate) dst_readback: Option<crate::kms::vk::dst_readback::DstReadback>,
    /// Per-`GcFunction` solid-fill pipelines (Xor / And / Or /
    /// Invert / Set / etc.; 16 X11 GcFunctions → 16 `VkLogicOp`s).
    /// Built lazily on first use of each function.
    pub(crate) logic_fill_pipelines:
        Option<crate::kms::vk::logic_fill_pipeline::LogicFillPipelineCache>,

    // Diagnostic: per-output flag set the first time a
    // pageflip-complete event arrives. Lets us see in the log
    // whether the kernel ever told us the very first flip latched.
    pub(crate) first_pageflip_logged: Vec<bool>,

    // Fonts (freetype)
    font_loader: FontLoader,
    fonts: HashMap<u32, FontState>,

    // Pixman pixmaps (non-window drawables)
    pixmaps: HashMap<u32, PixmapState>,

    // Background state (root)
    bg_pixel: Option<u32>,
    bg_pixmap: Option<PixmapHandle>,

    // Software cursor
    cursor_x: f32,
    cursor_y: f32,
    cursors: HashMap<u32, CursorState>,
    active_cursor: Option<u32>,

    // DRM hardware cursor plane (Phase 4.2 perf). When `Some` the
    // composite pass skips the Vulkan cursor quad and the kernel
    // positions a 64×64 overlay independently of compositor cadence
    // via `drmModeMoveCursor` — microsecond-class ioctl, no GPU
    // touch. Falls back to the Vulkan quad on cursors larger than
    // `HW_CURSOR_W × HW_CURSOR_H`, or when the plane couldn't be
    // initialised (test backend, KMS lacks cursor support).
    pub(crate) cursor_plane: Option<crate::kms::cursor_plane::CursorPlane>,
    /// Cursor XID currently uploaded into `cursor_plane`; `None`
    /// when nothing has been installed yet.
    pub(crate) hw_cursor_xid: Option<u32>,
    /// Hotspot of the cursor currently uploaded into the plane.
    /// Used to translate global cursor coords into per-CRTC
    /// `move_cursor` offsets.
    pub(crate) hw_cursor_hotspot: (u16, u16),

    // X11 KeyButMask bits for currently-held mouse buttons (Button1Mask
    // = 0x100 .. Button5Mask = 0x1000). OR'd into the `state` field of
    // MotionNotify (and ButtonRelease) events so WMs can detect drag
    // gestures. Without this, motion-during-press looks like idle
    // motion to fvwm and Move-or-Raise never starts a Move.
    button_mask: u16,

    // Top-level host_xid the cursor was last over. Drives synthetic
    // EnterNotify / LeaveNotify generation: when this changes between
    // motion events we emit Leave on the old window and Enter on the new.
    // ButtonPress / ButtonRelease additionally fire Enter(NotifyGrab) /
    // Leave(NotifyUngrab) for implicit-pointer-grab semantics — toolkits
    // (e16's button widgets in particular) won't fire click actions
    // without these crossing transitions around the press.
    prev_pointer_window: Option<u32>,

    // Pointer events generated by `process_one_input_event` are buffered
    // here instead of being dispatched directly through `event_sink`.
    // The input thread drains this buffer AFTER releasing the backend
    // mutex (see `drain_pending_pointer_events`) and only then forwards
    // events to the sink. This is mandatory: the sink calls
    // `pointer_event_fanout` which acquires `server.lock()`, while
    // request handlers regularly hold `server.lock()` and reach for the
    // backend mutex — a server→backend ordering on one side and a
    // backend→server ordering on the other deadlocks under load. Phase
    // 6.7's Enter(NotifyGrab) / Leave(NotifyUngrab) crossings tripled
    // the per-motion fanout count, exposing the latent race as a
    // routinely-reproducible freeze under e16.
    pending_pointer_events: Vec<HostPointerEvent>,

    // Current font for text rendering
    current_font: Option<u32>,

    // Current GC draw state (default GC values).
    current_function: GcFunction,
    current_foreground: u32,
    current_background: u32,
    current_fill: FillState,
    current_clip: ClipState,

    // RENDER picture tracking
    pictures: HashMap<u32, PictureState>,

    // Vk mirrors rescued from freed pixmaps still referenced by live
    // pictures. Keyed by picture host_xid. Cleaned up by
    // render_free_picture (drops the DrawableImage, which releases its
    // VkImage and allocation).
    picture_rescued_images: HashMap<u32, crate::kms::vk::target::DrawableImage>,

    // RENDER glyphset tracking
    glyphsets: HashMap<u32, GlyphSetState>,

    // SHAPE extension: per-window shape regions keyed by host XID.
    // None entry = no shape (full rectangle). Some(vec![]) = empty region.
    shape_bounding: HashMap<u32, Vec<xfixes::RegionRect>>, // kind=0
    shape_clip: HashMap<u32, Vec<xfixes::RegionRect>>,     // kind=1
    shape_input: HashMap<u32, Vec<xfixes::RegionRect>>,    // kind=2

    /// Refcounted lifecycle for `Composite::NameWindowPixmap`
    /// backings (L2 plan B.3). Entries get added by `RedirectWindow`
    /// activation (B.6a, refcount=1) and bumped/decremented by
    /// each alias / Unredirect / DestroyWindow / FreePixmap.
    pub(crate) alias_registry: AliasRegistry,
    /// `host_window → backing_pixmap_handle` map populated by
    /// `allocate_redirected_backing`; `name_window_pixmap` looks
    /// up the matching backing here. Cleared by B.6c when a
    /// redirect is torn down.
    pub(crate) host_window_to_backing: HashMap<u32, PixmapHandle>,
}

/// State for a RENDER picture on the KMS backend.
enum PictureState {
    /// Picture wraps a window or pixmap drawable. Composites are forwarded
    /// to that drawable's Pixman image.
    Drawable {
        /// XID of the backing window or pixmap in self.windows / self.pixmaps.
        host_xid: u32,
        /// Optional clip rectangles set via SetPictureClipRectangles.
        clip: Option<Vec<Rectangle16>>,
        repeat: Repeat,
        alpha_map: Option<u32>,
        alpha_x: i16,
        alpha_y: i16,
        clip_x: i16,
        clip_y: i16,
        component_alpha: bool,
        transform: Option<PictTransform>,
        graphics_exposure: bool,
        subwindow_mode: u8,
        poly_edge: u8,
        poly_mode: u8,
        /// Sampling filter set via `SetPictureFilter`. Defaults to
        /// Nearest per X RENDER spec; xfwm4's compositor sets
        /// `convolution` with a Gaussian kernel to blur window shadow
        /// pixmaps before compositing them onto the screen.
        filter: PictureFilter,
    },
    /// CreateSolidFill source: a single premultiplied BGRA colour
    /// the Vk render pipeline reads from a uniform / 1×1 sampler
    /// scratch as needed. Stored already-premultiplied so the
    /// recorder doesn't redo the conversion on every draw.
    SolidFill {
        premul: [f32; 4],
        repeat: Repeat,
        component_alpha: bool,
    },
    Gradient {
        gradient: crate::kms::vk::gradient::GradientPicture,
        repeat: Repeat,
        transform: Option<PictTransform>,
    },
}

/// X RENDER `SetPictureFilter` selection. Per the X RENDER protocol
/// spec, filters are identified by ASCII name string. We honor the
/// ones that affect actual rendering paths today; unknown names fall
/// back to `Bilinear` so the picture still renders something sane.
#[derive(Debug, Clone)]
enum PictureFilter {
    /// X RENDER `nearest`: point sampling. Per spec this is RENDER's
    /// default. Most modern clients explicitly request `bilinear`.
    Nearest,
    /// X RENDER `bilinear` / `good` / `fast` / `best` — single-sample
    /// linear filtering between the four neighbour texels. The Vk
    /// composite shader's hard-coded LINEAR sampler matches.
    Bilinear,
    /// X RENDER `convolution` filter: an N×M kernel of FIXED weights
    /// applied per output pixel. xfwm4's compositor uses this to
    /// apply a Gaussian blur to window-shadow pixmaps before
    /// compositing. `weights` is row-major (height rows × width
    /// columns); the Vk render pipeline runs a separable / direct
    /// convolution pre-pass to produce a blurred intermediate that
    /// the subsequent Composite reads as the source picture.
    Convolution {
        // Field reads land in the upcoming convolution-render pipeline;
        // suppress the dead-code warning for the
        // parse-and-store-only commit.
        #[allow(dead_code)]
        width: u16,
        #[allow(dead_code)]
        height: u16,
        #[allow(dead_code)]
        weights: Vec<f32>,
    },
}

impl PictureFilter {
    fn parse(body: &[u8]) -> Option<Self> {
        // SetPictureFilter request body (after the 4-byte picture xid
        // already consumed by the caller):
        //   name_length: u16
        //   pad: 2 bytes
        //   name: STRING8, padded to 4-byte boundary
        //   values: LISTofFIXED (i32 each, 16.16 fixed-point)
        if body.len() < 4 {
            return None;
        }
        let name_length = u16::from_le_bytes([body[0], body[1]]) as usize;
        let name_start = 4;
        if body.len() < name_start + name_length {
            return None;
        }
        let name = &body[name_start..name_start + name_length];
        let values_start = name_start + ((name_length + 3) & !3);
        let values = &body[values_start.min(body.len())..];

        // Aliases mirror the QueryFilters reply
        // (`write_render_query_filters_reply` in yserver-protocol).
        // X RENDER spec: `fast` aliases `nearest`; `good` and `best`
        // alias `bilinear`. Keeping this in sync with the advertised
        // alias table matters — clients that consult QueryFilters
        // first and then issue SetPictureFilter with the canonical
        // name (e.g. `nearest` instead of `fast`) get the same
        // semantics either way.
        match name {
            b"nearest" | b"fast" => Some(PictureFilter::Nearest),
            b"bilinear" | b"good" | b"best" => Some(PictureFilter::Bilinear),
            b"convolution" => parse_convolution(values),
            _ => None,
        }
    }
}

fn parse_convolution(values: &[u8]) -> Option<PictureFilter> {
    // First two FIXED values are the kernel dimensions (integer parts),
    // followed by width*height FIXED kernel weights.
    if values.len() < 8 {
        return None;
    }
    let read_fixed = |bytes: &[u8]| -> f32 {
        // i32 in two's complement, FIXED 16.16 → float.
        let raw = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        (raw as f32) / 65536.0
    };
    let width = read_fixed(&values[0..4]) as u16;
    let height = read_fixed(&values[4..8]) as u16;
    let n = width as usize * height as usize;
    // Cap kernel size defensively — a Gaussian blur shadow needs ≤21×21.
    if n == 0 || n > 64 * 64 {
        return None;
    }
    let weights_bytes_needed = n * 4;
    if values.len() < 8 + weights_bytes_needed {
        return None;
    }
    let mut weights = Vec::with_capacity(n);
    for i in 0..n {
        let off = 8 + i * 4;
        weights.push(read_fixed(&values[off..off + 4]));
    }
    Some(PictureFilter::Convolution {
        width,
        height,
        weights,
    })
}

fn default_drawable_picture(host_xid: u32) -> PictureState {
    PictureState::Drawable {
        host_xid,
        clip: None,
        repeat: Repeat::None,
        alpha_map: None,
        alpha_x: 0,
        alpha_y: 0,
        clip_x: 0,
        clip_y: 0,
        component_alpha: false,
        transform: None,
        graphics_exposure: false,
        subwindow_mode: 0,
        poly_edge: 0,
        poly_mode: 0,
        filter: PictureFilter::Nearest,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlyphSetFormat {
    A8,
    A1,
    /// ARGB32 source glyphs (Cairo's default for modern GTK3 themes
    /// using subpixel / colour-emoji rendering). On `AddGlyphs` we
    /// extract the alpha channel into a densely-packed A8 buffer and
    /// store the glyph as if it had been uploaded in A8 format — the
    /// downstream atlas + text pipeline path is identical from there
    /// on. Subpixel coverage detail and emoji colour are lost; glyph
    /// shape is preserved, which is enough for grayscale text.
    Argb32,
    Other,
}

struct StoredGlyph {
    width: u16,
    height: u16,
    /// RENDER wire field: top-left of bitmap relative to glyph origin.
    /// This is the *negative* of FreeType's bitmap_left.
    /// Draw at pen_x - x, pen_y - y.
    x: i16,
    y: i16,
    x_off: i16,
    /// Vertical pen advance. Parsed from wire for fidelity but unused —
    /// horizontal-text rendering only advances the x pen between glyphs.
    #[allow(dead_code)]
    y_off: i16,
    /// Row-major A8 bytes, densely packed (no per-row padding).
    pixels: Vec<u8>,
    format: GlyphSetFormat,
}

pub(super) struct GlyphSetState {
    format: GlyphSetFormat,
    glyphs: HashMap<u32, StoredGlyph>,
}

struct CursorState {
    /// Cursor extent in pixels. Mirrors `vk_mirror`'s extent when the
    /// mirror is present; stored separately so the composite scene
    /// builder can size the cursor quad without a Vk borrow.
    extent: ash::vk::Extent2D,
    hot_x: u16,
    hot_y: u16,
    /// Vulkan-side cursor image. Same Option semantics as
    /// [`WindowState::vk_mirror`] / [`PixmapState::vk_mirror`].
    /// Built and populated at cursor-create time via
    /// [`KmsBackend::upload_bgra_to_mirror`]; sampled by the
    /// composite pass's final cursor quad (Phase 4.1.3.4).
    vk_mirror: Option<crate::kms::vk::target::DrawableImage>,
}

struct WindowState {
    _nested_id: ResourceId,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border_width: u16,
    mapped: bool,
    _override_redirect: bool,
    _parent: Option<u32>,
    children: Vec<u32>,
    bg_pixel: Option<u32>,
    bg_pixmap: Option<PixmapHandle>,
    #[allow(dead_code)]
    depth: u8,
    #[allow(dead_code)]
    visual: u32,
    /// Cursor XID set on this window via DefineCursor. `0` means
    /// "inherit from parent" (X11 `None`). The effective cursor for
    /// rendering walks up the parent chain until it finds a non-zero
    /// XID, falling back to whatever cursor was last installed for the
    /// root container.
    cursor: u32,
    /// Vulkan-side image for this window. `None` when Vulkan isn't
    /// up or mirror allocation failed; in that case the window
    /// won't render content under the composite pass. Created at
    /// `create_subwindow` time and bg-pixel-filled if applicable;
    /// reallocated on resize.
    vk_mirror: Option<crate::kms::vk::target::DrawableImage>,
}

struct FontState {
    #[allow(dead_code)]
    handle: u32,
    face: RefCell<FreetypeFace>,
    metrics: FontMetrics,
    char_info_cache: HashMap<char, ProtocolCharInfo>,
}

struct PixmapState {
    #[allow(dead_code)]
    handle: u32,
    width: u16,
    height: u16,
    #[allow(dead_code)]
    depth: u8,
    /// Vulkan-side image for this pixmap. Same Option semantics as
    /// [`WindowState::vk_mirror`]. Created at `create_pixmap` time;
    /// rescued on `free_pixmap` when a live picture still references
    /// it (see [`KmsBackend::free_pixmap`]).
    vk_mirror: Option<crate::kms::vk::target::DrawableImage>,
}

/// Resolves X11 font names (aliases like `fixed`, XLFDs like
/// `-adobe-helvetica-bold-r-*-*-12-*-...`, or family names) to a filesystem
/// path via fontconfig, then opens the file with FreeType.
///
/// `catalog` is the list of XLFDs we advertise via ListFonts /
/// ListFontsWithInfo. It is built once at init time by enumerating
/// fontconfig's installed-font set and synthesising one XLFD per
/// (face × pixel-size × charset) combination. Every entry resolves
/// back through `open_font`, so the LFWI metrics path can return real
/// FreeType metrics for any name we hand out.
struct FontLoader {
    library: freetype::Library,
    fc: fontconfig::Fontconfig,
    catalog: Vec<String>,
}

impl FontLoader {
    fn new() -> io::Result<Self> {
        let fc = fontconfig::Fontconfig::new()
            .ok_or_else(|| io::Error::other("fontconfig init failed"))?;
        let catalog = build_font_catalog(&fc);
        log::info!("font catalog: {} XLFDs from fontconfig", catalog.len());
        Ok(Self {
            library: freetype::Library::init()
                .map_err(|e| io::Error::other(format!("freetype init failed: {e:?}")))?,
            fc,
            catalog,
        })
    }

    fn is_xlfd_pattern(name: &str) -> bool {
        name.starts_with('-')
    }

    /// Pull (family, style, pixel_size) hints out of an XLFD pattern.
    /// XLFD field indices after splitting on '-' (leading '-' produces an
    /// empty 0th element):
    ///   1=foundry 2=family 3=weight 4=slant 5=setwidth 6=addstyle
    ///   7=pixelsize 8=pointsize 9=resx 10=resy 11=spacing 12=avgwidth
    /// "*" or empty fields are treated as wildcards.
    fn parse_xlfd(name: &str) -> (Option<String>, Option<String>, Option<u32>) {
        let parts: Vec<&str> = name.split('-').collect();
        let take = |i: usize| -> Option<String> {
            parts
                .get(i)
                .filter(|s| !s.is_empty() && **s != "*")
                .map(|s| (*s).to_string())
        };
        let family = take(2);
        let weight = take(3);
        let slant = take(4);
        let style = match (weight.as_deref(), slant.as_deref()) {
            (None, Some("i" | "o")) => Some("Italic".to_string()),
            (Some(w), Some("i" | "o")) => Some(format!("{w} Italic")),
            (Some(w), _) => Some(w.to_string()),
            (None, _) => None,
        };
        let px = parts
            .get(7)
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|&s| s > 0);
        (family, style, px)
    }

    fn open_font(
        &self,
        name: &str,
    ) -> io::Result<(freetype::Face, FontMetrics, HashMap<char, ProtocolCharInfo>)> {
        // Resolve the X11 font name to a file path via fontconfig. We can't
        // rely on the high-level `Fontconfig::find`: when the requested family
        // doesn't exist, fontconfig falls back to the *system default* (often
        // a proportional sans-serif), which makes xterm/wmaker render with
        // wrong metrics. Build the pattern ourselves and chain "monospace" as
        // a secondary family — fontconfig prefers the first listed family but
        // falls through the chain before reaching its system default.
        let (family, style, xlfd_px) = if Self::is_xlfd_pattern(name) {
            Self::parse_xlfd(name)
        } else {
            (Some(name.to_string()), None, None)
        };
        let query_family = family.as_deref().unwrap_or("monospace");

        let cfamily = std::ffi::CString::new(query_family)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "font name has nul"))?;

        let mut pat = fontconfig::Pattern::new(&self.fc);
        pat.add_string(fontconfig::FC_FAMILY, &cfamily);
        if query_family != "monospace" {
            pat.add_string(fontconfig::FC_FAMILY, c"monospace");
        }
        let cstyle_storage;
        if let Some(style) = style.as_deref() {
            cstyle_storage = std::ffi::CString::new(style)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "font style has nul"))?;
            pat.add_string(fontconfig::FC_STYLE, &cstyle_storage);
        }
        let matched = pat.font_match();
        let path = matched.filename().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("font not found: {name}"))
        })?;
        let face_index: isize = matched.face_index().unwrap_or(0) as isize;
        let face = self
            .library
            .new_face(path, face_index)
            .map_err(|e| io::Error::other(format!("freetype new_face({path}) failed: {e:?}")))?;

        // Honour XLFD PIXEL_SIZE if specified; otherwise default to 12pt @ 96dpi.
        if let Some(px) = xlfd_px {
            let _ = face.set_pixel_sizes(0, px);
        } else {
            let _ = face.set_char_size(12 << 6, 12 << 6, 96, 96);
        }
        let (metrics, char_cache) = compute_font_metrics(&face);
        Ok((face, metrics, char_cache))
    }
}

fn compute_char_info(face: &freetype::Face, ch: char) -> ProtocolCharInfo {
    let glyph_idx = ch as usize;
    let _ = face.load_char(glyph_idx, freetype::face::LoadFlag::RENDER);
    let glyph = face.glyph();
    let bitmap = glyph.bitmap();
    let metrics = glyph.metrics();

    let width = (metrics.horiAdvance >> 6) as i16;
    let left_side_bearing = (metrics.horiBearingX >> 6) as i16;
    let right_side_bearing = left_side_bearing + bitmap.width() as i16;
    let ascent = (metrics.horiBearingY >> 6) as i16;
    let descent = (bitmap.rows() as i16) - ascent;

    ProtocolCharInfo {
        left_side_bearing,
        right_side_bearing,
        character_width: width,
        ascent,
        descent,
        attributes: 0,
    }
}

fn compute_font_metrics(face: &freetype::Face) -> (FontMetrics, HashMap<char, ProtocolCharInfo>) {
    let mut char_info_cache = HashMap::new();
    // min_bounds tracks the per-glyph minimum across each metric, so each
    // field starts at its type's MAX so the first observation overwrites it.
    let mut min_bounds = ProtocolCharInfo {
        left_side_bearing: i16::MAX,
        right_side_bearing: i16::MAX,
        character_width: i16::MAX,
        ascent: i16::MAX,
        descent: i16::MAX,
        attributes: 0,
    };
    // max_bounds tracks the per-glyph maximum, so each field starts at MIN.
    let mut max_bounds = ProtocolCharInfo {
        left_side_bearing: i16::MIN,
        right_side_bearing: i16::MIN,
        character_width: i16::MIN,
        ascent: i16::MIN,
        descent: i16::MIN,
        attributes: 0,
    };

    for code in 0x20u32..=0x7E {
        let ch = char::from_u32(code).unwrap();
        let ci = compute_char_info(face, ch);
        min_bounds.left_side_bearing = min_bounds.left_side_bearing.min(ci.left_side_bearing);
        max_bounds.left_side_bearing = max_bounds.left_side_bearing.max(ci.left_side_bearing);
        min_bounds.right_side_bearing = min_bounds.right_side_bearing.min(ci.right_side_bearing);
        max_bounds.right_side_bearing = max_bounds.right_side_bearing.max(ci.right_side_bearing);
        min_bounds.character_width = min_bounds.character_width.min(ci.character_width);
        max_bounds.character_width = max_bounds.character_width.max(ci.character_width);
        min_bounds.ascent = min_bounds.ascent.min(ci.ascent);
        max_bounds.ascent = max_bounds.ascent.max(ci.ascent);
        min_bounds.descent = min_bounds.descent.min(ci.descent);
        max_bounds.descent = max_bounds.descent.max(ci.descent);
        char_info_cache.insert(ch, ci);
    }

    let font_ascent = max_bounds.ascent;
    let font_descent = max_bounds.descent;

    let metrics = FontMetrics {
        min_bounds,
        max_bounds,
        min_char_or_byte2: 0x20,
        max_char_or_byte2: 0x7E,
        default_char: 0x20,
        draw_direction: 0, // LeftToRight
        min_byte1: 0,
        max_byte1: 0,
        all_chars_exist: true,
        font_ascent,
        font_descent,
        properties: Vec::new(),
        char_infos: char_info_cache.values().cloned().collect(),
    };
    (metrics, char_info_cache)
}

impl KmsBackend {
    pub fn open(device_path: &str) -> io::Result<Self> {
        Self::open_with_commit(device_path, drm::modeset::commit_modeset)
    }

    /// Construct a headless `KmsBackend` for integration tests.
    ///
    /// Wraps a `drm::Device::for_tests()` stub, leaves `vk = None`, and
    /// seeds a single 800x600 "test" output. Sufficient to drive
    /// `process_request` for protocol-level assertions; paint paths
    /// that require Vulkan return early (the `vk.as_ref()` guards in
    /// the op recorders short-circuit). Phase A composite tests that
    /// need real Vulkan paint will gain a `for_tests_with_vk` sibling.
    ///
    /// Hidden from rustdoc — this is wiring for test fixtures, not a
    /// public API. The first consumer is
    /// `crates/yserver/tests/common/server_fixture.rs`.
    #[doc(hidden)]
    #[must_use]
    pub fn for_tests() -> Self {
        let ctx = XkbContext(xkbcommon::xkb::Context::new(
            xkbcommon::xkb::CONTEXT_NO_FLAGS,
        ));
        let keymap = xkbcommon::xkb::Keymap::new_from_names(
            &ctx.0,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .or_else(|| {
            xkbcommon::xkb::Keymap::new_from_names(
                &ctx.0,
                "",
                "",
                "",
                "",
                None,
                xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        })
        .expect("test xkb keymap");
        let xkb_state = XkbState(xkbcommon::xkb::State::new(&keymap));

        KmsBackend {
            device: Arc::new(crate::drm::Device::for_tests().expect("test drm device")),
            render_node_fd: None,
            render_node_path: None,
            dri3_sync_resources: HashMap::new(),
            dri3_xshmfences: HashMap::new(),
            outputs: vec![OutputLayout {
                output: crate::drm::modeset::Output {
                    connector: ::drm::control::from_u32(1).unwrap(),
                    connector_name: "test".to_string(),
                    crtc: ::drm::control::from_u32(1).unwrap(),
                    plane: ::drm::control::from_u32(1).unwrap(),
                    // SAFETY: tests never pass this mode to DRM; it is only
                    // present to satisfy KmsBackend's production fields.
                    mode: unsafe { std::mem::zeroed() },
                    picked: crate::drm::modeset::Mode {
                        name: "test".to_string(),
                        width: 800,
                        height: 600,
                        vrefresh: 60,
                        preferred: true,
                    },
                    plane_fb_id_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_crtc_id_prop: ::drm::control::from_u32(1).unwrap(),
                },
                swapchain: crate::drm::Swapchain::empty_for_tests(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                damage: crate::kms::scheduler::damage::OutputDamageState::new(),
                composite_pools: None,
            }],
            fb_w: 800,
            fb_h: 600,
            windows: HashMap::new(),
            next_host_xid: 0x0040_0000,
            top_level_order: Vec::new(),
            window_id: 1,
            root_visual_xid: 0x21,
            xid_map: HostXidMap::new(),
            xkb_context: ctx,
            xkb_keymap: XkbKeymap(keymap),
            xkb_state,
            input_ctx: None,
            vk: None,
            first_pageflip_logged: vec![false; 1],
            scheduler: crate::kms::scheduler::RenderScheduler::new(),
            pixmap_pool: None,
            trap_pipeline: None,
            scanout_pools: Vec::new(),
            compositor_pipeline: None,
            ops_command_pool: None,
            renderer_failed: false,
            shutting_down: false,
            composite_defer_stats: CompositeDeferStats::default(),
            ops_staging: None,
            glyph_atlas: None,
            text_pipeline: None,
            render_pipelines: None,
            convolution_pipeline: None,
            solid_src_image: None,
            solid_mask_image: None,
            white_mask_image: None,
            mask_scratch: None,
            copy_scratch: None,
            dst_readback: None,
            logic_fill_pipelines: None,
            font_loader: FontLoader::new().expect("test font loader"),
            fonts: HashMap::new(),
            pixmaps: HashMap::new(),
            bg_pixel: None,
            bg_pixmap: None,
            // Initial cursor position: center of the 800×600 test
            // framebuffer (matches the input_thread's own initial
            // state). See parallel comment in the real constructor
            // for the GTK gesture-drag rationale; tests don't rely
            // on the cursor being at the origin and using midpoint
            // here keeps the for_test backend internally consistent.
            cursor_x: 400.0,
            cursor_y: 300.0,
            cursors: HashMap::new(),
            active_cursor: None,
            cursor_plane: None,
            hw_cursor_xid: None,
            hw_cursor_hotspot: (0, 0),
            button_mask: 0,
            prev_pointer_window: None,
            pending_pointer_events: Vec::new(),
            current_font: None,
            current_function: GcFunction::Copy,
            current_foreground: 0,
            current_background: 0x00ff_ffff,
            current_fill: FillState::Solid,
            current_clip: ClipState::None,
            pictures: HashMap::new(),
            picture_rescued_images: HashMap::new(),
            glyphsets: HashMap::new(),
            shape_bounding: HashMap::new(),
            shape_clip: HashMap::new(),
            shape_input: HashMap::new(),
            alias_registry: AliasRegistry::default(),
            host_window_to_backing: HashMap::new(),
        }
    }

    /// Variant of [`Self::for_tests`] that attaches a real `VkContext`
    /// plus the supporting Vulkan state needed for the L1 alpha-invariant
    /// tests to record into window / pixmap mirrors and read them back:
    /// `ops_command_pool`, `ops_staging`, and `logic_fill_pipelines`.
    /// Scanout-side state (`scanout_pools`, `compositor_pipeline`,
    /// RENDER pipeline cache, etc.) is **not** initialised — those
    /// require a real DRM device and land with the scanout-capture
    /// task (A.16 in the composite implementation plan).
    ///
    /// Returns `Err` if any of the Vulkan-side bring-up steps fail.
    /// Hidden from rustdoc; for test-fixture use only.
    ///
    /// # Errors
    ///
    /// Propagates errors from `VkContext::new`, `OpsCommandPool::new`,
    /// `OpsStaging::new`, and `LogicFillPipelineCache::new`.
    #[doc(hidden)]
    pub fn for_tests_with_vk() -> io::Result<Self> {
        let mut backend = Self::for_tests();
        let vk = crate::kms::vk::device::VkContext::new()
            .map_err(|e| io::Error::other(format!("VkContext::new: {e}")))?;
        let ops_pool = crate::kms::vk::ops::OpsCommandPool::new(Arc::clone(&vk))
            .map_err(|e| io::Error::other(format!("OpsCommandPool::new: {e:?}")))?;
        let ops_staging = crate::kms::vk::ops::OpsStaging::new(Arc::clone(&vk), 1024 * 1024)
            .map_err(|e| io::Error::other(format!("OpsStaging::new: {e:?}")))?;
        let logic_fill = crate::kms::vk::logic_fill_pipeline::LogicFillPipelineCache::new(
            Arc::clone(&vk),
            ash::vk::Format::B8G8R8A8_UNORM,
        )
        .map_err(|e| io::Error::other(format!("LogicFillPipelineCache::new: {e:?}")))?;
        // pixmap-pool T2: backend-owned recycle pool for server-owned
        // pixmap-backing triples. Mirrors the production init in
        // `open_with_commit`; needed here so `free_pixmap` in
        // for_tests_with_vk-driven tests goes through the same
        // defer-release path.
        let pixmap_pool = Arc::new(crate::kms::vk::pixmap_pool::PixmapPool::new(Arc::clone(
            &vk,
        )));
        crate::kms::vk::pixmap_pool::register_for_telemetry(&pixmap_pool);
        // gpu-trap T2: mirror the production init so for_tests_with_vk-
        // driven tests exercise the same `trap_pipeline.is_some()` path.
        // Failure here leaves the field `None`; tests that don't rely on
        // GPU trap rasterize keep working.
        let trap_pipeline = match crate::kms::vk::trap_pipeline::TrapPipeline::new(
            Arc::clone(&vk),
            ash::vk::Format::R8_UNORM,
        ) {
            Ok(p) => Some(p),
            Err(e) => {
                log::warn!(
                    "vk trap pipeline init failed (for_tests_with_vk): {e:?} — \
                     traps will fall back to pixman"
                );
                None
            }
        };
        backend.vk = Some(vk);
        backend.pixmap_pool = Some(pixmap_pool);
        backend.trap_pipeline = trap_pipeline;
        backend.ops_command_pool = Some(ops_pool);
        backend.ops_staging = Some(ops_staging);
        backend.logic_fill_pipelines = Some(logic_fill);
        Ok(backend)
    }

    /// Snapshot of the backend's [`PixmapPool`] stats. Returns `None`
    /// if the pool was never initialised (no Vulkan context attached —
    /// e.g. the headless `for_tests` path).
    ///
    /// Test / introspection only. Stable enough to expose as `pub`
    /// (could ship in a debug HUD); not part of any documented public
    /// API. Hidden from rustdoc.
    ///
    /// [`PixmapPool`]: crate::kms::vk::pixmap_pool::PixmapPool
    #[doc(hidden)]
    #[must_use]
    pub fn pixmap_pool_stats(&self) -> Option<crate::kms::vk::pixmap_pool::PixmapPoolStats> {
        self.pixmap_pool.as_ref().map(|p| p.stats())
    }

    /// Close the currently-open paint batch (if any) AND drain every
    /// in-flight submitted batch, blocking on each fence. Forces the
    /// `PooledPixmapReturn` `BatchResource`s adopted by `free_pixmap`
    /// to release their entries back into the pool synchronously.
    ///
    /// Test-only — production code retires batches lazily via
    /// `poll_retired_paint_batches` and composite ticks; the
    /// `_for_test` suffix is the contract. Do not call from
    /// production paths.
    ///
    /// A bare "drain submitted batches" is **not** sufficient because
    /// `free_pixmap` adopts the mirror's BatchResource into the
    /// *currently-open* batch, which will not appear in
    /// `submitted_paint_batches` until something closes it. This
    /// helper closes first, then drains.
    ///
    /// Hidden from rustdoc.
    ///
    /// # Errors
    ///
    /// Propagates any [`BatchError`] returned by
    /// `close_and_submit_async` or `drain_submitted_paint_batches`.
    ///
    /// [`BatchError`]: crate::kms::scheduler::paint_batch::BatchError
    #[doc(hidden)]
    pub fn force_retire_in_flight_for_test(
        &mut self,
    ) -> Result<(), crate::kms::scheduler::paint_batch::BatchError> {
        // Close + submit-async whatever is open (no-op for an Idle/
        // missing batch, returning a null fence). The submit_async
        // path queues a Submitted batch on submitted_paint_batches.
        self.scheduler.close_and_submit_async(Vec::new())?;
        // Drain every submitted batch, waiting on each fence in FIFO
        // order. After this returns, every BatchResource adopted by
        // earlier free_pixmap calls has released — pool stats reflect
        // the final return-accept / return-reject tallies.
        self.scheduler.drain_submitted_paint_batches()?;
        Ok(())
    }

    /// Public wrapper over [`Self::read_mirror_pixels`] for test
    /// fixtures. Returns `(width, height, bgra_bytes)` where `bytes`
    /// is the tightly-packed `B8G8R8A8_UNORM` pixel buffer of the
    /// mirror behind `host_xid` (a window or pixmap host XID).
    /// `None` if the drawable is unknown, has no Vulkan mirror, or
    /// the readback failed.
    ///
    /// Hidden from rustdoc; intended for the test fixture in
    /// `crates/yserver/tests/common/server_fixture.rs`.
    #[doc(hidden)]
    pub fn capture_mirror_bgra8(&mut self, host_xid: u32) -> Option<(u32, u32, Vec<u8>)> {
        let rb = self.read_mirror_pixels(host_xid)?;
        // The fixture's image accessor presumes 4 bytes per pixel
        // (BGRA8). Mirrors of depth 1/8 use `R8_UNORM` (bpp = 1) —
        // refuse to coerce those silently.
        if rb.bytes_per_pixel != 4 {
            return None;
        }
        Some((rb.width, rb.height, rb.bytes))
    }

    /// Flush the current paint batch for `reason`.
    ///
    /// Error semantics depend on `reason`:
    ///
    /// - `VisibleComposite` / `SizeLimit` / `LatencyLimit` /
    ///   `Shutdown`: best-effort. A Poisoned batch (recorder
    ///   failure earlier this cycle) is acceptable — composite
    ///   will sample whatever mirrors currently hold; the
    ///   recorder's affected drawables are already marked
    ///   dirty for the next cycle.
    /// - `Readback` / `ExternalSync` / `ProtocolBarrier`: the
    ///   caller's contract requires the batch's work to have
    ///   COMPLETED before this returns. A Poisoned or
    ///   InvalidState batch means we cannot promise that; surface
    ///   the failure so the caller can fail the request (return
    ///   `BadAlloc`-shaped X error, return zeros from GetImage,
    ///   etc.).
    ///
    /// **Any `Err(vk::Result)` returned here is fatal**: it comes
    /// from `submit_and_wait`'s path 2 (wait failure ⇒ abandoned
    /// CB/resources). Callers MUST propagate up to the main loop
    /// and enter backend teardown / disabled-renderer state;
    /// continuing to schedule paint work after this is not a
    /// supported steady state.
    pub fn flush_if_needed(
        &mut self,
        reason: crate::kms::scheduler::paint_batch::BatchFlushReason,
    ) -> Result<(), ash::vk::Result> {
        use crate::kms::scheduler::paint_batch::{BatchError, BatchFlushReason};
        if self.renderer_failed {
            // Already failed: best-effort reasons swallow; strict
            // reasons surface ERROR_DEVICE_LOST so the caller's
            // synchronous-reply contract isn't silently broken.
            return match reason {
                BatchFlushReason::Readback
                | BatchFlushReason::ExternalSync
                | BatchFlushReason::ProtocolBarrier => Err(ash::vk::Result::ERROR_DEVICE_LOST),
                _ => Ok(()),
            };
        }
        log::trace!("flush_if_needed: reason={reason:?}");
        // Per-source submit attribution: tag this flush by reason.
        // The submit counter under that name increments iff
        // close_and_submit{,_async} actually issues a queue_submit2
        // (the Idle / Poisoned short-circuits do not submit, so this
        // pre-attribution slightly over-counts; if that becomes a
        // problem move the increment inside the submit path).
        match reason {
            BatchFlushReason::VisibleComposite => {
                crate::kms::vk::call_stats::VK_CALLS
                    .submit_visible_composite
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            BatchFlushReason::Readback => {
                crate::kms::vk::call_stats::VK_CALLS
                    .submit_readback
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            BatchFlushReason::ExternalSync => {
                crate::kms::vk::call_stats::VK_CALLS
                    .submit_external_sync
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            BatchFlushReason::ProtocolBarrier => {
                crate::kms::vk::call_stats::VK_CALLS
                    .submit_protocol_barrier
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            BatchFlushReason::SizeLimit => {
                crate::kms::vk::call_stats::VK_CALLS
                    .submit_size_limit
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            BatchFlushReason::LatencyLimit => {
                crate::kms::vk::call_stats::VK_CALLS
                    .submit_latency_limit
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            BatchFlushReason::Shutdown => {
                crate::kms::vk::call_stats::VK_CALLS
                    .submit_shutdown
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let dirty_outputs: Vec<usize> = (0..self.outputs.len())
            .filter(|&i| self.outputs[i].damage.needs_composite())
            .collect();
        let strict = matches!(
            reason,
            BatchFlushReason::Readback
                | BatchFlushReason::ExternalSync
                | BatchFlushReason::ProtocolBarrier
        );

        // 4-T3: strict reasons use the blocking submit (close_and_submit
        // → submit_and_wait → wait_for_fences on the batch's own fence).
        // Best-effort reasons use the async path: close_and_submit_async
        // moves the batch to the submitted-paint-batches queue; the
        // composite-tick poll retires it later when its fence signals.
        let result = if strict {
            self.scheduler.close_and_submit(dirty_outputs)
        } else {
            self.scheduler
                .close_and_submit_async(dirty_outputs)
                .map(|_fence| ())
        };
        match result {
            Ok(()) => Ok(()),
            Err(BatchError::Vk(r)) => {
                log::error!(
                    "flush_if_needed({reason:?}): submit_and_wait returned fatal {r:?}; \
                     latching renderer_failed — KMS renderer disabled until restart"
                );
                self.renderer_failed = true;
                Err(r)
            }
            Err(BatchError::Poisoned) if strict => {
                log::warn!(
                    "flush_if_needed({reason:?}): batch was Poisoned; \
                     caller's completion guarantee cannot be honoured"
                );
                Err(ash::vk::Result::ERROR_DEVICE_LOST)
            }
            Err(BatchError::InvalidState(s)) if strict => {
                log::error!(
                    "flush_if_needed({reason:?}): batch in invalid state {s:?}; \
                     caller's completion guarantee cannot be honoured"
                );
                Err(ash::vk::Result::ERROR_UNKNOWN)
            }
            // Best-effort reasons swallow Poisoned / InvalidState.
            Err(_) => Ok(()),
        }
    }

    /// Returns the resources needed to call
    /// `self.scheduler.record_paint_op` / `record_paint_batch_op`,
    /// or `None` if the renderer is failed / Vk is unavailable /
    /// the ops pool is not yet built.
    ///
    /// **Use this at every paint call site that needs to take a
    /// `&mut self.windows[id]` / `&mut self.pixmaps[id]` borrow
    /// for the recorder's `&mut DrawableImage` argument.** Going
    /// through `self.record_paint_op(...)` (the shim below) is
    /// convenient when no such borrow conflict exists, but the shim
    /// is `&mut self` and conflicts with field borrows.
    ///
    /// Both shim and helper gate on `renderer_failed` — every
    /// paint entry point checks the flag.
    fn paint_resources(
        &self,
    ) -> Option<(
        std::sync::Arc<crate::kms::vk::device::VkContext>,
        ash::vk::CommandPool,
    )> {
        if self.renderer_failed {
            return None;
        }
        let vk_arc = self.vk.as_ref().cloned()?;
        let pool_handle = self.ops_command_pool.as_ref()?.handle();
        Some((vk_arc, pool_handle))
    }

    /// Shim: pull vk + ops pool via `paint_resources()`, delegate
    /// to the scheduler-level `record_paint_batch_op`. Useful when
    /// the caller doesn't hold a conflicting `&mut self.windows`
    /// / `.pixmaps` borrow. Recorders that DO hold such a borrow
    /// must use `paint_resources()` + `self.scheduler.record_paint_batch_op(...)`
    /// directly (field projection works because `&mut self.scheduler`
    /// is disjoint from `&mut self.windows`).
    pub fn record_paint_batch_op<F>(&mut self, record: F) -> Result<(), ash::vk::Result>
    where
        F: FnOnce(
            &crate::kms::vk::device::VkContext,
            &mut crate::kms::scheduler::paint_batch::PaintBatch,
            ash::vk::CommandBuffer,
        ) -> Result<(), ash::vk::Result>,
    {
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            // Either renderer_failed or vk/ops_pool unavailable. The
            // caller's existing fallback (typically log + return
            // false to fall back to pixman) handles either case.
            return Err(ash::vk::Result::ERROR_DEVICE_LOST);
        };
        self.scheduler
            .record_paint_batch_op(vk_arc, pool_handle, record)
    }

    /// Shim for recorders that don't need the batch handle.
    pub fn record_paint_op<F>(&mut self, record: F) -> Result<(), ash::vk::Result>
    where
        F: FnOnce(
            &crate::kms::vk::device::VkContext,
            ash::vk::CommandBuffer,
        ) -> Result<(), ash::vk::Result>,
    {
        self.record_paint_batch_op(|vk, _batch, cb| record(vk, cb))
    }

    /// Run a paint op via `run_one_shot_op` after first flushing any
    /// pending `PaintBatch`. Use this at every paint-side
    /// `run_one_shot_op` call site that is NOT yet migrated to
    /// `record_paint_op` — phase-3B–3D migrations replace these
    /// wrappers one family at a time.
    ///
    /// **Why the flush:** a migrated recorder (e.g., fill) records
    /// commands into the batch and mutates CPU-side
    /// `DrawableImage::current_layout` immediately, while the
    /// batch CB hasn't been submitted. A later legacy op reading
    /// `current_layout` would emit barriers from a layout the GPU
    /// hasn't reached. The flush forces the batch to submit + wait
    /// idle before the legacy op runs.
    ///
    /// Readback handlers (`GetImage`, `read_mirror_pixels`,
    /// `hw_cursor_refresh`) DO NOT use this wrapper — they keep
    /// their existing `flush_if_needed(Readback)` + direct
    /// `run_one_shot_op` for semantic clarity that they read
    /// CPU-visible pixels. Behaviour-wise Readback and
    /// ProtocolBarrier are both strict (both surface Vk errors
    /// via `ERROR_DEVICE_LOST`); only the audit signal differs.
    ///
    /// Phase-3B T0 catalogue of paint-side run_one_shot_op sites
    /// (every site as of 2026-05-13):
    ///
    ///   upload_bgra_to_mirror:            mirror.record_upload_rect          — migrated 3C T2 (record_paint_batch_op + arena)
    ///   fill_mirror_solid:                fill::record_fill_rectangles        — migrated T2 (record_paint_op)
    ///   copy_drawable_to_new_cursor_mirror: vk_copy::record_copy_area_distinct — migrated T3 (record_paint_op)
    ///   copy_pixmap_mirror_to_cursor:     vk_copy::record_copy_area_distinct  — migrated T3 (record_paint_op)
    ///   try_vk_copy_area (same-overlap):  copy::record_copy_area_same_overlap — migrated 3D (record_paint_batch_op, shared CopyScratch)
    ///   try_vk_copy_area (same):          copy::record_copy_area_same         — migrated T3 (record_paint_op)
    ///   try_vk_copy_area (distinct):      copy::record_copy_area_distinct     — migrated T3 (record_paint_op)
    ///   try_vk_fill_with_function:        fill::record_logic_fill             — migrated T2 (record_paint_op)
    ///   try_vk_solid_fill:                fill::record_fill_rectangles        — migrated T2 (record_paint_op)
    ///   try_vk_put_image:                 image::record_put_image             — migrated 3C T1 (record_paint_batch_op + arena)
    ///   try_vk_text_run:                  text::record_text_run               — migrated 3E T1 (record_paint_op)
    ///   try_vk_render_traps_or_tris:      render::record_render_composite     — migrated 3F-2 (record_paint_batch_op + arena upload + arena descriptors)
    ///   try_vk_render_composite_glyphs:   text::record_text_run               — migrated 3E T2 (record_paint_op)
    ///   try_vk_render_composite:          render::record_render_composite     — migrated 3F-1 (record_paint_batch_op + BatchDescriptorArena)
    ///
    ///   open_with_commit (constructor):   record_solid_color_clear            — left alone (one-time init, no DrawableImage)
    ///   hw_cursor_refresh:                image::record_get_image             — left on Readback flush (readback handler)
    ///   read_mirror_pixels:               image::record_get_image             — left on Readback flush (readback handler)
    ///   try_vk_get_image_pixels:          image::record_get_image             — left on Readback flush (readback handler)
    ///   dump_scanout_one:                 (scanout dump, different signature) — left alone (not paint-side)
    ///
    /// T1 will move record_paint_op to RenderScheduler, enabling borrow-split
    /// and resolving the borrow-conflict fallbacks above.
    pub fn run_legacy_paint_op<F>(&mut self, record: F) -> Result<(), ash::vk::Result>
    where
        F: FnOnce(
            &crate::kms::vk::device::VkContext,
            ash::vk::CommandBuffer,
        ) -> Result<(), ash::vk::Result>,
    {
        use crate::kms::scheduler::paint_batch::BatchFlushReason;
        if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
            // ProtocolBarrier is strict — flush failure here means the
            // batch was Poisoned or the renderer failed. Either way
            // the legacy op cannot proceed safely.
            log::warn!("run_legacy_paint_op: pre-flush failed ({e:?})");
            return Err(e);
        }
        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return Err(ash::vk::Result::ERROR_INITIALIZATION_FAILED);
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return Err(ash::vk::Result::ERROR_INITIALIZATION_FAILED);
        };
        if let Err(e) = crate::kms::vk::ops::run_one_shot_op(&vk_arc, pool_handle, record) {
            log::error!(
                "run_legacy_paint_op: run_one_shot_op returned fatal {e:?}; \
                 latching renderer_failed — KMS renderer disabled until restart"
            );
            self.renderer_failed = true;
            return Err(e);
        }
        Ok(())
    }

    fn open_with_commit(
        device_path: &str,
        commit: fn(
            &crate::drm::Device,
            &crate::drm::modeset::Output,
            ::drm::control::framebuffer::Handle,
        ) -> io::Result<()>,
    ) -> io::Result<Self> {
        let device = Arc::new(drm::Device::open(device_path)?);
        let (render_node_fd, render_node_path) =
            match crate::kms::render_node::open_for_card(&*device) {
                Ok((fd, path)) => {
                    use std::os::fd::AsRawFd;
                    let raw = fd.as_raw_fd();
                    let stat_minor = std::fs::metadata(&path)
                        .ok()
                        .map(|m| {
                            use std::os::unix::fs::MetadataExt;
                            let rdev = m.rdev();
                            ((rdev >> 8) & 0xff, rdev & 0xff)
                        })
                        .map(|(maj, min)| format!("{maj}:{min}"))
                        .unwrap_or_else(|| "?".into());
                    log::info!(
                        "DRI3 render node ready (sibling of {device_path}): fd={raw} \
                     path={path:?} rdev={stat_minor} (render node minor should be >=128)"
                    );
                    (Some(fd), Some(path))
                }
                Err(err) => {
                    log::warn!(
                        "DRI3 render node unavailable: {err}; DRI3 import path will be \
                     unavailable but the rest of yserver continues"
                    );
                    (None, None)
                }
            };
        let outputs = drm::modeset::discover_outputs(&device)?;

        // Horizontal layout in connector order. If anything fails part
        // way through bring-up, disable everything we have already
        // committed so the next caller starts from a clean slate.
        // TODO(phase-6.10): no unit test exercises this rollback yet —
        // discover_outputs requires a real DRM device, so a synthetic
        // seam isn't cheap. Manual smoke at Step 5/6 covers it.
        let mut layouts: Vec<OutputLayout> = Vec::with_capacity(outputs.len());
        let mut next_x: i32 = 0;
        let mut bring_up_err: Option<io::Error> = None;
        for output in outputs {
            let w = output.picked.width;
            let h = output.picked.height;
            let mut buffers = Vec::with_capacity(2);
            let mut buffer_err: Option<io::Error> = None;
            for _ in 0..2 {
                match drm::Buffer::new(Arc::clone(&device), w, h) {
                    Ok(b) => buffers.push(b),
                    Err(e) => {
                        buffer_err = Some(e);
                        break;
                    }
                }
            }
            if let Some(e) = buffer_err {
                bring_up_err = Some(e);
                break;
            }
            let initial_fb = buffers[0].fb_id();
            if let Err(e) = commit(&device, &output, initial_fb) {
                bring_up_err = Some(e);
                break;
            }
            let swapchain = drm::Swapchain::with_initial_scanout(buffers, 0);
            layouts.push(OutputLayout {
                output,
                swapchain,
                x: next_x,
                y: 0,
                width: w,
                height: h,
                damage: crate::kms::scheduler::damage::OutputDamageState::new(),
                composite_pools: None,
            });
            next_x = next_x.saturating_add(i32::from(w));
        }
        if let Some(err) = bring_up_err {
            for done in layouts.iter().rev() {
                let _ = drm::modeset::disable_output(&device, &done.output);
            }
            return Err(err);
        }

        // fb_w / fb_h carry the virtual-screen extent. Saturating
        // cast: huge layouts that exceed u16 are clamped — the rest
        // of the backend assumes u16 framebuffer dims (changing that
        // is out of scope for Step 3).
        let fb_w: u16 = layouts
            .iter()
            .map(|l| u16::try_from(l.x.saturating_add(i32::from(l.width))).unwrap_or(u16::MAX))
            .max()
            .unwrap_or(0);
        let fb_h: u16 = layouts
            .iter()
            .map(|l| u16::try_from(l.y.saturating_add(i32::from(l.height))).unwrap_or(u16::MAX))
            .max()
            .unwrap_or(0);

        let input_ctx = match crate::input::SendContext::new() {
            Ok(ctx) => Some(ctx),
            Err(err) => {
                log::warn!("libinput unavailable, continuing without input: {err}");
                None
            }
        };

        let ctx = XkbContext(xkbcommon::xkb::Context::new(
            xkbcommon::xkb::CONTEXT_NO_FLAGS,
        ));
        let keymap = xkbcommon::xkb::Keymap::new_from_names(
            &ctx.0,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .or_else(|| {
            xkbcommon::xkb::Keymap::new_from_names(
                &ctx.0,
                "",
                "",
                "",
                "",
                None,
                xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        })
        .ok_or_else(|| io::Error::other("failed to create xkb keymap"))?;
        let xkb_state = XkbState(xkbcommon::xkb::State::new(&keymap));
        let xkb_keymap = XkbKeymap(keymap);

        let mut xid_map: HostXidMap = HashMap::new();
        xid_map.insert(0x0000_0001u32, ResourceId(0x0000_0100));

        // Phase 4.1.1: Vulkan is brought up alongside pixman but doesn't
        // drive any rendering yet. If no ICD is available (e.g. virtio-
        // gpu-pci without VK_ICD_FILENAMES pointing at lavapipe), keep
        // running on the pixman path so the existing recipe matrix
        // stays usable. 4.1.2+ will harden this once Vulkan is
        // load-bearing.
        let vk = match crate::kms::vk::device::VkContext::new() {
            Ok(ctx) => {
                let device_name = unsafe {
                    ctx.instance
                        .get_physical_device_properties(ctx.physical_device)
                }
                .device_name_as_c_str()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "<unknown>".to_string());
                log::info!("vulkan initialised on physical device {device_name}");
                Some(ctx)
            }
            Err(e) => {
                log::warn!("vulkan init failed, continuing on pixman-only path: {e}");
                None
            }
        };

        // Per-output scanout pools. Vulkan-first: each bo is a
        // VkImage (TILING_LINEAR) exported as a dma-buf and imported
        // as a DRM framebuffer. Best-effort: any failure here falls
        // back to None (existing dumb-buffer path stays intact for
        // that output). 3 bos per pool per design §2.
        let scanout_pools: Vec<Option<crate::kms::vk::scanout::ScanoutBoPool>> =
            if let Some(vkctx) = vk.as_ref() {
                layouts
                    .iter()
                    .map(|l| {
                        match crate::kms::vk::scanout::ScanoutBoPool::allocate(
                            Arc::clone(vkctx),
                            Arc::clone(&device),
                            u32::from(l.width),
                            u32::from(l.height),
                            3,
                        ) {
                            Ok(pool) => {
                                if let Some(first) = pool.bos.first() {
                                    log::info!(
                                        "scanout pool: 3x{}x{} bos for output {} (pitch {})",
                                        l.width,
                                        l.height,
                                        l.output.connector_name,
                                        first.pitch
                                    );
                                }
                                Some(pool)
                            }
                            Err(e) => {
                                log::warn!(
                                    "scanout pool: allocation failed for output {}: {e} \
                                     — falling back to dumb-buffer scanout for this output",
                                    l.output.connector_name
                                );
                                None
                            }
                        }
                    })
                    .collect()
            } else {
                std::iter::repeat_with(|| None)
                    .take(layouts.len())
                    .collect()
            };
        let layouts_len = layouts.len();

        // Compositor pipeline (Phase 4.1.3.4): graphics pipeline +
        // sampler + descriptor set layout for the textured-quad
        // composite. Color format must match the scanout bos —
        // those are `B8G8R8A8_UNORM` (per `vk/scanout.rs`'s
        // `allocate_vk_scanout_image`).
        let compositor_pipeline = vk.as_ref().and_then(|vkctx| {
            match crate::kms::vk::pipeline::CompositorPipeline::new(
                Arc::clone(vkctx),
                ash::vk::Format::B8G8R8A8_UNORM,
            ) {
                Ok(p) => Some(p),
                Err(e) => {
                    log::warn!(
                        "compositor pipeline init failed: {e} — Vulkan composite path will \
                         remain disabled; falls back to pixman composite"
                    );
                    None
                }
            }
        });

        // Drawing-op command pool (Phase 4.1.4): graphics CBs for
        // direct-to-mirror writes (`PolyFillRectangle`, `ClearArea`,
        // …). RAII wrapper destroys the pool on shutdown.
        let ops_command_pool = vk.as_ref().and_then(|vkctx| {
            match crate::kms::vk::ops::OpsCommandPool::new(Arc::clone(vkctx)) {
                Ok(p) => Some(p),
                Err(e) => {
                    log::warn!(
                        "ops command pool init failed: {e:?} — Phase 4.1.4 ops will fall back \
                         to the pixman path"
                    );
                    None
                }
            }
        });

        // Image-transfer staging buffer (Phase 4.1.4.3). 1 MiB starter
        // covers most PutImage requests; grows on demand. None when
        // Vulkan didn't come up.
        let ops_staging =
            vk.as_ref().and_then(|vkctx| {
                match crate::kms::vk::ops::OpsStaging::new(Arc::clone(vkctx), 1024 * 1024) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        log::warn!(
                            "ops staging buffer init failed: {e:?} — Phase 4.1.4.3 image ops \
                         will fall back to the pixman path"
                        );
                        None
                    }
                }
            });

        // Glyph atlas (Phase 4.1.4.5). 4096² R8 fixed allocation.
        let glyph_atlas =
            vk.as_ref().and_then(|vkctx| {
                match crate::kms::vk::glyph::GlyphAtlas::new(Arc::clone(vkctx)) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        log::warn!(
                            "glyph atlas init failed: {e:?} — Phase 4.1.4.5 text ops will \
                         fall back to the pixman path"
                        );
                        None
                    }
                }
            });

        // Text pipeline (Phase 4.1.4.5). Built once for the
        // mirror's `B8G8R8A8_UNORM` format; binds the atlas at
        // construction.
        let text_pipeline = match (vk.as_ref(), glyph_atlas.as_ref()) {
            (Some(vkctx), Some(atlas)) => {
                match crate::kms::vk::text_pipeline::TextPipeline::new(
                    Arc::clone(vkctx),
                    ash::vk::Format::B8G8R8A8_UNORM,
                    atlas,
                ) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        log::warn!(
                            "text pipeline init failed: {e:?} — Phase 4.1.4.5 text ops \
                             will fall back to the pixman path"
                        );
                        None
                    }
                }
            }
            _ => None,
        };

        // RENDER `Composite` pipeline cache (Phase 4.1.4.6
        // commit 1). Pipelines compile lazily on first use of
        // each PictOp; the cache spans the rest of the session.
        let render_pipelines = vk.as_ref().and_then(|vkctx| {
            match crate::kms::vk::render_pipeline::RenderPipelineCache::new(Arc::clone(vkctx)) {
                Ok(p) => Some(p),
                Err(e) => {
                    log::warn!(
                        "render pipeline cache init failed: {e:?} — Phase 4.1.4.6 RENDER \
                         ops will fall back to the pixman path"
                    );
                    None
                }
            }
        });

        // Phase 2 of the convolution-filter work (plan
        // 2026-05-15-render-convolution-filter.md). Single-pass
        // arbitrary-kernel pipeline used when a Composite source has
        // `SetPictureFilter convolution`. Failure here leaves the
        // field `None`; `try_vk_render_composite` falls back to the
        // standard pipeline (= no convolution effect, matches
        // pre-phase-2 behaviour).
        let convolution_pipeline = vk.as_ref().and_then(|vkctx| {
            match crate::kms::vk::convolution_pipeline::ConvolutionPipeline::new(Arc::clone(vkctx))
            {
                Ok(p) => Some(p),
                Err(e) => {
                    log::warn!(
                        "convolution pipeline init failed: {e:?} — RENDER convolution-filter \
                         Composite paths will fall back to the standard sampler (= no kernel \
                         effect, hard-edged shadows)"
                    );
                    None
                }
            }
        });

        // 1×1 BGRA8 scratch for `SolidFill` source / `render_fill_rectangles`
        // colour. `cmd_clear_color_image` rewrites it inside each
        // composite CB before sampling.
        let solid_src_image =
            vk.as_ref().and_then(
                |vkctx| match crate::kms::vk::render_pipeline::SolidColorImage::new(Arc::clone(
                    vkctx,
                )) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        log::warn!(
                            "solid colour image init failed: {e:?} — Phase 4.1.4.6 SolidFill \
                         RENDER ops will fall back to the pixman path"
                        );
                        None
                    }
                },
            );

        // Second 1×1 BGRA8 scratch for `SolidFill` mask. Same shape
        // as `solid_src_image`; cleared per-call to the mask
        // picture's colour.
        let solid_mask_image =
            vk.as_ref().and_then(
                |vkctx| match crate::kms::vk::render_pipeline::SolidColorImage::new(Arc::clone(
                    vkctx,
                )) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        log::warn!("solid mask scratch init failed: {e:?}");
                        None
                    }
                },
            );

        // 1×1 BGRA8 mask cleared once to opaque white at backend
        // init. Bound as `mask_tex` for Composite calls without a
        // mask — `mask.a == 1.0` makes the multiplication a no-op
        // and keeps the shader / descriptor layout uniform.
        let white_mask_image = match (vk.as_ref(), ops_command_pool.as_ref()) {
            (Some(vkctx), Some(pool)) => {
                match crate::kms::vk::render_pipeline::SolidColorImage::new(Arc::clone(vkctx)) {
                    Ok(mut s) => {
                        let pool_handle = pool.handle();
                        match crate::kms::vk::ops::run_one_shot_op(vkctx, pool_handle, |vk, cb| {
                            crate::kms::vk::render_pipeline::record_solid_color_clear(
                                vk,
                                cb,
                                &mut s,
                                [1.0, 1.0, 1.0, 1.0],
                            );
                            Ok(())
                        }) {
                            Ok(()) => Some(s),
                            Err(e) => {
                                log::warn!("white mask scratch clear failed: {e:?}");
                                None
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("white mask scratch init failed: {e:?}");
                        None
                    }
                }
            }
            _ => None,
        };

        let mask_scratch = vk.as_ref().and_then(|vkctx| {
            match crate::kms::vk::mask_scratch::MaskScratch::new(Arc::clone(vkctx)) {
                Ok(s) => Some(s),
                Err(e) => {
                    log::warn!("vk mask scratch init failed: {e:?} — render_trapezoids/triangles will be no-op");
                    None
                }
            }
        });

        let copy_scratch = vk.as_ref().and_then(|vkctx| {
            match crate::kms::vk::copy_scratch::CopyScratch::new(Arc::clone(vkctx)) {
                Ok(s) => Some(s),
                Err(e) => {
                    log::warn!(
                        "vk copy scratch init failed: {e:?} — same-target CopyArea overlap dropped"
                    );
                    None
                }
            }
        });

        let dst_readback = vk
            .as_ref()
            .map(|vkctx| crate::kms::vk::dst_readback::DstReadback::new(Arc::clone(vkctx)));

        // Backend-owned pixmap-backing recycle pool (pixmap-pool T1).
        // Created once Vulkan is up; `free_pixmap` returns mirrors
        // here via the scheduler's defer-release mechanism and
        // `allocate_pixmap_mirror` will try-take from it ahead of
        // freshly allocating (wired in T3).
        let pixmap_pool = vk.as_ref().map(|vkctx| {
            let p = Arc::new(crate::kms::vk::pixmap_pool::PixmapPool::new(Arc::clone(
                vkctx,
            )));
            crate::kms::vk::pixmap_pool::register_for_telemetry(&p);
            p
        });

        // GPU trap-rasterize pipeline (gpu-trap T1/T2). When this
        // fails to build, traps fall back to pixman — the renderer
        // still functions, just on the CPU rasterize path. Logged
        // so the bring-up state is auditable.
        let trap_pipeline = vk.as_ref().and_then(|vkctx| {
            match crate::kms::vk::trap_pipeline::TrapPipeline::new(
                Arc::clone(vkctx),
                ash::vk::Format::R8_UNORM,
            ) {
                Ok(p) => Some(p),
                Err(e) => {
                    log::warn!(
                        "vk trap pipeline init failed: {e:?} — traps will fall back to pixman"
                    );
                    None
                }
            }
        });

        let logic_fill_pipelines = vk.as_ref().and_then(|vkctx| {
            match crate::kms::vk::logic_fill_pipeline::LogicFillPipelineCache::new(
                Arc::clone(vkctx),
                ash::vk::Format::B8G8R8A8_UNORM,
            ) {
                Ok(c) => Some(c),
                Err(e) => {
                    log::warn!(
                        "vk logic-fill pipeline cache init failed: {e:?} — non-Copy GC \
                         function fills dropped"
                    );
                    None
                }
            }
        });

        let mut me = Self {
            device,
            render_node_fd,
            render_node_path,
            dri3_sync_resources: HashMap::new(),
            dri3_xshmfences: HashMap::new(),
            outputs: layouts,
            fb_w,
            fb_h,
            windows: HashMap::new(),
            next_host_xid: 0x0040_0000,
            top_level_order: Vec::new(),
            window_id: 1,
            root_visual_xid: 0x21,
            xid_map,
            xkb_context: ctx,
            xkb_keymap,
            xkb_state,
            input_ctx,
            vk,
            first_pageflip_logged: vec![false; layouts_len],
            scheduler: crate::kms::scheduler::RenderScheduler::new(),
            pixmap_pool,
            trap_pipeline,
            scanout_pools,
            compositor_pipeline,
            ops_command_pool,
            renderer_failed: false,
            shutting_down: false,
            composite_defer_stats: CompositeDeferStats::default(),
            ops_staging,
            glyph_atlas,
            text_pipeline,
            render_pipelines,
            convolution_pipeline,
            solid_src_image,
            solid_mask_image,
            white_mask_image,
            mask_scratch,
            copy_scratch,
            dst_readback,
            logic_fill_pipelines,
            font_loader: FontLoader::new()?,
            fonts: HashMap::new(),
            pixmaps: HashMap::new(),
            bg_pixel: None,
            bg_pixmap: None,
            // Initial cursor position: screen center, matching
            // input_thread::LibinputThreadState::new() which seeds
            // its own cursor at (fb_w/2, fb_h/2). Without this,
            // KMS-backend state tracks the cursor at (0, 0) until
            // the first libinput motion arrives — and during that
            // window, any synthetic EnterNotify generated by
            // window-map / focus changes carries (0, 0). GTK's
            // gesture-drag controller in caja-desktop anchors its
            // drag-start at the first event it sees, so a stray
            // (0, 0) Enter locks the anchor at screen origin and
            // produces a rubber-band selection from origin to the
            // click point on every subsequent single click.
            cursor_x: f32::from(fb_w) / 2.0,
            cursor_y: f32::from(fb_h) / 2.0,
            cursors: HashMap::new(),
            active_cursor: None,
            cursor_plane: None,
            hw_cursor_xid: None,
            hw_cursor_hotspot: (0, 0),
            button_mask: 0,
            prev_pointer_window: None,
            pending_pointer_events: Vec::new(),
            current_font: None,
            current_function: GcFunction::Copy,
            current_foreground: 0,
            current_background: 0x00ff_ffff,
            current_fill: FillState::Solid,
            current_clip: ClipState::None,
            pictures: HashMap::new(),
            picture_rescued_images: HashMap::new(),
            glyphsets: HashMap::new(),
            shape_bounding: HashMap::new(),
            shape_clip: HashMap::new(),
            shape_input: HashMap::new(),
            alias_registry: AliasRegistry::default(),
            host_window_to_backing: HashMap::new(),
        };
        // Try to bring up the DRM hardware cursor plane. A failure
        // here is non-fatal — the compositor falls back to the
        // Vulkan-composited cursor quad. Logged so we know which
        // path is active.
        match crate::kms::cursor_plane::CursorPlane::new(Arc::clone(&me.device)) {
            Ok(plane) => {
                log::info!("kms: hardware cursor plane initialised (64x64 ARGB8888)");
                me.cursor_plane = Some(plane);
            }
            Err(e) => {
                log::warn!(
                    "kms: hardware cursor plane init failed ({e}); falling back to \
                     Vulkan-composited cursor quad"
                );
            }
        }
        // Install a built-in X-shaped cursor as the universal fallback;
        // any later DefineCursor on the root window will override it.
        me.install_default_cursor();
        Ok(me)
    }

    fn next_host_xid(&mut self) -> u32 {
        self.next_host_xid = self
            .next_host_xid
            .checked_add(1)
            .expect("xid space exhausted");
        self.next_host_xid
    }

    /// Build the classic X-shaped default cursor and install it as the
    /// initial `active_cursor`. Used before any client calls
    /// DefineCursor — without it, the wallpaper area shows nothing
    /// during early startup (and after that, until fvwm sets a root
    /// cursor). 16×16, 2-pixel-thick black X with a 1-pixel white halo
    /// for visibility on dark backgrounds. Hotspot at the center.
    #[allow(dead_code)] // associated-impl shim isn't strictly needed, kept for future use
    fn _placeholder(&self) {}

    /// Refresh the hardware cursor plane's image to match the current
    /// effective cursor. Idempotent — bails out cheaply when the
    /// currently-uploaded cursor XID still matches `effective_cursor()`.
    ///
    /// Called from cursor-change paths (DefineCursor, install_default_cursor,
    /// crossing into a window with a different cursor). Position is
    /// handled separately by [`Self::hw_cursor_move`] — this routine
    /// only touches the kernel ioctl on actual *image* changes.
    ///
    /// Falls back to hiding the plane (so the Vulkan cursor quad takes
    /// over) when:
    /// - The plane wasn't initialised (`cursor_plane` is `None`),
    /// - The cursor extent exceeds `HW_CURSOR_W × HW_CURSOR_H`,
    /// - The GPU readback or load_image fails.
    pub(crate) fn hw_cursor_refresh(&mut self) {
        use crate::kms::{
            cursor_plane::{HW_CURSOR_H, HW_CURSOR_W},
            vk::ops::{image as vk_image, run_one_shot_op},
        };

        if self.cursor_plane.is_none() {
            return;
        }
        let Some(cursor_xid) = self.effective_cursor() else {
            // No cursor at all — hide the plane.
            self.hw_cursor_hide_all();
            return;
        };
        if self.hw_cursor_xid == Some(cursor_xid) {
            return;
        }

        // Snapshot extent + hotspot without holding a borrow.
        let Some(cs) = self.cursors.get(&cursor_xid) else {
            return;
        };
        let (cw, ch) = (cs.extent.width, cs.extent.height);
        let (hot_x, hot_y) = (cs.hot_x, cs.hot_y);
        if cs.vk_mirror.is_none() {
            // No GPU image — we can't populate the plane buffer.
            self.hw_cursor_hide_all();
            return;
        }
        if cw == 0 || ch == 0 || cw > HW_CURSOR_W || ch > HW_CURSOR_H {
            // Cursor too large for the hardware plane — hide it and
            // let the composite path render the quad instead.
            self.hw_cursor_hide_all();
            return;
        }
        let bpp = 4u32; // cursor mirrors are BGRA8

        // GPU readback into ops_staging. Same shape as
        // `read_mirror_pixels` but reads directly from a cursor mirror
        // (which lives in `self.cursors`, not in `windows`/`pixmaps`).
        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return;
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return;
        };
        if self.ops_staging.is_none() {
            return;
        }

        // Ensure any pending paint batch work is flushed before we read
        // pixels back to the CPU. In Phase 3A the batch is always Idle so
        // this is a no-op; it becomes load-bearing once 3B migrates recorders.
        if let Err(e) =
            self.flush_if_needed(crate::kms::scheduler::paint_batch::BatchFlushReason::Readback)
        {
            log::warn!("hw_cursor_refresh: pre-flush failed ({e:?}); skipping cursor update");
            return;
        }

        let total_bytes = u64::from(cw) * u64::from(ch) * u64::from(bpp);
        if let Err(e) = self
            .ops_staging
            .as_mut()
            .expect("checked is_none above")
            .ensure(total_bytes)
        {
            log::warn!("hw_cursor_refresh: staging grow failed for {total_bytes} bytes: {e:?}");
            return;
        }
        let regions = [ash::vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                ash::vk::ImageSubresourceLayers::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_offset(ash::vk::Offset3D { x: 0, y: 0, z: 0 })
            .image_extent(ash::vk::Extent3D {
                width: cw,
                height: ch,
                depth: 1,
            })];
        let staging_buffer = self.ops_staging.as_ref().expect("present").buffer();
        let Some(mirror) = self
            .cursors
            .get_mut(&cursor_xid)
            .and_then(|c| c.vk_mirror.as_mut())
        else {
            return;
        };
        if let Err(e) = run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            vk_image::record_get_image(vk, cb, mirror, staging_buffer, &regions)
        }) {
            log::error!(
                "hw_cursor_refresh: run_one_shot_op returned fatal {e:?}; \
                 latching renderer_failed — KMS renderer disabled until restart"
            );
            self.renderer_failed = true;
            return;
        }
        let total_bytes_usize = total_bytes as usize;
        let mut bytes = vec![0u8; total_bytes_usize];
        // SAFETY: `ensure(total_bytes)` succeeded above, and
        // `run_one_shot_op` waited for the submit before returning,
        // so the staging range is now valid.
        unsafe {
            let staging_ptr = self.ops_staging.as_ref().expect("present").mapped_ptr();
            std::ptr::copy_nonoverlapping(staging_ptr, bytes.as_mut_ptr(), total_bytes_usize);
        }

        let Some(plane) = self.cursor_plane.as_mut() else {
            return;
        };
        if let Err(e) = plane.load_image(cw, ch, &bytes) {
            log::warn!("hw_cursor_refresh: load_image failed: {e}");
            return;
        }
        // Re-bind on every CRTC. Even if the same cursor was already
        // showing, set_cursor2 has to be called again to swap in the
        // new image (the kernel doesn't peek at the dumb buffer
        // contents — only at the buffer handle).
        let layouts_snapshot: Vec<::drm::control::crtc::Handle> =
            self.outputs.iter().map(|l| l.output.crtc).collect();
        for crtc_handle in layouts_snapshot {
            if let Err(e) = plane.show(crtc_handle, (i32::from(hot_x), i32::from(hot_y))) {
                log::warn!("hw_cursor_refresh: set_cursor2 failed on crtc: {e}");
            }
        }
        self.hw_cursor_xid = Some(cursor_xid);
        self.hw_cursor_hotspot = (hot_x, hot_y);

        // Position the freshly-shown cursor immediately so it doesn't
        // sit at stale per-CRTC coords from a previous cursor's life.
        self.hw_cursor_move();
    }

    /// Issue `drmModeMoveCursor` on every CRTC to reflect the current
    /// `(cursor_x, cursor_y)`. Cheap — one ioctl per output, no GPU
    /// involvement. Called on every pointer-absolute event when the
    /// HW plane is active.
    pub(crate) fn hw_cursor_move(&mut self) {
        let Some(plane) = self.cursor_plane.as_mut() else {
            return;
        };
        if self.hw_cursor_xid.is_none() {
            return;
        }
        let (hot_x, hot_y) = self.hw_cursor_hotspot;
        let cursor_x = self.cursor_x as i32;
        let cursor_y = self.cursor_y as i32;
        for layout in &self.outputs {
            // CRTC-local coords. The kernel clips outside the visible
            // rect on each CRTC (effectively hiding the cursor on that
            // output), so we just compute and submit blindly.
            let cx = cursor_x - layout.x - i32::from(hot_x);
            let cy = cursor_y - layout.y - i32::from(hot_y);
            if let Err(e) = plane.move_to(layout.output.crtc, cx, cy) {
                log::warn!("hw_cursor_move: move_cursor failed: {e}");
            }
        }
    }

    /// Detach the cursor plane from every CRTC. Used when the
    /// effective cursor is too large for the plane (forcing fallback
    /// to the composite quad) or when there's no cursor at all.
    pub(crate) fn hw_cursor_hide_all(&mut self) {
        let Some(plane) = self.cursor_plane.as_mut() else {
            return;
        };
        let layouts_snapshot: Vec<::drm::control::crtc::Handle> =
            self.outputs.iter().map(|l| l.output.crtc).collect();
        for crtc_handle in layouts_snapshot {
            if let Err(e) = plane.hide(crtc_handle) {
                log::warn!("hw_cursor_hide_all: hide failed: {e}");
            }
        }
        self.hw_cursor_xid = None;
    }

    /// True when the HW cursor plane currently has pixels uploaded
    /// and is bound to at least one CRTC. The composite-scene builder
    /// uses this to skip pushing the Vulkan cursor quad — without
    /// this gate both the kernel overlay and the GPU quad would draw
    /// the cursor twice (the latter trailing by one composite cadence
    /// — which was the whole problem this fixes).
    #[must_use]
    pub(crate) fn hw_cursor_active(&self) -> bool {
        self.cursor_plane
            .as_ref()
            .is_some_and(crate::kms::cursor_plane::CursorPlane::is_visible)
            && self.hw_cursor_xid.is_some()
    }
}

/// Advance every bo in a [`ScanoutBoPool`] one step on
/// pageflip-complete (per design §2 transition table):
///
/// - `Retiring → Free` (release fence is signalled by the kernel
///   issuing the new pageflip-complete; we close the fd).
/// - `OnScreen → Retiring` (its successor just landed on screen).
/// - `Pending → OnScreen` (the freshly-flipped bo is now scanning).
///
/// Order matters: snapshot phases first to drive transitions, so
/// each bo gets exactly one state change per event regardless of
/// iteration order.
fn advance_pool_on_pageflip_complete(pool: &mut crate::kms::vk::scanout::ScanoutBoPool) {
    use crate::kms::vk::scanout::BoPhase;
    use std::os::fd::{FromRawFd, OwnedFd};

    let phases: Vec<BoPhase> = pool.bos.iter().map(|b| b.state.phase).collect();
    for (i, phase) in phases.into_iter().enumerate() {
        match phase {
            BoPhase::Retiring => {
                if let Some(fd) = pool.bos[i].state.transition_to_free_after_retire() {
                    // SAFETY: the fd was inserted by the
                    // Submitted→Pending transition; we're the only
                    // owner. Reconstructing as OwnedFd lets Drop
                    // close it.
                    drop(unsafe { OwnedFd::from_raw_fd(fd) });
                }
            }
            BoPhase::OnScreen => pool.bos[i].state.transition_to_retiring(),
            BoPhase::Pending => pool.bos[i].state.transition_to_on_screen(),
            _ => {}
        }
    }
}

/// RENDER primitive list passed through to
/// [`KmsBackend::try_vk_render_traps_or_tris`] (gpu-trap T2 + T3).
///
/// Both variants route through the GPU-rasterize path: the function
/// records a `TrapPipeline` draw that writes coverage into MaskScratch
/// inside the open paint batch. `Traps` uses
/// `TrapPipeline::trapezoid_pipeline()` (4-edge analytic coverage);
/// `Tris` uses `TrapPipeline::triangle_pipeline()` (3-edge analytic
/// coverage with winding-order-aware inside-side selection).
pub(crate) enum TrapsOrTris<'a> {
    /// Decoded trapezoids. T2 GPU-rasterizes via `TrapPipeline`.
    Traps(&'a [crate::kms::vk::ops::traps::Trapezoid]),
    /// Decoded triangles. T3 GPU-rasterizes via `TrapPipeline`'s
    /// sibling `triangle_pipeline`.
    Tris(&'a [crate::kms::vk::ops::traps::Triangle]),
}

impl KmsBackend {
    fn install_default_cursor(&mut self) {
        let w: u16 = 16;
        let h: u16 = 16;
        // Build the cursor pattern straight to a tightly-packed BGRA
        // byte buffer (the layout `B8G8R8A8_UNORM` reads). No pixman
        // detour — the mirror is the canonical store.
        let mut packed = vec![0u8; w as usize * h as usize * 4];
        let last = i32::from(w) - 1;
        for y in 0..i32::from(h) {
            for x in 0..i32::from(w) {
                // Distance to either diagonal of the 16×16 box.
                let d1 = (x - y).abs();
                let d2 = (x + y - last).abs();
                let dist = d1.min(d2);
                let bgra: [u8; 4] = match dist {
                    0 => [0x00, 0x00, 0x00, 0xFF], // black core, opaque
                    1 => [0xFF, 0xFF, 0xFF, 0xFF], // white halo, opaque
                    _ => [0x00, 0x00, 0x00, 0x00], // transparent
                };
                let off = (y as usize * w as usize + x as usize) * 4;
                packed[off..off + 4].copy_from_slice(&bgra);
            }
        }
        let xid = self.next_host_xid();
        let mut vk_mirror = self.allocate_cursor_mirror(u32::from(w), u32::from(h));
        if let Some(mirror) = vk_mirror.as_mut()
            && let Err(e) = self.upload_bgra_to_mirror(mirror, &packed)
        {
            log::warn!("install_default_cursor: mirror upload failed: {e:?}");
        }
        self.cursors.insert(
            xid,
            CursorState {
                extent: ash::vk::Extent2D {
                    width: u32::from(w),
                    height: u32::from(h),
                },
                hot_x: w / 2,
                hot_y: h / 2,
                vk_mirror,
            },
        );
        self.active_cursor = Some(xid);
        // Push the freshly-installed default cursor onto the HW
        // plane (no-op when the plane isn't available).
        self.hw_cursor_refresh();
    }

    /// Upload tightly-packed BGRA bytes into the full extent of
    /// `mirror`. Used by cursor create/update paths that build cursor
    /// pixels host-side (no source `DrawableImage` to copy from).
    ///
    /// Pixman's `A8R8G8B8` LE memory order is `[B, G, R, A]`, which
    /// matches `B8G8R8A8_UNORM` on the GPU exactly — so callers that
    /// pass a pixman-image's raw bytes do not need any byte
    /// permutation.
    ///
    /// The caller passes a `&mut DrawableImage` directly (typically
    /// the freshly-allocated one from `allocate_cursor_mirror`,
    /// before it lands in `self.cursors`), so this method does not
    /// alias any HashMap borrow of `self`.
    fn upload_bgra_to_mirror(
        &mut self,
        mirror: &mut crate::kms::vk::target::DrawableImage,
        pixels: &[u8],
    ) -> Result<(), ash::vk::Result> {
        let needed = pixels.len() as u64;
        if needed == 0 {
            return Ok(());
        }

        let (vk_arc, pool_handle) = self
            .paint_resources()
            .ok_or(ash::vk::Result::ERROR_INITIALIZATION_FAILED)?;

        let extent = mirror.extent;
        let pixels_ptr = pixels.as_ptr();
        let pixels_len = pixels.len();

        let mut arena_oom = false;
        let result = self
            .scheduler
            .record_paint_batch_op(vk_arc, pool_handle, |_vk, batch, cb| {
                let alloc = match batch.upload_arena_mut().alloc(needed, 16) {
                    Ok(a) => a,
                    Err(e) => {
                        // Arena alloc failed BEFORE we recorded anything into the
                        // batch CB. Don't poison the batch — that would drop
                        // unrelated 3B fill/copy work recorded earlier in this
                        // batch (e.g., create_glyph_cursor reaches here with the
                        // batch in Recording state if a fill ran earlier).
                        // Signal failure via the outer flag; the closure returns
                        // Ok(()) so the batch state is unchanged.
                        log::warn!(
                            "vk upload_bgra_to_mirror: arena alloc {needed} bytes failed: {e:?} — \
                             cursor mirror upload will fail without poisoning batch"
                        );
                        arena_oom = true;
                        return Ok(());
                    }
                };
                // SAFETY: `alloc.mapped_ptr` is a HOST_VISIBLE |
                // HOST_COHERENT mapped pointer at `alloc.buffer +
                // alloc.offset` covering `needed` bytes;
                // `pixels_ptr` is valid for `pixels_len` bytes and
                // we checked `pixels_len == needed`.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        pixels_ptr,
                        alloc.mapped_ptr.as_ptr(),
                        pixels_len,
                    );
                }
                mirror.record_upload_rect(
                    cb,
                    alloc.buffer,
                    alloc.offset,
                    ash::vk::Rect2D {
                        offset: ash::vk::Offset2D { x: 0, y: 0 },
                        extent,
                    },
                );
                Ok(())
            });

        if arena_oom {
            return Err(ash::vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        }
        result
    }

    /// Read the full mirror of a window or pixmap drawable back to
    /// host memory via `vkCmdCopyImageToBuffer`. Used by `copy_plane`
    /// (the last remaining pixman-source read) and any future code
    /// that needs to peek at GPU-canonical pixels.
    ///
    /// The returned [`MirrorReadback`] holds tightly-packed bytes in
    /// the mirror's native format (`R8_UNORM` for depth 1/8,
    /// `B8G8R8A8_UNORM` for depth 24/32). Returns `None` if the
    /// drawable is unknown, has no mirror, has zero extent, or the
    /// staging buffer cannot be grown to fit.
    fn read_mirror_pixels(&mut self, host_xid: u32) -> Option<MirrorReadback> {
        use crate::kms::vk::ops::{image as vk_image, run_one_shot_op};

        let vk_arc = self.vk.as_ref().cloned()?;
        let pool_handle = self.ops_command_pool.as_ref().map(|p| p.handle())?;
        self.ops_staging.as_ref()?;

        // Ensure any pending paint batch work is flushed before we read
        // pixels back to the CPU. In Phase 3A the batch is always Idle so
        // this is a no-op; it becomes load-bearing once 3B migrates recorders.
        if let Err(e) =
            self.flush_if_needed(crate::kms::scheduler::paint_batch::BatchFlushReason::Readback)
        {
            log::warn!("read_mirror_pixels: pre-flush failed ({e:?}); returning None");
            return None;
        }

        // Snapshot extent + bpp without holding a mut borrow on self.
        let (mirror_w, mirror_h, mirror_bpp) = {
            let mirror = if let Some(w) = self.windows.get(&host_xid) {
                w.vk_mirror.as_ref()
            } else if let Some(p) = self.pixmaps.get(&host_xid) {
                p.vk_mirror.as_ref()
            } else {
                None
            };
            let mirror = mirror?;
            (
                mirror.extent.width,
                mirror.extent.height,
                mirror.bytes_per_pixel(),
            )
        };
        if mirror_w == 0 || mirror_h == 0 {
            return None;
        }
        let total_bytes = u64::from(mirror_w) * u64::from(mirror_h) * u64::from(mirror_bpp);

        if let Err(e) = self
            .ops_staging
            .as_mut()
            .expect("checked is_none above")
            .ensure(total_bytes)
        {
            log::warn!("read_mirror_pixels: staging grow failed for {total_bytes} bytes: {e:?}");
            return None;
        }

        let regions = [ash::vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                ash::vk::ImageSubresourceLayers::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_offset(ash::vk::Offset3D { x: 0, y: 0, z: 0 })
            .image_extent(ash::vk::Extent3D {
                width: mirror_w,
                height: mirror_h,
                depth: 1,
            })];

        let staging_buffer = self.ops_staging.as_ref().expect("present").buffer();
        let mirror = if let Some(w) = self.windows.get_mut(&host_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&host_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let mirror = mirror?;

        if let Err(e) = run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            vk_image::record_get_image(vk, cb, mirror, staging_buffer, &regions)
        }) {
            log::error!(
                "read_mirror_pixels: run_one_shot_op returned fatal {e:?}; \
                 latching renderer_failed — KMS renderer disabled until restart"
            );
            self.renderer_failed = true;
            return None;
        }

        let total_bytes_usize = total_bytes as usize;
        let mut bytes = vec![0u8; total_bytes_usize];
        // SAFETY: `staging.ensure(total_bytes)` succeeded, and the
        // submit above was waited on inside `run_one_shot_op`, so the
        // staging buffer's host-mapped range now contains the readback.
        unsafe {
            let staging_ptr = self.ops_staging.as_ref().expect("present").mapped_ptr();
            std::ptr::copy_nonoverlapping(staging_ptr, bytes.as_mut_ptr(), total_bytes_usize);
        }

        Some(MirrorReadback {
            width: mirror_w,
            height: mirror_h,
            bytes_per_pixel: mirror_bpp,
            bytes,
        })
    }

    /// Solid-fill the entire extent of `mirror` with the X11 colour
    /// pixel `fg` (0xRRGGBB or 0xAARRGGBB; alpha defaulted to opaque
    /// for windows). Used at create-time / resize-time to apply a
    /// window's `bg_pixel` to a freshly allocated mirror — the
    /// `MirrorUploader` pump that used to handle this is gone.
    fn fill_mirror_solid(
        &mut self,
        mirror: &mut crate::kms::vk::target::DrawableImage,
        fg: u32,
    ) -> Result<(), ash::vk::Result> {
        use crate::kms::vk::ops::fill;
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            return Err(ash::vk::Result::ERROR_DEVICE_LOST);
        };
        let extent = mirror.extent;
        let color = [
            ((fg >> 16) & 0xFF) as f32 / 255.0,
            ((fg >> 8) & 0xFF) as f32 / 255.0,
            (fg & 0xFF) as f32 / 255.0,
            1.0,
        ];
        let rects = [ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent,
        }];
        let scissor = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent,
        };
        self.scheduler
            .record_paint_op(vk_arc, pool_handle, |vk, cb| {
                fill::record_fill_rectangles(vk, cb, mirror, color, &rects, scissor)
            })
    }

    /// Allocate a cursor mirror sized for `src` and `vkCmdCopyImage`
    /// the rescued source into it. Returns `None` if mirror
    /// allocation or the copy fails — caller falls through to a
    /// `None` `vk_mirror` (cursor still inserted, but won't render
    /// pixels until next DefineCursor).
    fn copy_drawable_to_new_cursor_mirror(
        &mut self,
        src: &mut crate::kms::vk::target::DrawableImage,
    ) -> Option<crate::kms::vk::target::DrawableImage> {
        use crate::kms::vk::ops::copy as vk_copy;

        let cw = src.extent.width;
        let ch = src.extent.height;
        let mut cm = self.allocate_cursor_mirror(cw, ch)?;

        let regions = [ash::vk::ImageCopy::default()
            .src_subresource(
                ash::vk::ImageSubresourceLayers::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .src_offset(ash::vk::Offset3D { x: 0, y: 0, z: 0 })
            .dst_subresource(
                ash::vk::ImageSubresourceLayers::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .dst_offset(ash::vk::Offset3D { x: 0, y: 0, z: 0 })
            .extent(ash::vk::Extent3D {
                width: cw,
                height: ch,
                depth: 1,
            })];

        // src and cm are not borrowed from self.windows/pixmaps, so
        // paint_resources() + scheduler.record_paint_op avoids the
        // borrow conflict that run_legacy_paint_op would introduce.
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            log::warn!("copy_drawable_to_new_cursor_mirror: paint_resources unavailable");
            return None;
        };
        if let Err(e) = self
            .scheduler
            .record_paint_op(vk_arc, pool_handle, |vk, cb| {
                vk_copy::record_copy_area_distinct(vk, cb, src, &mut cm, &regions)
            })
        {
            log::warn!("copy_drawable_to_new_cursor_mirror: copy failed: {e:?}");
            return None;
        }
        Some(cm)
    }

    /// Live-pixmap variant of [`copy_drawable_to_new_cursor_mirror`]:
    /// the source mirror lives in `self.pixmaps[host_xid]`, the
    /// destination is owned by the caller. Disjoint mut borrows on
    /// `self.pixmaps` and `cm` keep the borrow checker happy.
    fn copy_pixmap_mirror_to_cursor(
        &mut self,
        host_xid: u32,
        cm: &mut crate::kms::vk::target::DrawableImage,
        cw: u32,
        ch: u32,
    ) -> Result<(), ash::vk::Result> {
        use crate::kms::vk::ops::copy as vk_copy;

        let regions = [ash::vk::ImageCopy::default()
            .src_subresource(
                ash::vk::ImageSubresourceLayers::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .src_offset(ash::vk::Offset3D { x: 0, y: 0, z: 0 })
            .dst_subresource(
                ash::vk::ImageSubresourceLayers::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .dst_offset(ash::vk::Offset3D { x: 0, y: 0, z: 0 })
            .extent(ash::vk::Extent3D {
                width: cw,
                height: ch,
                depth: 1,
            })];
        // `src` is borrowed from `self.pixmaps`; take paint_resources()
        // first (immutable borrows on self.vk / self.ops_command_pool drop
        // at the let-statement end), then borrow self.pixmaps mutably, then
        // call self.scheduler.record_paint_op (disjoint from self.pixmaps).
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            return Err(ash::vk::Result::ERROR_DEVICE_LOST);
        };
        let src = self
            .pixmaps
            .get_mut(&host_xid)
            .and_then(|p| p.vk_mirror.as_mut())
            .ok_or(ash::vk::Result::ERROR_UNKNOWN)?;
        self.scheduler
            .record_paint_op(vk_arc, pool_handle, |vk, cb| {
                vk_copy::record_copy_area_distinct(vk, cb, src, cm, &regions)
            })
    }

    /// Allocate a Vulkan mirror sized for a cursor pixmap. Cursors
    /// always carry alpha (RenderCreateCursor delivers `A8R8G8B8`),
    /// so the mirror format is `B8G8R8A8_UNORM` regardless of depth
    /// — handled identically to a depth-32 pixmap.
    fn allocate_cursor_mirror(
        &self,
        width: u32,
        height: u32,
    ) -> Option<crate::kms::vk::target::DrawableImage> {
        let vkctx = self.vk.as_ref()?;
        if width == 0 || height == 0 {
            return None;
        }
        match crate::kms::vk::target::DrawableImage::new_server_owned_pixmap(
            std::sync::Arc::clone(vkctx),
            width,
            height,
            32,
        ) {
            Ok(mut img) => {
                if let Some(pool) = self.ops_command_pool.as_ref() {
                    crate::vk_count!(init_clear_cursor);
                    if let Err(e) = img.initialize_clear(pool.handle()) {
                        log::warn!("cursor mirror initialize_clear failed: {e:?}");
                    }
                }
                // Cursor mirror is fully dirty at creation so the
                // first composite pass has it populated before
                // sampling.
                img.mark_full_damage();
                Some(img)
            }
            Err(e) => {
                log::warn!(
                    "DrawableImage::new_server_owned_pixmap (cursor {width}x{height}): {e} — \
                     cursor will not render under VkComposite"
                );
                None
            }
        }
    }

    fn drawable_depth(&self, host_xid: u32) -> Option<u8> {
        self.windows
            .get(&host_xid)
            .map(|w| w.depth)
            .or_else(|| self.pixmaps.get(&host_xid).map(|p| p.depth))
    }

    /// Width × height for a window or pixmap drawable, in pixels.
    /// Reads from the recorded `WindowState` / `PixmapState`
    /// dimensions — no pixman buffer involved.
    fn drawable_dims(&self, host_xid: u32) -> Option<(u32, u32)> {
        if let Some(w) = self.windows.get(&host_xid) {
            Some((u32::from(w.width), u32::from(w.height)))
        } else {
            self.pixmaps
                .get(&host_xid)
                .map(|p| (u32::from(p.width), u32::from(p.height)))
        }
    }

    // `with_image_mut` and `image_ptr_for_xid` deleted in 4.1.5.
    // Drawing ops now record into Vk command buffers directly; no
    // helper is needed to grab a pixman pointer or mark the mirror
    // for upload.

    /// Decode the wire-packed clip rectangle list (`Vec<u8>` of i16 x, i16
    /// y, u16 w, u16 h tuples) into `Rectangle16`s in dst-coords (i.e. with
    /// the GC clip-origin already added). Returns `None` if the current GC
    /// clip is `None` or `Pixmap` (latter not yet enforced).
    fn current_clip_rects_in_dst_space(&self) -> Option<Vec<Rectangle16>> {
        let ClipState::Rectangles { origin, rects } = &self.current_clip else {
            return None;
        };
        let bytes = &rects.rectangles;
        let mut out = Vec::with_capacity(bytes.len() / 8);
        for chunk in bytes.chunks_exact(8) {
            let cx = i16::from_le_bytes([chunk[0], chunk[1]]) as i32 + origin.0 as i32;
            let cy = i16::from_le_bytes([chunk[2], chunk[3]]) as i32 + origin.1 as i32;
            let cw = u16::from_le_bytes([chunk[4], chunk[5]]) as i32;
            let ch = u16::from_le_bytes([chunk[6], chunk[7]]) as i32;
            if cw <= 0 || ch <= 0 {
                continue;
            }
            out.push(Rectangle16 {
                x: cx.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                y: cy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                width: cw.min(u16::MAX as i32) as u16,
                height: ch.min(u16::MAX as i32) as u16,
            });
        }
        Some(out)
    }

    /// Intersect each rect in `rects` against the current GC clip. Returns
    /// the original list when no clip is active. For `ClipState::Pixmap` we
    /// pass through untouched (TODO: rasterise the mask).
    fn intersect_with_current_clip(&self, rects: &[Rectangle16]) -> Vec<Rectangle16> {
        let Some(clip_rects) = self.current_clip_rects_in_dst_space() else {
            return rects.to_vec();
        };
        let mut out = Vec::with_capacity(rects.len());
        for r in rects {
            let rx0 = r.x as i32;
            let ry0 = r.y as i32;
            let rx1 = rx0 + r.width as i32;
            let ry1 = ry0 + r.height as i32;
            for c in &clip_rects {
                let cx0 = c.x as i32;
                let cy0 = c.y as i32;
                let cx1 = cx0 + c.width as i32;
                let cy1 = cy0 + c.height as i32;
                let ix0 = rx0.max(cx0);
                let iy0 = ry0.max(cy0);
                let ix1 = rx1.min(cx1);
                let iy1 = ry1.min(cy1);
                if ix0 < ix1 && iy0 < iy1 {
                    out.push(Rectangle16 {
                        x: ix0 as i16,
                        y: iy0 as i16,
                        width: (ix1 - ix0) as u16,
                        height: (iy1 - iy0) as u16,
                    });
                }
            }
        }
        out
    }

    /// Fill `rects` on `dst_xid`, honoring `self.current_fill`. For
    /// `Solid`, paints with `fg`. For `Tiled`, repeats the tile pixmap
    /// (offset by the GC's tile origin). e16 paints popup backgrounds via
    /// Tiled — the menu chrome+text lives in the tile pixmap and the
    /// destination pixmap (the window's bg-pixmap) is filled by tiling
    /// it, so honoring this is required for any visible popup.
    /// `Stippled`/`OpaqueStippled` fall through to solid for now (no real
    /// client driving that path on KMS yet).
    fn fill_rects_honoring_fill_state(&mut self, dst_xid: u32, fg: u32, rects: &[Rectangle16]) {
        let function = self.current_function;
        let fill = self.current_fill.clone();
        let clipped = self.intersect_with_current_clip(rects);
        if clipped.is_empty() {
            return;
        }
        let rects = clipped.as_slice();
        match fill {
            FillState::Tiled { pixmap, origin } => {
                let tile_xid = pixmap.as_raw();
                if tile_xid == dst_xid {
                    log::debug!(
                        "fill_rects_honoring_fill_state: tile == dst (0x{tile_xid:x}); \
                         degenerating to solid"
                    );
                    self.try_vk_fill_with_function(dst_xid, function, fg, rects);
                    return;
                }
                // Tiled fill is composited with Operation::Src, which
                // matches GcFunction::Copy. Other GC functions (Xor /
                // etc.) on a tiled fill aren't covered by any current
                // client; degenerate to a solid logic-op fill so the
                // function is honoured.
                if !matches!(function, GcFunction::Copy) {
                    log::debug!(
                        "fill_rects_honoring_fill_state: Tiled+{function:?} not implemented; \
                         degenerating to solid logic-op fill"
                    );
                    self.try_vk_fill_with_function(dst_xid, function, fg, rects);
                    return;
                }
                if !self.try_vk_tiled_fill(dst_xid, tile_xid, origin.0, origin.1, rects) {
                    log::warn!(
                        "fill_rects_honoring_fill_state: tile fill on 0x{dst_xid:x} from \
                         tile 0x{tile_xid:x} failed; rect dropped"
                    );
                }
            }
            FillState::Solid | FillState::Stippled { .. } | FillState::OpaqueStippled { .. } => {
                // Stipple cases share this arm — proper stipple
                // support is task #4.1.4.8 territory; until then
                // they fall through as solid (matches the pre-
                // pixman-removal behaviour).
                self.try_vk_fill_with_function(dst_xid, function, fg, rects);
            }
        }
    }

    /// Phase 4.1.5 prep: tile fill via `try_vk_render_composite`.
    /// Tile pixmap supplies the source colours; `Repeat::Normal`
    /// makes it wrap. Per-rect source origin `(rect_dst_x - ox,
    /// rect_dst_y - oy)` so that the shader's `src_origin +
    /// dst_offset` lands on the right tile pixel.
    fn try_vk_tiled_fill(
        &mut self,
        dst_xid: u32,
        tile_xid: u32,
        ox: i16,
        oy: i16,
        rects: &[Rectangle16],
    ) -> bool {
        if rects.is_empty() {
            return true;
        }
        if tile_xid == dst_xid {
            return false; // self-tile would alias src and dst
        }
        // Tile must have a Vk mirror to be sampleable.
        let tile_format = if let Some(w) = self.windows.get(&tile_xid) {
            w.vk_mirror.as_ref().map(|m| m.format)
        } else if let Some(p) = self.pixmaps.get(&tile_xid) {
            p.vk_mirror.as_ref().map(|m| m.format)
        } else {
            None
        };
        if tile_format != Some(ash::vk::Format::B8G8R8A8_UNORM) {
            return false;
        }
        // Build CompositeRects in dst space; the shader's
        // `(src_origin + dst_offset)` lands on the right tile pixel
        // when `src_origin = rect_dst - tile_origin`.
        let composite_rects: Vec<crate::kms::vk::ops::render::CompositeRect> = rects
            .iter()
            .map(|r| crate::kms::vk::ops::render::CompositeRect {
                src_x: i32::from(r.x) - i32::from(ox),
                src_y: i32::from(r.y) - i32::from(oy),
                mask_x: 0,
                mask_y: 0,
                dst_x: i32::from(r.x),
                dst_y: i32::from(r.y),
                width: u32::from(r.width),
                height: u32::from(r.height),
            })
            .collect();
        // Use a coarse scissor that covers all rects.
        let mut x0 = i32::MAX;
        let mut y0 = i32::MAX;
        let mut x1 = i32::MIN;
        let mut y1 = i32::MIN;
        for r in rects {
            x0 = x0.min(i32::from(r.x));
            y0 = y0.min(i32::from(r.y));
            x1 = x1.max(i32::from(r.x) + i32::from(r.width));
            y1 = y1.max(i32::from(r.y) + i32::from(r.height));
        }
        if x1 <= x0 || y1 <= y0 {
            return true;
        }
        let scissor = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: x0.max(0),
                y: y0.max(0),
            },
            extent: ash::vk::Extent2D {
                width: u32::try_from((x1 - x0.max(0)).max(0)).unwrap_or(0),
                height: u32::try_from((y1 - y0.max(0)).max(0)).unwrap_or(0),
            },
        };
        // Op `Src` (1) — tile fill replaces the destination.
        const OP_SRC: u8 = 1;
        self.try_vk_render_composite(
            OP_SRC,
            RenderPic::Drawable(tile_xid),
            RenderPic::None,
            dst_xid,
            &composite_rects,
            scissor,
            Repeat::Normal,
            Repeat::None,
            None,
            None,
            false,
            &PictureFilter::Nearest,
        )
    }

    /// Sub-phase 4.1.4.4 helper. Solid-fill `rects` on `dst_xid`,
    /// honouring the current GC `function`, with the Vulkan
    /// fast-fill path when `function == Copy` and the target has
    /// a mirror. Used by the stroke-style poly ops (`PolyLine`,
    /// `PolySegment`, `PolyPoint`, `PolyArc`, `PolyRectangle`),
    /// where every rasterised rect is in the GC's single
    /// foreground colour. This is the same routing
    /// `fill_rects_honoring_fill_state` does for the
    /// `FillState::Solid` arm — split out because stroke ops
    /// always treat the request as "Solid foreground" regardless
    /// of GC `fill_style`, while filled ops respect it.
    fn fill_rects_solid_with_gc_function(&mut self, dst_xid: u32, fg: u32, rects: &[Rectangle16]) {
        if rects.is_empty() {
            return;
        }
        let function = self.current_function;
        // Phase 4.1.5: Vk-only. `try_vk_fill_with_function` picks
        // the Copy fast path or the per-`VkLogicOp` pipeline.
        self.try_vk_fill_with_function(dst_xid, function, fg, rects);
    }

    /// Vulkan-direct `CopyArea` via `vkCmdCopyImage`. Returns
    /// `true` iff the Vulkan path wrote and the caller can return;
    /// `false` means the caller falls back to pixman.
    ///
    /// Conditions for the Vulkan path: both drawables have
    /// mirrors, mirrors share the same Vulkan format,
    /// same-target draws don't overlap (overlap → staging-image
    /// path is 4.1.4.2 follow-up territory).
    #[allow(clippy::too_many_arguments)]
    fn try_vk_copy_area(
        &mut self,
        src_xid: u32,
        dst_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> bool {
        use crate::kms::vk::ops::copy;

        // paint_resources() checks renderer_failed + extracts vk/pool.
        // Taken here before any self.windows/pixmaps borrow so the
        // immutable borrows on self.vk / self.ops_command_pool drop
        // before the mutable map borrows below.
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            return false;
        };

        // GC clip the dst rect into a sub-rect list. Each
        // surviving sub-rect's src offset shifts by the same
        // amount the dst was clipped (mirrors the existing pixman
        // path).
        let dst_rect = Rectangle16 {
            x: dst_x,
            y: dst_y,
            width,
            height,
        };
        let sub_rects = self.intersect_with_current_clip(&[dst_rect]);
        if sub_rects.is_empty() {
            // Fully clipped — nothing to draw, but the request is
            // "handled" successfully.
            return true;
        }

        let same_target = src_xid == dst_xid;
        if same_target {
            let overlapping = rects_overlap_axis_aligned(
                i32::from(src_x),
                i32::from(src_y),
                i32::from(dst_x),
                i32::from(dst_y),
                i32::from(width),
                i32::from(height),
            );

            if overlapping {
                // Overlap: round-trip through `copy_scratch` (single
                // shared GPU-resident scratch image, no host staging).
                //
                // 3D migration: append to the open PaintBatch via
                // record_paint_batch_op instead of run_one_shot_op.
                // The closure holds three disjoint &mut field borrows
                // simultaneously: scheduler (via the call), mirror
                // (re-borrowed from windows/pixmaps), and scratch
                // (re-borrowed from self.copy_scratch). Disjoint field
                // paths make this OK with the borrow checker.
                //
                // vk_arc / pool_handle come from the outer-scope
                // paint_resources() call at the top of this function.

                // 5-T3: defer-release replaces the pre-flush gate. No
                // need to drain the open batch — the old scratch image
                // is adopted into the scheduler's retire flow so it
                // survives any in-flight CB.
                //
                // CRITICAL borrow-checker note (codex P1): the scratch's
                // &mut borrow MUST end BEFORE
                // `self.scheduler.defer_resource_release` borrows
                // `&mut self`. Use a tight block so the `as_mut()`
                // binding drops at the closing brace. Reborrow
                // `self.copy_scratch` later (for the recorder closure)
                // as a fresh borrow — that's fine because the earlier
                // &mut already ended.
                let retired = {
                    let Some(scratch) = self.copy_scratch.as_mut() else {
                        return false;
                    };
                    match scratch.ensure_size_returning_old(u32::from(width), u32::from(height)) {
                        Ok(r) => r,
                        Err(e) => {
                            log::warn!("vk copy: scratch resize failed: {e:?}");
                            return false;
                        }
                    }
                }; // <-- scratch's &mut borrow ends here.
                if let Some(old) = retired {
                    self.scheduler
                        .defer_resource_release(vk_arc.clone(), pool_handle, old);
                }

                // Resolve the mirror; we need a single &mut to it.
                let Some(mirror) = self
                    .windows
                    .get_mut(&src_xid)
                    .and_then(|w| w.vk_mirror.as_mut())
                    .or_else(|| {
                        self.pixmaps
                            .get_mut(&src_xid)
                            .and_then(|p| p.vk_mirror.as_mut())
                    })
                else {
                    return false;
                };
                let regions = build_image_copy_regions(
                    &sub_rects,
                    src_x,
                    src_y,
                    dst_x,
                    dst_y,
                    mirror.extent,
                    mirror.extent,
                );
                if regions.is_empty() {
                    return true;
                }

                // Re-borrow scratch for the recorder closure. The
                // resize above (if any) already happened; here we
                // just hand the live `&mut CopyScratch` to the
                // record_copy_area_same_overlap helper.
                let Some(scratch) = self.copy_scratch.as_mut() else {
                    return false;
                };
                let bbox_origin = (i32::from(src_x), i32::from(src_y));

                let result =
                    self.scheduler
                        .record_paint_batch_op(vk_arc, pool_handle, |vk, _batch, cb| {
                            copy::record_copy_area_same_overlap(
                                vk,
                                cb,
                                mirror,
                                scratch,
                                &regions,
                                bbox_origin,
                            )
                        });
                return match result {
                    Ok(()) => true,
                    Err(e) => {
                        log::warn!(
                            "vk copy: same-image overlap record failed on xid {src_xid:#x}: \
                             {e:?}"
                        );
                        false
                    }
                };
            }

            // Non-overlapping same-image copy: resolve mirror, then append
            // to PaintBatch. mirror is borrowed from self.windows/pixmaps;
            // self.scheduler is disjoint, so the borrow split works.
            let Some(mirror) = self
                .windows
                .get_mut(&src_xid)
                .and_then(|w| w.vk_mirror.as_mut())
                .or_else(|| {
                    self.pixmaps
                        .get_mut(&src_xid)
                        .and_then(|p| p.vk_mirror.as_mut())
                })
            else {
                return false;
            };
            let regions = build_image_copy_regions(
                &sub_rects,
                src_x,
                src_y,
                dst_x,
                dst_y,
                mirror.extent,
                mirror.extent,
            );
            if regions.is_empty() {
                return true;
            }
            return match self
                .scheduler
                .record_paint_op(vk_arc, pool_handle, |vk, cb| {
                    copy::record_copy_area_same(vk, cb, mirror, &regions)
                }) {
                Ok(()) => true,
                Err(e) => {
                    log::warn!("vk copy: same-image record failed on xid {src_xid:#x}: {e:?}");
                    false
                }
            };
        }

        // Distinct src/dst. Determine which HashMap each lives
        // in. Bail to pixman fallback if either is missing or if
        // their mirrors disagree on format.
        enum Map {
            Window,
            Pixmap,
            None,
        }
        let src_map = if self.windows.contains_key(&src_xid) {
            Map::Window
        } else if self.pixmaps.contains_key(&src_xid) {
            Map::Pixmap
        } else {
            Map::None
        };
        let dst_map = if self.windows.contains_key(&dst_xid) {
            Map::Window
        } else if self.pixmaps.contains_key(&dst_xid) {
            Map::Pixmap
        } else {
            Map::None
        };

        let (src_mirror, dst_mirror): (
            &mut crate::kms::vk::target::DrawableImage,
            &mut crate::kms::vk::target::DrawableImage,
        ) = match (src_map, dst_map) {
            (Map::Window, Map::Window) => {
                let [s_state, d_state] = self.windows.get_disjoint_mut([&src_xid, &dst_xid]);
                let (Some(s), Some(d)) = (s_state, d_state) else {
                    log::debug!(
                        "vk copy diag: W->W lookup failed src={src_xid:#x} dst={dst_xid:#x}"
                    );
                    return false;
                };
                let s_has = s.vk_mirror.is_some();
                let d_has = d.vk_mirror.is_some();
                let (Some(s_m), Some(d_m)) = (s.vk_mirror.as_mut(), d.vk_mirror.as_mut()) else {
                    log::debug!(
                        "vk copy diag: W->W missing mirror src={src_xid:#x}(mirror={s_has}) \
                         dst={dst_xid:#x}(mirror={d_has})"
                    );
                    return false;
                };
                (s_m, d_m)
            }
            (Map::Pixmap, Map::Pixmap) => {
                let [s_state, d_state] = self.pixmaps.get_disjoint_mut([&src_xid, &dst_xid]);
                let (Some(s), Some(d)) = (s_state, d_state) else {
                    log::debug!(
                        "vk copy diag: P->P lookup failed src={src_xid:#x} dst={dst_xid:#x}"
                    );
                    return false;
                };
                let s_has = s.vk_mirror.is_some();
                let d_has = d.vk_mirror.is_some();
                let (Some(s_m), Some(d_m)) = (s.vk_mirror.as_mut(), d.vk_mirror.as_mut()) else {
                    log::debug!(
                        "vk copy diag: P->P missing mirror src={src_xid:#x}(mirror={s_has}) \
                         dst={dst_xid:#x}(mirror={d_has})"
                    );
                    return false;
                };
                (s_m, d_m)
            }
            (Map::Window, Map::Pixmap) => {
                let s_has = self
                    .windows
                    .get(&src_xid)
                    .is_some_and(|w| w.vk_mirror.is_some());
                let d_has = self
                    .pixmaps
                    .get(&dst_xid)
                    .is_some_and(|p| p.vk_mirror.is_some());
                let s = self
                    .windows
                    .get_mut(&src_xid)
                    .and_then(|w| w.vk_mirror.as_mut());
                let d = self
                    .pixmaps
                    .get_mut(&dst_xid)
                    .and_then(|p| p.vk_mirror.as_mut());
                let (Some(s), Some(d)) = (s, d) else {
                    log::debug!(
                        "vk copy diag: W->P missing mirror src_window={src_xid:#x}(mirror={s_has}) \
                         dst_pixmap={dst_xid:#x}(mirror={d_has})"
                    );
                    return false;
                };
                (s, d)
            }
            (Map::Pixmap, Map::Window) => {
                let s_has = self
                    .pixmaps
                    .get(&src_xid)
                    .is_some_and(|p| p.vk_mirror.is_some());
                let d_has = self
                    .windows
                    .get(&dst_xid)
                    .is_some_and(|w| w.vk_mirror.is_some());
                let s = self
                    .pixmaps
                    .get_mut(&src_xid)
                    .and_then(|p| p.vk_mirror.as_mut());
                let d = self
                    .windows
                    .get_mut(&dst_xid)
                    .and_then(|w| w.vk_mirror.as_mut());
                let (Some(s), Some(d)) = (s, d) else {
                    log::debug!(
                        "vk copy diag: P->W missing mirror src_pixmap={src_xid:#x}(mirror={s_has}) \
                         dst_window={dst_xid:#x}(mirror={d_has})"
                    );
                    return false;
                };
                (s, d)
            }
            _ => {
                log::debug!(
                    "vk copy diag: src/dst not registered src={src_xid:#x} dst={dst_xid:#x}"
                );
                return false;
            }
        };

        if src_mirror.format != dst_mirror.format {
            log::debug!(
                "vk copy diag: format mismatch src={src_xid:#x}({:?}) dst={dst_xid:#x}({:?})",
                src_mirror.format,
                dst_mirror.format,
            );
            // vkCmdCopyImage requires matching formats; format
            // conversion needs a shader (out of scope for 4.1.4.2).
            return false;
        }

        let regions = build_image_copy_regions(
            &sub_rects,
            src_x,
            src_y,
            dst_x,
            dst_y,
            src_mirror.extent,
            dst_mirror.extent,
        );
        if regions.is_empty() {
            return true;
        }
        // Distinct-image copy: append to PaintBatch.
        // src_mirror/dst_mirror are borrowed from self.windows/pixmaps;
        // self.scheduler is disjoint, so the borrow split works.
        match self
            .scheduler
            .record_paint_op(vk_arc, pool_handle, |vk, cb| {
                copy::record_copy_area_distinct(vk, cb, src_mirror, dst_mirror, &regions)
            }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk copy: distinct-image record failed (src={src_xid:#x} dst={dst_xid:#x}): \
                     {e:?} — falling back to pixman"
                );
                false
            }
        }
    }

    /// Fill `rects` on `dst_xid`'s mirror in solid `fg` color via
    /// `vk::ops::fill::record_fill_rectangles`. Returns `true` iff
    /// the Vulkan path actually wrote — caller falls back to
    /// pixman on `false`. `fg` is an X11 24-bit `0xRRGGBB` pixel.
    /// Solid-fill `rects` honouring the GC `function`. For `Copy`
    /// (the common case) routes through the existing `cmd_clear_
    /// attachments` fast path. For every other GC function (Xor,
    /// And, Or, Invert, Set, etc.) it draws a quad through the
    /// per-function `LogicFillPipelineCache` so the destination
    /// pixels go through the matching `VkLogicOp`.
    ///
    /// Returns `true` iff the operation took. Mirror-missing /
    /// pipeline-cache-missing return `false` (op silently dropped —
    /// the mid-port directive forbids a pixman fallback).
    fn try_vk_fill_with_function(
        &mut self,
        dst_xid: u32,
        function: GcFunction,
        fg: u32,
        rects: &[Rectangle16],
    ) -> bool {
        if rects.is_empty() {
            return true;
        }
        if matches!(function, GcFunction::Copy) {
            return self.try_vk_solid_fill(dst_xid, fg, rects);
        }
        if matches!(function, GcFunction::NoOp) {
            return true;
        }

        use crate::kms::vk::ops::fill;

        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            return false;
        };

        // Resolve depth eagerly so the pipeline cache key reflects
        // the destination's α policy. Drop the mirror borrow here so
        // the subsequent `logic_fill_pipelines.as_mut()` doesn't
        // conflict with `self.windows` / `self.pixmaps`.
        let depth = if let Some(w) = self.windows.get(&dst_xid) {
            w.depth
        } else if let Some(p) = self.pixmaps.get(&dst_xid) {
            p.depth
        } else {
            0
        };
        let opaque_alpha = depth != 32;

        let pipeline = match self.logic_fill_pipelines.as_mut() {
            Some(cache) => match cache.get(function, opaque_alpha) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("vk logic-fill: pipeline build failed for {function:?}: {e:?}");
                    return false;
                }
            },
            None => return false,
        };
        let pipeline_layout = self
            .logic_fill_pipelines
            .as_ref()
            .expect("checked above")
            .pipeline_layout();

        let mirror = if let Some(w) = self.windows.get_mut(&dst_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&dst_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(mirror) = mirror else {
            return false;
        };

        let extent_w = mirror.extent.width;
        let extent_h = mirror.extent.height;
        // Same R8_UNORM / BGRA8 distinction as `try_vk_solid_fill` —
        // depth-1 / depth-8 mirrors need fg in color[0].
        let color = if mirror.format == ash::vk::Format::R8_UNORM {
            [(fg & 0xFF) as f32 / 255.0, 0.0, 0.0, 1.0]
        } else {
            [
                ((fg >> 16) & 0xFF) as f32 / 255.0,
                ((fg >> 8) & 0xFF) as f32 / 255.0,
                (fg & 0xFF) as f32 / 255.0,
                1.0,
            ]
        };

        let vk_rects: Vec<ash::vk::Rect2D> = rects
            .iter()
            .filter_map(|r| {
                let x0 = i32::from(r.x).max(0);
                let y0 = i32::from(r.y).max(0);
                let x1 = (i32::from(r.x).saturating_add(i32::from(r.width))).min(extent_w as i32);
                let y1 = (i32::from(r.y).saturating_add(i32::from(r.height))).min(extent_h as i32);
                if x1 <= x0 || y1 <= y0 {
                    return None;
                }
                Some(ash::vk::Rect2D {
                    offset: ash::vk::Offset2D { x: x0, y: y0 },
                    extent: ash::vk::Extent2D {
                        width: (x1 - x0) as u32,
                        height: (y1 - y0) as u32,
                    },
                })
            })
            .collect();
        if vk_rects.is_empty() {
            return true;
        }
        let scissor = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: mirror.extent,
        };

        match self
            .scheduler
            .record_paint_op(vk_arc, pool_handle, |vk, cb| {
                fill::record_logic_fill(
                    vk,
                    cb,
                    mirror,
                    pipeline,
                    pipeline_layout,
                    color,
                    &vk_rects,
                    scissor,
                )
            }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk logic-fill: record failed on xid {dst_xid:#x} ({function:?}): {e:?}"
                );
                false
            }
        }
    }

    fn try_vk_solid_fill(&mut self, dst_xid: u32, fg: u32, rects: &[Rectangle16]) -> bool {
        use crate::kms::vk::ops::fill;
        if rects.is_empty() {
            return false;
        }
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            return false;
        };

        // Resolve the destination depth before the mut-borrow of the
        // mirror. L1 task A.3 needs it to decide the α policy:
        //   depth == 32 → preserve the client's `fg` α byte (ARGB
        //                 visual, alpha_mask = 0xff00_0000),
        //   else        → force α = 1.0 (server-owned α; the
        //                 composite scene's pass-through reveals the
        //                 painted pixels as opaque).
        // Pull the mirror reference. Returns &mut DrawableImage.
        let (depth, mirror) = if let Some(w) = self.windows.get_mut(&dst_xid) {
            (w.depth, w.vk_mirror.as_mut())
        } else if let Some(p) = self.pixmaps.get_mut(&dst_xid) {
            (p.depth, p.vk_mirror.as_mut())
        } else {
            (0, None)
        };
        let Some(mirror) = mirror else {
            return false;
        };

        let extent_w = mirror.extent.width;
        let extent_h = mirror.extent.height;
        // Vulkan's vkCmdClearAttachments interprets the [f32; 4] in
        // RGBA order regardless of the image's swizzle, so for
        // BGRA8_UNORM mirrors we unpack X11's 0xRRGGBB into [R,G,B,A].
        // For R8_UNORM mirrors (depth-1 shape masks, depth-8 alpha
        // masks) only color[0] is used, and the foreground is a
        // single byte — putting the RGB-unpacked R into color[0]
        // would leave depth-1 bit-1 fills as byte=0 (since fg=1 has
        // no bits in 16..23). xeyes's PolyFillArc onto its shape
        // mask hit exactly this trap.
        let alpha = if depth == 32 {
            ((fg >> 24) & 0xFF) as f32 / 255.0
        } else {
            1.0
        };
        let color = if mirror.format == ash::vk::Format::R8_UNORM {
            [(fg & 0xFF) as f32 / 255.0, 0.0, 0.0, alpha]
        } else {
            [
                ((fg >> 16) & 0xFF) as f32 / 255.0,
                ((fg >> 8) & 0xFF) as f32 / 255.0,
                (fg & 0xFF) as f32 / 255.0,
                alpha,
            ]
        };

        // Convert request rects to vk::Rect2D, clamping to the
        // mirror extent so cmd_clear_attachments doesn't see
        // negative offsets or sizes that exceed the render area.
        let vk_rects: Vec<ash::vk::Rect2D> = rects
            .iter()
            .filter_map(|r| {
                let x0 = i32::from(r.x).max(0);
                let y0 = i32::from(r.y).max(0);
                let x1 = (i32::from(r.x).saturating_add(i32::from(r.width))).min(extent_w as i32);
                let y1 = (i32::from(r.y).saturating_add(i32::from(r.height))).min(extent_h as i32);
                if x1 <= x0 || y1 <= y0 {
                    return None;
                }
                Some(ash::vk::Rect2D {
                    offset: ash::vk::Offset2D { x: x0, y: y0 },
                    extent: ash::vk::Extent2D {
                        width: (x1 - x0) as u32,
                        height: (y1 - y0) as u32,
                    },
                })
            })
            .collect();
        if vk_rects.is_empty() {
            return false;
        }

        let scissor = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: mirror.extent,
        };

        match self
            .scheduler
            .record_paint_op(vk_arc, pool_handle, |vk, cb| {
                fill::record_fill_rectangles(vk, cb, mirror, color, &vk_rects, scissor)
            }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk fill: record_fill_rectangles failed on xid {dst_xid:#x}: {e:?} — \
                     falling back to pixman this op"
                );
                false
            }
        }
    }

    /// PutImage Vulkan-direct path (sub-phase 4.1.4.3). Memcpy the
    /// X11-format pixel data into the host-visible staging buffer
    /// (with the same byte permutation the pixman path applies — see
    /// `put_image` depth-24/32 arm), record one
    /// `vkCmdCopyBufferToImage` per surviving GC-clipped sub-rect,
    /// submit + wait. Returns `false` (so caller falls back to pixman)
    /// when:
    ///
    /// - Vulkan or the staging buffer / op pool didn't come up.
    /// - The drawable has no `vk_mirror` (window/pixmap not GPU-backed).
    /// - The depth doesn't match the mirror's bytes-per-pixel
    ///   (e.g. depth-1 PutImage into an R8 mirror — bit-packing
    ///   differs; pixman stays correct for those).
    /// - A Vulkan API call fails mid-record.
    fn try_vk_put_image(
        &mut self,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> bool {
        if width == 0 || height == 0 {
            return false;
        }

        // Map X11 source depth to mirror bytes-per-pixel. depth-1
        // unpacks 1 bit/pixel from the wire to 1 byte/pixel R8 in
        // the staging buffer. depth-4 / depth-15 / depth-16 are
        // not in scope today (no matching mirror format).
        let src_bpp: usize = match depth {
            1 | 8 => 1,
            24 | 32 => 4,
            _ => return false,
        };

        // GC clip is in dst space. MIT-SHM PutImage clears the clip
        // before calling backend.put_image, so this returns the input
        // unchanged for that path; core PutImage applies the GC's
        // clip rectangles.
        let dst_rect = Rectangle16 {
            x: dst_x,
            y: dst_y,
            width,
            height,
        };
        let sub_rects = self.intersect_with_current_clip(&[dst_rect]);
        if sub_rects.is_empty() {
            // Fully clipped — nothing to draw, but the request is
            // "handled" so don't fall back to pixman.
            return true;
        }

        // Borrow the mirror to read its extent + format.
        let (mirror_w, mirror_h, mirror_bpp) = {
            let mirror = if let Some(w) = self.windows.get(&host_xid) {
                w.vk_mirror.as_ref()
            } else if let Some(p) = self.pixmaps.get(&host_xid) {
                p.vk_mirror.as_ref()
            } else {
                None
            };
            let Some(mirror) = mirror else {
                return false;
            };
            (
                mirror.extent.width,
                mirror.extent.height,
                mirror.bytes_per_pixel() as usize,
            )
        };
        if mirror_bpp != src_bpp {
            return false;
        }

        // Source row stride in `data` (X11 ZPixmap on the wire).
        //   depth-1: bits packed MSB-first per byte, scanline padded
        //            to 32 bits → `((w + 31) / 32) * 4` bytes.
        //   depth-8: 1 byte/pixel, scanline padded to 32 bits.
        //   depth-24/32: 4 bytes/pixel, no extra row pad.
        let src_row_stride: usize = match depth {
            1 => width.div_ceil(32) as usize * 4,
            8 => (width as usize + 3) & !3,
            24 | 32 => width as usize * 4,
            _ => unreachable!(),
        };

        // Per-sub-rect plan: clip each sub-rect to mirror extent and
        // to the original PutImage rect (so pixels outside the source
        // are skipped), compute staging offset + image offset.
        let mirror_w_i = i32::try_from(mirror_w).unwrap_or(i32::MAX);
        let mirror_h_i = i32::try_from(mirror_h).unwrap_or(i32::MAX);
        let orig_x0 = i32::from(dst_x);
        let orig_y0 = i32::from(dst_y);
        let orig_x1 = orig_x0 + i32::from(width);
        let orig_y1 = orig_y0 + i32::from(height);

        struct PutPlan {
            staging_offset: u64,
            image_x: i32,
            image_y: i32,
            extent_w: u32,
            extent_h: u32,
            src_x: u32,
            src_y: u32,
        }

        let mut plans: Vec<PutPlan> = Vec::with_capacity(sub_rects.len());
        let mut total_bytes: u64 = 0;
        for sub in &sub_rects {
            let sub_x0 = i32::from(sub.x);
            let sub_y0 = i32::from(sub.y);
            let sub_x1 = sub_x0.saturating_add(i32::from(sub.width));
            let sub_y1 = sub_y0.saturating_add(i32::from(sub.height));

            let x0 = sub_x0.max(orig_x0).max(0);
            let y0 = sub_y0.max(orig_y0).max(0);
            let x1 = sub_x1.min(orig_x1).min(mirror_w_i);
            let y1 = sub_y1.min(orig_y1).min(mirror_h_i);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let w = (x1 - x0) as u32;
            let h = (y1 - y0) as u32;
            let bytes = u64::from(w) * u64::from(h) * src_bpp as u64;
            plans.push(PutPlan {
                staging_offset: total_bytes,
                image_x: x0,
                image_y: y0,
                extent_w: w,
                extent_h: h,
                src_x: (x0 - orig_x0) as u32,
                src_y: (y0 - orig_y0) as u32,
            });
            total_bytes += bytes;
        }
        if plans.is_empty() {
            return true;
        }

        // Acquire batch resources (gated by renderer_failed).
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            return false;
        };

        // Re-borrow the mirror mutably for the recording. The borrow
        // is held across `self.scheduler.record_paint_batch_op` — that
        // mutates `self.scheduler` only, disjoint from
        // `self.windows`/`self.pixmaps`.
        let mirror = if let Some(w) = self.windows.get_mut(&host_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&host_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(mirror) = mirror else {
            return false;
        };

        let mut arena_oom = false;
        let result = self
            .scheduler
            .record_paint_batch_op(vk_arc, pool_handle, |vk, batch, cb| {
                // Per-batch staging allocation (replaces OpsStaging).
                let alloc = match batch.upload_arena_mut().alloc(total_bytes, 16) {
                    Ok(a) => a,
                    Err(e) => {
                        // Arena alloc failed BEFORE we recorded anything into the
                        // batch CB. Don't poison the batch — that would drop
                        // unrelated fill/copy work landed by earlier handlers.
                        // Signal failure via the outer flag instead; the closure
                        // returns Ok(()) so record_paint_batch_op leaves the
                        // batch state unchanged.
                        log::warn!(
                            "vk put_image: arena alloc {total_bytes} bytes failed: {e:?} — \
                             falling back to pixman without poisoning batch"
                        );
                        arena_oom = true;
                        return Ok(());
                    }
                };

                // Host → staging memcpy. For depth-24/32 the X11
                // wire is ZPixmap in the visual's byte order: with our
                // advertised masks (R=0x00FF0000, G=0x0000FF00,
                // B=0x000000FF) and an LE client, a 32-bit pixel
                // `(A<<24)|(R<<16)|(G<<8)|B` is written LE → memory
                // bytes `[B, G, R, A]`. That already matches
                // `B8G8R8A8_UNORM`'s memory order, so depth-32 is a
                // straight memcpy. depth-24 has the same byte layout
                // on the wire but the 4th byte is undefined padding;
                // overwrite it with 0xFF so the mirror reads opaque
                // for RENDER composites. For depth-8 it's a per-byte
                // copy; mirror is R8, same byte-per-pixel layout.
                let staging_base = alloc.mapped_ptr.as_ptr();
                for plan in &plans {
                    let row_dst_bytes = plan.extent_w as usize * src_bpp;
                    for row in 0..plan.extent_h {
                        let host_row = (plan.src_y + row) as usize;
                        let src_row_byte_start = host_row * src_row_stride;
                        if src_row_byte_start + src_row_stride > data.len() {
                            // Truncated source — zero-fill the staging row.
                            unsafe {
                                let dst = staging_base.add(
                                    plan.staging_offset as usize + row as usize * row_dst_bytes,
                                );
                                std::ptr::write_bytes(dst, 0, row_dst_bytes);
                            }
                            continue;
                        }
                        unsafe {
                            let dst_row = staging_base
                                .add(plan.staging_offset as usize + row as usize * row_dst_bytes);
                            let src_row = data.as_ptr().add(src_row_byte_start);
                            match depth {
                                1 => {
                                    // X11 ZPixmap depth-1: bits packed
                                    // per the server's advertised
                                    // bitmap_format_bit_order (LSBFirst);
                                    // scanlines padded to 32 bits.
                                    // Unpack each bit into a byte
                                    // (0xFF / 0x00) for the R8 mirror.
                                    for col in 0..plan.extent_w as usize {
                                        let bit_index = plan.src_x as usize + col;
                                        let byte = *src_row.add(bit_index >> 3);
                                        let bit = (byte >> (bit_index & 7)) & 1;
                                        *dst_row.add(col) = if bit != 0 { 0xFF } else { 0x00 };
                                    }
                                }
                                8 => {
                                    let src = src_row.add(plan.src_x as usize);
                                    std::ptr::copy_nonoverlapping(src, dst_row, row_dst_bytes);
                                }
                                24 | 32 => {
                                    // Wire bytes [B, G, R, A] already
                                    // match B8G8R8A8_UNORM. depth-32:
                                    // straight memcpy. depth-24: copy
                                    // then stamp byte[3] = 0xFF.
                                    let src = src_row.add(plan.src_x as usize * 4);
                                    std::ptr::copy_nonoverlapping(src, dst_row, row_dst_bytes);
                                    if depth == 24 {
                                        for col in 0..plan.extent_w as usize {
                                            *dst_row.add(col * 4 + 3) = 0xFFu8;
                                        }
                                    }
                                }
                                _ => unreachable!(),
                            }
                        }
                    }
                }

                // Build BufferImageCopy regions with alloc-relative offsets.
                let regions: Vec<ash::vk::BufferImageCopy> = plans
                    .iter()
                    .map(|p| {
                        ash::vk::BufferImageCopy::default()
                            .buffer_offset(alloc.offset + p.staging_offset)
                            .buffer_row_length(0)
                            .buffer_image_height(0)
                            .image_subresource(
                                ash::vk::ImageSubresourceLayers::default()
                                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                                    .layer_count(1),
                            )
                            .image_offset(ash::vk::Offset3D {
                                x: p.image_x,
                                y: p.image_y,
                                z: 0,
                            })
                            .image_extent(ash::vk::Extent3D {
                                width: p.extent_w,
                                height: p.extent_h,
                                depth: 1,
                            })
                    })
                    .collect();

                crate::kms::vk::ops::image::record_put_image(vk, cb, mirror, alloc.buffer, &regions)
            });

        if arena_oom {
            return false;
        }
        match result {
            Ok(()) => {
                // The Vk-direct write made the mirror current; we do
                // NOT mark damage here. Damage tells `MirrorUploader`
                // to upload pixman → mirror at the next composite
                // frame, which would *clobber* the bytes we just
                // wrote with whatever (stale) pixman contents.
                true
            }
            Err(e) => {
                log::warn!(
                    "vk put_image: record failed on xid {host_xid:#x}: {e:?} — \
                     falling back to pixman"
                );
                false
            }
        }
    }

    // -----------------------------------------------------------------------
    // CPU-visible / sync-export request handler flush-audit (Phase 3A T5)
    //
    // Catalogued 2026-05-13. Every site that needs GPU work to be complete
    // before CPU reads pixels or before a sync object is exported must call
    // `flush_if_needed(Readback | ExternalSync)` before its per-op
    // `run_one_shot_op`. In Phase 3A the batch is always Idle on these paths
    // (no recorder has been migrated yet), so these flushes are no-ops;
    // they become load-bearing in 3B/3C/3D when recorders start populating
    // the batch.
    //
    // grep basis:
    //   rg -n 'record_get_image|read_mirror_pixels|hw_cursor_refresh' kms/backend.rs
    //   rg -n 'PresentPixmap|present_pixmap|dri3|SyncTriggerFence|sync_trigger' src/
    //
    // Site decisions:
    //
    //   try_vk_get_image_pixels (line ~3855):
    //     FLUSH Readback — calls run_one_shot_op(record_get_image). CPU reads
    //     staging after the op. flush_if_needed(Readback) added before the
    //     has_read / run_one_shot_op block.  This function returns `bool`; on
    //     Err, return `false` (existing fallback path, no change in
    //     behaviour).
    //
    //   hw_cursor_refresh (line ~1999):
    //     FLUSH Readback — calls run_one_shot_op(record_get_image) to read
    //     the cursor mirror into the dumb-buffer for the HW cursor plane.
    //     CPU reads staging after the op. flush_if_needed(Readback) added at
    //     the top of the GPU-readback block, before run_one_shot_op.  On Err,
    //     return early (matches existing warn-and-return pattern).
    //
    //   read_mirror_pixels (line ~2331):
    //     FLUSH Readback — calls run_one_shot_op(record_get_image) and then
    //     copies staging bytes to host Vec.  flush_if_needed(Readback) added
    //     before run_one_shot_op. Returns `Option`; on Err, return None
    //     (matches existing warn-and-return pattern).
    //
    //   create_cursor (line ~8601) calls read_mirror_pixels:
    //     NOT a separate flush site — read_mirror_pixels already carries the
    //     flush.
    //
    //   copy_plane (line ~9078) calls read_mirror_pixels:
    //     NOT a separate flush site — read_mirror_pixels already carries the
    //     flush.
    //
    //   read_depth1_pixmap (line ~9210) calls read_mirror_pixels:
    //     NOT a separate flush site — read_mirror_pixels already carries the
    //     flush.
    //
    //   dri3_trigger_fence (line ~7674):
    //     SKIP ExternalSync — current impl only signals an xshmfence (futex
    //     write) or is a no-op for VkSemaphore-backed fences. No GPU work is
    //     involved and no batch dependency exists; adding a flush here would
    //     be premature. Revisit in Phase 3B when a real Present fence
    //     pipeline is wired.
    //
    //   dri3_fd_from_fence (line ~7687):
    //     SKIP ExternalSync — exports an already-created VkSemaphore fd.
    //     No GPU work runs here; the semaphore is pre-populated on import.
    //
    //   dri3_signal_syncobj (line ~7724):
    //     SKIP ExternalSync — signals a timeline semaphore that was
    //     imported from a client DRM syncobj. No paint batch work is
    //     involved.
    //
    //   dri3_export_pixmap (line ~7737):
    //     SKIP ExternalSync — exports a dma-buf fd from an imported
    //     DrawableImage. No GPU work runs here.
    //
    //   PresentPixmap / present_pixmap / SyncTriggerFence:
    //     Not found as handler entry points in kms/backend.rs. The DRI3
    //     Present protocol is dispatched via dri3_trigger_fence /
    //     dri3_signal_syncobj above; no separate PresentPixmap handler exists.
    // -----------------------------------------------------------------------

    /// GetImage Vulkan-direct readback (sub-phase 4.1.4.3). Records
    /// `vkCmdCopyImageToBuffer` from the mirror into the host-visible
    /// staging buffer, waits, and writes the pixel bytes (with the
    /// same byte ordering the pixman path emits) into the caller's
    /// reply Vec. The 32-byte X11 GetImage reply header is emitted by
    /// the trait impl; this returns just the pixel payload.
    ///
    /// Returns `None` when the path is unavailable (no Vulkan, no
    /// staging, no mirror, depth/format mismatch). Caller falls back
    /// to the pixman read in that case.
    fn try_vk_get_image_pixels(
        &mut self,
        host_xid: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        depth: u8,
        out: &mut Vec<u8>,
    ) -> bool {
        use crate::kms::vk::ops::{image as vk_image, run_one_shot_op};

        if width == 0 || height == 0 {
            return false;
        }

        // Match the depth → mirror format mapping from put_image. For
        // the depths we don't accelerate (1, 4, 15, 16) the trait
        // impl's pixman zero-fill path is preserved.
        let bpp: usize = match depth {
            8 => 1,
            24 | 32 => 4,
            _ => return false,
        };

        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return false;
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return false;
        };
        if self.ops_staging.is_none() {
            return false;
        }

        // Ensure any pending paint batch work is flushed before we read
        // pixels back to the CPU. In Phase 3A the batch is always Idle so
        // this is a no-op; it becomes load-bearing once 3B migrates recorders.
        if let Err(e) =
            self.flush_if_needed(crate::kms::scheduler::paint_batch::BatchFlushReason::Readback)
        {
            log::warn!("vk get_image: pre-flush failed ({e:?}); returning zeros");
            return false;
        }

        let (mirror_w, mirror_h, mirror_bpp) = {
            let mirror = if let Some(w) = self.windows.get(&host_xid) {
                w.vk_mirror.as_ref()
            } else if let Some(p) = self.pixmaps.get(&host_xid) {
                p.vk_mirror.as_ref()
            } else {
                None
            };
            let Some(mirror) = mirror else {
                return false;
            };
            (
                mirror.extent.width,
                mirror.extent.height,
                mirror.bytes_per_pixel() as usize,
            )
        };
        if mirror_bpp != bpp {
            return false;
        }

        // Reply rows are padded to a 4-byte boundary per X11 ZPixmap.
        let row_data_bytes = width as usize * bpp;
        let row_stride_bytes = (row_data_bytes + 3) & !3;
        let row_pad_bytes = row_stride_bytes - row_data_bytes;

        // Clip the read rect to the mirror extent. Out-of-range cells
        // are zero-filled in the output (matching the pixman path).
        let mirror_w_i = i32::try_from(mirror_w).unwrap_or(i32::MAX);
        let mirror_h_i = i32::try_from(mirror_h).unwrap_or(i32::MAX);
        let orig_x0 = i32::from(x);
        let orig_y0 = i32::from(y);
        let orig_x1 = orig_x0 + i32::from(width);
        let orig_y1 = orig_y0 + i32::from(height);
        let read_x0 = orig_x0.max(0);
        let read_y0 = orig_y0.max(0);
        let read_x1 = orig_x1.min(mirror_w_i);
        let read_y1 = orig_y1.min(mirror_h_i);
        let has_read = read_x1 > read_x0 && read_y1 > read_y0;

        let read_w = if has_read {
            (read_x1 - read_x0) as u32
        } else {
            0
        };
        let read_h = if has_read {
            (read_y1 - read_y0) as u32
        } else {
            0
        };
        let read_bytes = u64::from(read_w) * u64::from(read_h) * bpp as u64;

        if has_read {
            if let Err(e) = self
                .ops_staging
                .as_mut()
                .expect("checked above")
                .ensure(read_bytes)
            {
                log::warn!(
                    "vk get_image: staging grow failed for {read_bytes} bytes: \
                     {e:?} — falling back to pixman"
                );
                return false;
            }

            let regions = [ash::vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(
                    ash::vk::ImageSubresourceLayers::default()
                        .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_offset(ash::vk::Offset3D {
                    x: read_x0,
                    y: read_y0,
                    z: 0,
                })
                .image_extent(ash::vk::Extent3D {
                    width: read_w,
                    height: read_h,
                    depth: 1,
                })];

            let mirror = if let Some(w) = self.windows.get_mut(&host_xid) {
                w.vk_mirror.as_mut()
            } else if let Some(p) = self.pixmaps.get_mut(&host_xid) {
                p.vk_mirror.as_mut()
            } else {
                None
            };
            let Some(mirror) = mirror else {
                return false;
            };
            let staging_buffer = self.ops_staging.as_ref().expect("checked above").buffer();
            match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
                vk_image::record_get_image(vk, cb, mirror, staging_buffer, &regions)
            }) {
                Ok(()) => {}
                Err(e) => {
                    log::error!(
                        "try_vk_get_image_pixels: run_one_shot_op returned fatal {e:?} \
                         on xid {host_xid:#x}; latching renderer_failed — KMS renderer \
                         disabled until restart"
                    );
                    self.renderer_failed = true;
                    return false;
                }
            }
        }

        // Walk the requested rect row by row, copying from the staging
        // buffer where the row/column intersects the mirror, and
        // emitting zeros where it doesn't. Mirror's BGRA8 memory bytes
        // are the same as pixman's `0xAARRGGBB` LE u32 storage, so a
        // straight memcpy yields identical wire bytes for depth-24/32.
        // Depth-8 is single-byte memcpy.
        let staging_ptr = self.ops_staging.as_ref().unwrap().mapped_ptr() as *const u8;
        for row in 0..i32::from(height) {
            let dy = orig_y0 + row;
            for col in 0..i32::from(width) {
                let dx = orig_x0 + col;
                if has_read && dx >= read_x0 && dx < read_x1 && dy >= read_y0 && dy < read_y1 {
                    let staging_row = (dy - read_y0) as usize;
                    let staging_col = (dx - read_x0) as usize;
                    let staging_off = staging_row * (read_w as usize * bpp) + staging_col * bpp;
                    unsafe {
                        let src = staging_ptr.add(staging_off);
                        let mut buf = [0u8; 4];
                        std::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), bpp);
                        out.extend_from_slice(&buf[..bpp]);
                    }
                } else {
                    out.extend(std::iter::repeat_n(0u8, bpp));
                }
            }
            if row_pad_bytes > 0 {
                out.extend(std::iter::repeat_n(0u8, row_pad_bytes));
            }
        }

        true
    }

    /// Sub-phase 4.1.4.5 helper. Intern each rendered glyph in the
    /// shared atlas and dispatch a single text-pipeline draw onto
    /// the target's mirror. Returns `true` when the path took (the
    /// caller should `return Ok(())`); `false` means the caller
    /// falls back to the existing pixman compositing path.
    ///
    /// All-or-nothing: any glyph that fails to intern (atlas full,
    /// API failure) makes the whole run fall back. Partial Vulkan
    /// and partial pixman would double-draw glyphs; the caller's
    /// pixman path already handles the whole run correctly.
    fn try_vk_text_run(
        &mut self,
        host_xid: u32,
        font_xid: u32,
        foreground: u32,
        rendered: &[RenderedGlyph],
    ) -> bool {
        use crate::kms::vk::{glyph::GlyphKey, ops::text as vk_text};

        if rendered.is_empty() {
            return true;
        }

        // 3E: acquire batch resources up-front (gated by
        // renderer_failed). Atlas upload (intern) and the recorder
        // BOTH route through this — single source for vk_arc /
        // pool_handle removes the duplicate binding that pre-3E
        // had (one for intern's pool, one for run_one_shot_op's
        // submit).
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            return false;
        };
        if self.glyph_atlas.is_none() || self.text_pipeline.is_none() {
            return false;
        }

        // Mirror format check before spending atlas slots on a
        // run that can't use them.
        let mirror_format = if let Some(w) = self.windows.get(&host_xid) {
            w.vk_mirror.as_ref().map(|m| m.format)
        } else if let Some(p) = self.pixmaps.get(&host_xid) {
            p.vk_mirror.as_ref().map(|m| m.format)
        } else {
            None
        };
        let Some(mirror_format) = mirror_format else {
            return false;
        };
        if mirror_format != ash::vk::Format::B8G8R8A8_UNORM {
            return false;
        }

        // Intern glyphs first. Each call may run a one-shot
        // upload CB; that's fine — happens before we record the
        // text run, so atlas is fully populated by the time the
        // text-run CB executes.
        let mut glyphs_to_draw: Vec<vk_text::TextGlyph> = Vec::with_capacity(rendered.len());
        for g in rendered {
            let key = GlyphKey {
                font_xid,
                codepoint: g.codepoint,
            };
            let atlas = self.glyph_atlas.as_mut().expect("checked above");
            let Some(entry) =
                atlas.intern(key, g.w as u32, g.h as u32, 0, 0, &g.pixels, pool_handle)
            else {
                // Atlas full or upload failed — abort the run.
                return false;
            };
            glyphs_to_draw.push(vk_text::TextGlyph {
                entry,
                dst_x: g.dst_x,
                dst_y: g.dst_y,
            });
        }

        let foreground_rgba = [
            ((foreground >> 16) & 0xFF) as f32 / 255.0,
            ((foreground >> 8) & 0xFF) as f32 / 255.0,
            (foreground & 0xFF) as f32 / 255.0,
            1.0,
        ];

        // Re-borrow the mirror mutably for the recording. The
        // closure also captures atlas + pipeline (read-only) from
        // disjoint fields. self.scheduler.record_paint_op is the
        // remaining mutable borrow; field-disjoint, so the borrow
        // checker accepts.
        let mirror = if let Some(w) = self.windows.get_mut(&host_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&host_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(mirror) = mirror else {
            return false;
        };
        let atlas = self.glyph_atlas.as_ref().expect("checked above");
        let pipeline = self.text_pipeline.as_ref().expect("checked above");

        match self
            .scheduler
            .record_paint_op(vk_arc, pool_handle, |vk, cb| {
                vk_text::record_text_run(
                    vk,
                    cb,
                    mirror,
                    atlas,
                    pipeline,
                    &glyphs_to_draw,
                    foreground_rgba,
                )
            }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk text_run: record failed on xid {host_xid:#x}: {e:?} — falling back to \
                     pixman"
                );
                false
            }
        }
    }

    /// Phase 4.1.4.7 trapezoid wire-decoder + Vk dispatch. Returns
    /// `true` when the Vulkan path took.
    fn try_vk_render_trapezoids_path(
        &mut self,
        op: u8,
        host_src: u32,
        host_dst: u32,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> bool {
        use crate::kms::vk::ops::traps as vk_traps;

        let n_traps = traps.len() / 40;
        if n_traps == 0 {
            return true;
        }
        let mut decoded: Vec<vk_traps::Trapezoid> = Vec::with_capacity(n_traps);
        for chunk in traps.chunks_exact(40) {
            let read_i32 = |o: usize| -> i32 {
                i32::from_le_bytes([chunk[o], chunk[o + 1], chunk[o + 2], chunk[o + 3]])
            };
            decoded.push(vk_traps::Trapezoid {
                top: read_i32(0),
                bottom: read_i32(4),
                left_p1: (read_i32(8), read_i32(12)),
                left_p2: (read_i32(16), read_i32(20)),
                right_p1: (read_i32(24), read_i32(28)),
                right_p2: (read_i32(32), read_i32(36)),
            });
        }
        // Apply (x_off, y_off) in fixed-point units.
        let dx = i32::from(x_off) << 16;
        let dy = i32::from(y_off) << 16;
        if dx != 0 || dy != 0 {
            for t in &mut decoded {
                t.top = t.top.wrapping_add(dy);
                t.bottom = t.bottom.wrapping_add(dy);
                t.left_p1.0 = t.left_p1.0.wrapping_add(dx);
                t.left_p1.1 = t.left_p1.1.wrapping_add(dy);
                t.left_p2.0 = t.left_p2.0.wrapping_add(dx);
                t.left_p2.1 = t.left_p2.1.wrapping_add(dy);
                t.right_p1.0 = t.right_p1.0.wrapping_add(dx);
                t.right_p1.1 = t.right_p1.1.wrapping_add(dy);
                t.right_p2.0 = t.right_p2.0.wrapping_add(dx);
                t.right_p2.1 = t.right_p2.1.wrapping_add(dy);
            }
        }

        let Some((bx, by, bx1, by1)) = vk_traps::trapezoid_bbox(&decoded) else {
            return true; // empty / degenerate
        };
        // Clamp to non-negative — Vulkan dst-coords are unsigned.
        let bx = bx.max(0);
        let by = by.max(0);
        if bx1 <= bx || by1 <= by {
            return true;
        }
        let bw = (bx1 - bx) as u32;
        let bh = (by1 - by) as u32;
        // gpu-trap T2/T3/T5: CPU rasterize is gone from both arms.
        // `try_vk_render_traps_or_tris` builds the coverage mask on the
        // GPU inside the open paint batch (no synchronous CPU work in
        // the X protocol request handler).
        self.try_vk_render_traps_or_tris(
            op,
            host_src,
            host_dst,
            TrapsOrTris::Traps(&decoded),
            bx,
            by,
            bw,
            bh,
        )
    }

    /// Phase 4.1.4.7 triangle wire-decoder + Vk dispatch. Mirrors
    /// the trapezoid path but for triangle / tristrip / trifan.
    #[allow(clippy::too_many_arguments)]
    fn try_vk_render_triangles_path(
        &mut self,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        primitives: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> bool {
        use crate::kms::vk::ops::traps as vk_traps;

        let read_point = |off: usize, chunk: &[u8]| -> (i32, i32) {
            let x =
                i32::from_le_bytes([chunk[off], chunk[off + 1], chunk[off + 2], chunk[off + 3]]);
            let y = i32::from_le_bytes([
                chunk[off + 4],
                chunk[off + 5],
                chunk[off + 6],
                chunk[off + 7],
            ]);
            (x, y)
        };

        let mut tris: Vec<vk_traps::Triangle> = match minor {
            11 => {
                if !primitives.len().is_multiple_of(24) {
                    return false;
                }
                primitives
                    .chunks_exact(24)
                    .map(|c| vk_traps::Triangle {
                        p1: read_point(0, c),
                        p2: read_point(8, c),
                        p3: read_point(16, c),
                    })
                    .collect()
            }
            12 => {
                if !primitives.len().is_multiple_of(8) || primitives.len() < 24 {
                    return false;
                }
                let pts: Vec<(i32, i32)> = primitives
                    .chunks_exact(8)
                    .map(|c| read_point(0, c))
                    .collect();
                (0..pts.len() - 2)
                    .map(|i| vk_traps::Triangle {
                        p1: pts[i],
                        p2: pts[i + 1],
                        p3: pts[i + 2],
                    })
                    .collect()
            }
            13 => {
                if !primitives.len().is_multiple_of(8) || primitives.len() < 24 {
                    return false;
                }
                let pts: Vec<(i32, i32)> = primitives
                    .chunks_exact(8)
                    .map(|c| read_point(0, c))
                    .collect();
                (1..pts.len() - 1)
                    .map(|i| vk_traps::Triangle {
                        p1: pts[0],
                        p2: pts[i],
                        p3: pts[i + 1],
                    })
                    .collect()
            }
            _ => return false,
        };
        if tris.is_empty() {
            return true;
        }
        let dx = i32::from(x_off) << 16;
        let dy = i32::from(y_off) << 16;
        if dx != 0 || dy != 0 {
            for t in &mut tris {
                t.p1.0 = t.p1.0.wrapping_add(dx);
                t.p1.1 = t.p1.1.wrapping_add(dy);
                t.p2.0 = t.p2.0.wrapping_add(dx);
                t.p2.1 = t.p2.1.wrapping_add(dy);
                t.p3.0 = t.p3.0.wrapping_add(dx);
                t.p3.1 = t.p3.1.wrapping_add(dy);
            }
        }

        let Some((bx, by, bx1, by1)) = vk_traps::triangle_bbox(&tris) else {
            return true;
        };
        let bx = bx.max(0);
        let by = by.max(0);
        if bx1 <= bx || by1 <= by {
            return true;
        }
        let bw = (bx1 - bx) as u32;
        let bh = (by1 - by) as u32;
        // gpu-trap T3: triangles route through the GPU rasterizer
        // via TrapPipeline's sibling `triangle_pipeline`. No
        // synchronous CPU work in the X protocol request handler.
        self.try_vk_render_traps_or_tris(
            op,
            host_src,
            host_dst,
            TrapsOrTris::Tris(&tris),
            bx,
            by,
            bw,
            bh,
        )
    }

    /// Sub-phase 4.1.4.7 helper. Vulkan-direct RENDER `Trapezoids`
    /// / `Triangles`. Both arms build the coverage mask on the GPU
    /// inside the open paint batch via [`TrapPipeline`]: the trap
    /// arm (T2) draws via `trapezoid_pipeline()`, the triangle arm
    /// (T3) draws via `triangle_pipeline()`. The post-rasterize
    /// composite path is shared.
    ///
    /// [`TrapPipeline`]: crate::kms::vk::trap_pipeline::TrapPipeline
    ///
    /// Either way the resulting R8 coverage mask lives in
    /// [`MaskScratch`](crate::kms::vk::mask_scratch::MaskScratch);
    /// the composite path then reads `src.color * mask.alpha` exactly
    /// like a normal Composite call with an A8 mask.
    ///
    /// Source resolution mirrors [`Self::try_vk_render_composite`]:
    /// `SolidFill` clears a 1×1 scratch with the picture colour,
    /// `Drawable` samples the drawable's mirror (with the same
    /// alpha-vs-no-alpha view selection rules), `Gradient` samples
    /// the pre-evaluated gradient image and inherits its
    /// axis-projection transform.
    ///
    /// Returns `true` if the Vulkan path took (or trivially had no
    /// pixels to draw); `false` means the caller should treat the
    /// request as unhandled.
    #[allow(clippy::too_many_arguments)]
    fn try_vk_render_traps_or_tris(
        &mut self,
        op: u8,
        host_src: u32,
        host_dst: u32,
        prims: TrapsOrTris<'_>,
        bbox_x: i32,
        bbox_y: i32,
        bbox_w: u32,
        bbox_h: u32,
    ) -> bool {
        use crate::kms::vk::{
            ops::render as vk_render,
            render_pipeline::{StdPictOp, record_solid_color_clear},
            trap_pipeline::TrapDrawPushConsts,
        };

        if bbox_w == 0 || bbox_h == 0 {
            return true;
        }
        // Primitive list emptiness == no draw.
        match &prims {
            TrapsOrTris::Traps([]) | TrapsOrTris::Tris([]) => return true,
            _ => {}
        }

        let Some(std_op) = StdPictOp::from_u8(op) else {
            return false;
        };

        // gpu-trap T2/T3: both GPU arms require the pipeline to be
        // live. Build a snapshot of pipeline handles outside the
        // closure so the (`&mut self`) → record_paint_batch_op handoff
        // doesn't alias `self.trap_pipeline`. If the pipeline is None
        // (init failed at backend bring-up), the arm declines (caller
        // falls back to pixman). The snapshot's first slot holds the
        // primitive-type-specific pipeline; the layout is shared.
        let trap_pipeline_snapshot = {
            let Some(tp) = self.trap_pipeline.as_ref() else {
                log::debug!(
                    "vk render_traps bail: trap_pipeline missing (init failed?) — \
                     falling back to pixman"
                );
                return false;
            };
            let prim_pipeline = match &prims {
                TrapsOrTris::Traps(_) => tp.trapezoid_pipeline(),
                TrapsOrTris::Tris(_) => tp.triangle_pipeline(),
            };
            (prim_pipeline, tp.pipeline_layout())
        };

        // `resolve_render_pic` returns None for gradients (the variant
        // doesn't carry the picture XID); the trapezoid path must use
        // the gradient-aware resolver. Without this, every mate-CC
        // button-hover RenderTrapezoids — which sends a gradient as
        // its source — declined and we rasterised on CPU for nothing.
        let Some(src) = resolve_render_pic_with_gradient_xid(&self.pictures, host_src) else {
            return false;
        };

        let (dst_xid, picture_clip) = match self.pictures.get(&host_dst) {
            Some(PictureState::Drawable { host_xid, clip, .. }) => (*host_xid, clip.clone()),
            _ => return false,
        };

        // Self-composite (src aliases dst) needs a staging copy —
        // out of scope for this path.
        let src_xid_if_drawable = match src {
            RenderPic::Drawable(xid) => Some(xid),
            _ => None,
        };
        if src_xid_if_drawable == Some(dst_xid) {
            return false;
        }

        // 3F-2: acquire batch resources up-front (gated by
        // renderer_failed). Same shape try_vk_render_composite uses
        // since 3F-1.
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            log::debug!(
                "vk render_traps bail: paint_resources unavailable (renderer_failed or vk/pool absent) (dst=0x{dst_xid:x})"
            );
            return false;
        };
        if self.render_pipelines.is_none()
            || self.solid_src_image.is_none()
            || self.mask_scratch.is_none()
            || self.white_mask_image.is_none()
        {
            return false;
        }
        let (dst_format, dst_extent, dst_depth, drawable_size) =
            if let Some(w) = self.windows.get(&dst_xid) {
                (
                    w.vk_mirror.as_ref().map(|m| m.format),
                    w.vk_mirror.as_ref().map(|m| m.extent),
                    w.depth,
                    Some((u32::from(w.width), u32::from(w.height))),
                )
            } else if let Some(p) = self.pixmaps.get(&dst_xid) {
                (
                    p.vk_mirror.as_ref().map(|m| m.format),
                    p.vk_mirror.as_ref().map(|m| m.extent),
                    p.depth,
                    Some((u32::from(p.width), u32::from(p.height))),
                )
            } else {
                (None, None, 0, None)
            };
        let dst_format = match dst_format {
            Some(f @ (ash::vk::Format::B8G8R8A8_UNORM | ash::vk::Format::R8_UNORM)) => f,
            _ => return false,
        };
        let Some(dst_extent) = dst_extent else {
            return false;
        };
        let Some((drawable_w, drawable_h)) = drawable_size else {
            return false;
        };
        let dst_has_alpha = dst_format == ash::vk::Format::R8_UNORM || dst_depth == 32;
        let needs_dst_readback = std_op.needs_dst_readback();
        if needs_dst_readback && self.dst_readback.is_none() {
            return false;
        }

        // Drawable src must have a sampleable mirror in a known
        // format. Matches the gating in try_vk_render_composite.
        if let Some(xid) = src_xid_if_drawable
            && !self.ensure_drawable_mirror_sampleable(xid)
        {
            return false;
        }
        if let Some(xid) = src_xid_if_drawable {
            let f = if let Some(w) = self.windows.get(&xid) {
                w.vk_mirror.as_ref().map(|m| m.format)
            } else if let Some(p) = self.pixmaps.get(&xid) {
                p.vk_mirror.as_ref().map(|m| m.format)
            } else {
                None
            };
            if !matches!(
                f,
                Some(ash::vk::Format::B8G8R8A8_UNORM | ash::vk::Format::R8_UNORM)
            ) {
                return false;
            }
        }

        // Pipeline + scissor. render_traps doesn't compute via the
        // component-alpha path (its mask is the CPU-rasterised
        // coverage, not a client picture), so component_alpha=false.
        let pipeline = match self.render_pipelines.as_mut().expect("checked above").get(
            std_op,
            dst_format,
            dst_has_alpha,
            false,
        ) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("vk render_traps: pipeline build failed: {e:?}");
                return false;
            }
        };
        let pipeline_layout = self
            .render_pipelines
            .as_ref()
            .expect("checked above")
            .pipeline_layout();

        let solid_src_view = self
            .solid_src_image
            .as_ref()
            .expect("checked above")
            .image_view();

        // Resolve src view + extent + (optional) clear colour.
        // Tracks any per-source affine the picture itself induces
        // (gradient axis projection); the user-transform composition
        // path in try_vk_render_composite isn't reachable here yet
        // because the trapezoid/triangle paths don't carry one.
        let mut src_clear_color: Option<[f32; 4]> = None;
        let src_view;
        let src_extent;
        let src_picture_xform: Option<vk_render::AffineXform>;
        match src {
            RenderPic::Drawable(xid) => {
                let (m_format, extent, depth) = {
                    let (m, depth) = if let Some(w) = self.windows.get(&xid) {
                        (w.vk_mirror.as_ref(), w.depth)
                    } else if let Some(p) = self.pixmaps.get(&xid) {
                        (p.vk_mirror.as_ref(), p.depth)
                    } else {
                        (None, 0)
                    };
                    let Some(m) = m else { return false };
                    (m.format, m.extent, depth)
                };
                let view = if m_format == ash::vk::Format::R8_UNORM {
                    let v = if let Some(w) = self.windows.get_mut(&xid) {
                        w.vk_mirror
                            .as_mut()
                            .map(|m| m.mask_image_view())
                            .transpose()
                    } else if let Some(p) = self.pixmaps.get_mut(&xid) {
                        p.vk_mirror
                            .as_mut()
                            .map(|m| m.mask_image_view())
                            .transpose()
                    } else {
                        Ok(None)
                    };
                    match v {
                        Ok(Some(v)) => v,
                        _ => return false,
                    }
                } else if depth == 24 {
                    let v = if let Some(w) = self.windows.get_mut(&xid) {
                        w.vk_mirror
                            .as_mut()
                            .map(|m| m.no_alpha_src_image_view())
                            .transpose()
                    } else if let Some(p) = self.pixmaps.get_mut(&xid) {
                        p.vk_mirror
                            .as_mut()
                            .map(|m| m.no_alpha_src_image_view())
                            .transpose()
                    } else {
                        Ok(None)
                    };
                    match v {
                        Ok(Some(v)) => v,
                        _ => return false,
                    }
                } else if let Some(w) = self.windows.get(&xid) {
                    w.vk_mirror.as_ref().expect("checked above").vk_image_view
                } else {
                    self.pixmaps
                        .get(&xid)
                        .and_then(|p| p.vk_mirror.as_ref())
                        .expect("checked above")
                        .vk_image_view
                };
                src_view = view;
                src_extent = extent;
                src_picture_xform = None;
            }
            RenderPic::Solid(color) => {
                src_view = solid_src_view;
                src_extent = ash::vk::Extent2D {
                    width: 1,
                    height: 1,
                };
                src_clear_color = Some(color);
                src_picture_xform = None;
            }
            RenderPic::Gradient(xid) => {
                let Some(PictureState::Gradient { gradient, .. }) = self.pictures.get(&xid) else {
                    return false;
                };
                src_view = gradient.image_view();
                src_extent = gradient.extent();
                src_picture_xform = Some(gradient.axis_projection);
            }
            RenderPic::None => return false,
        }

        // 5-T5: defer-release replaces the pre-flush gate. The old
        // mask image is adopted into the scheduler's retire flow so
        // it survives any in-flight CB.
        //
        // CRITICAL borrow-checker note (same pattern as T3/T4): the
        // scratch's &mut borrow MUST end BEFORE
        // `self.scheduler.defer_resource_release` borrows `&mut self`.
        // Use a tight block so the `as_mut()` binding drops at the
        // closing brace. Reborrow `self.mask_scratch` later (as &ref)
        // for view + extent below.
        let retired = {
            let scratch = self.mask_scratch.as_mut().expect("checked above");
            match scratch.ensure_image_size_returning_old(bbox_w, bbox_h) {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("vk render_traps: mask ensure_image_size failed: {e:?}");
                    return false;
                }
            }
        }; // <-- scratch's &mut borrow ends here.
        if let Some(old) = retired {
            self.scheduler
                .defer_resource_release(vk_arc.clone(), pool_handle, old);
        }

        // P1: `mask_view` / `mask_extent` / `mask_image` MUST be read
        // AFTER the grow above — the grow path replaces the underlying
        // `vk::Image` / `vk::ImageView`, so a pre-grow capture would
        // refer to the just-retired handle.
        //
        // gpu-trap T2 borrow-checker note: also snapshot `mask_image`
        // here as an immutable copy. The GPU-rasterize barrier
        // sequence needs the raw image handle; reading it inside the
        // closure would require a second `&` borrow on
        // `mask_scratch`, which conflicts with the `&mut` borrow used
        // for `set_current_layout` at the closure tail.
        let mask_view = self
            .mask_scratch
            .as_ref()
            .expect("checked above")
            .image_view();
        let mask_extent = self.mask_scratch.as_ref().expect("checked above").extent();
        let mask_image = self.mask_scratch.as_ref().expect("checked above").image();

        // Disjoint/Conjoint ops reach this path too (e.g. rendercheck's
        // `Conjoint*` triangles cases). For those the shader reads dst
        // via binding 2; ensure the dst-readback scratch is sized for
        // the dst mirror and snapshot dst into it inside the CB.
        // Standard ops bind the white-mask scratch as a placeholder.
        let white_mask_view = self
            .white_mask_image
            .as_ref()
            .expect("checked above")
            .image_view();
        let dst_readback_view = if needs_dst_readback {
            // 5-T4: defer-release replaces the pre-flush gate. The
            // scratch's &mut borrow MUST end BEFORE
            // `self.scheduler.defer_resource_release` borrows
            // `&mut self`. Use a tight block so the `as_mut()` binding
            // drops at the closing brace, then reborrow for view
            // extraction.
            let retired = {
                let scratch = self.dst_readback.as_mut().expect("checked above");
                match scratch.ensure_returning_old(dst_format, dst_extent.width, dst_extent.height)
                {
                    Ok(r) => r,
                    Err(e) => {
                        log::warn!("vk render_traps: dst readback ensure failed: {e:?}");
                        return false;
                    }
                }
            }; // <-- scratch's &mut borrow ends here.
            if let Some(old) = retired {
                self.scheduler
                    .defer_resource_release(vk_arc.clone(), pool_handle, old);
            }
            // Reborrow self.dst_readback for view extraction now that
            // the earlier &mut and the scheduler borrow have ended.
            let scratch = self.dst_readback.as_mut().expect("checked above");
            match scratch.view(dst_format, dst_has_alpha) {
                Ok(Some(v)) => v,
                Ok(None) => return false,
                Err(e) => {
                    log::warn!("vk render_traps: dst readback view build failed: {e:?}");
                    return false;
                }
            }
        } else {
            white_mask_view
        };

        // Pixman's `zero_src_has_no_effect` table inverted: ops where
        // mask=0 still affects dst (e.g. Clear, Src, In, InReverse,
        // Out, AtopReverse, Saturate, plus every Disjoint/Conjoint
        // variant — pixman's table doesn't list those, so they fall
        // into the default-FALSE arm) must composite across the
        // entire destination, not just the trapezoid bbox. The
        // mask scratch sits at offset (bbox_x, bbox_y) within the
        // destination; outside that window REPEAT_NONE returns 0,
        // and the operator math then yields the right outside-bbox
        // result (Clear → 0, Src → 0, etc.). See xserver
        // `fb/fbtrap.c` → `pixman/pixman-trap.c::get_trap_extents`.
        let needs_full_dst = matches!(op, 0 | 1 | 5 | 6 | 7 | 10 | 13 | 16..=27 | 32..=43);
        let (render_dst_x, render_dst_y, render_w, render_h, mask_off_x, mask_off_y) =
            if needs_full_dst {
                (0, 0, drawable_w, drawable_h, -bbox_x, -bbox_y)
            } else {
                (bbox_x, bbox_y, bbox_w, bbox_h, 0, 0)
            };

        // Build the scissor + composite rect.
        let scissor = match self.build_render_composite_inputs(
            &picture_clip,
            0,
            0,
            0,
            0,
            render_dst_x as i16,
            render_dst_y as i16,
            render_w.try_into().unwrap_or(u16::MAX),
            render_h.try_into().unwrap_or(u16::MAX),
        ) {
            Some((_, scissor)) => scissor,
            None => return true, // clipped out completely
        };
        let rects = vec![vk_render::CompositeRect {
            src_x: 0,
            src_y: 0,
            mask_x: mask_off_x,
            mask_y: mask_off_y,
            dst_x: render_dst_x,
            dst_y: render_dst_y,
            width: render_w,
            height: render_h,
        }];
        let attrs = vk_render::CompositeAttrs {
            src_extent,
            mask_extent,
            // Synthetic 1×1 sources tile trivially; real Drawable /
            // Gradient sources sample within their extent and the
            // bounding box keeps us in-range, so Normal is a safe
            // default.
            src_repeat: 1,
            // Mask scratch is sized exactly to the trapezoid bbox.
            // Sampling outside that window with REPEAT_NONE returns 0,
            // which is what we want for the full-dst path's
            // outside-bbox pixels (mask=0 makes the operator yield
            // the right zero / dst-preserved result).
            mask_repeat: 0,
            src_xform: src_picture_xform.unwrap_or(vk_render::AffineXform::IDENTITY),
            mask_xform: vk_render::AffineXform::IDENTITY,
        };

        let dst_mirror = if let Some(w) = self.windows.get_mut(&dst_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&dst_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(dst_mirror) = dst_mirror else {
            return false;
        };
        let solid_src_image = self.solid_src_image.as_mut().expect("checked above");
        let mask_scratch = self.mask_scratch.as_mut().expect("checked above");
        let dst_readback = if needs_dst_readback {
            Some(self.dst_readback.as_mut().expect("checked above"))
        } else {
            None
        };

        // 3F-2: mask upload + descriptor alloc move into the closure.
        // `render_cache` is a shared borrow on self.render_pipelines —
        // disjoint from &mut self.scheduler and the other &mut field
        // captures.
        //
        // Arena alloc failure uses the outer-flag pattern (3C T2):
        // failure happens BEFORE any CB recording, so a poisoned-batch
        // return would discard unrelated 3B/3C/3E/3F-1 work already
        // recorded in this batch. Set `arena_oom = true`, return Ok,
        // and report failure to the caller after `record_paint_batch_op`
        // returns.
        //
        // gpu-trap T2/T3: both arms build the coverage mask via a
        // `TrapPipeline` draw into MaskScratch (one
        // vkCmdDraw(4, n_primitives), additive blend, R8_UNORM clamps
        // to [0,1] for saturating-add semantics). T2 wired traps; T3
        // wires tris (this commit). The pipeline + instance-data
        // layout differs between the two; the post-rasterize composite
        // path is shared.
        let render_cache = self.render_pipelines.as_ref().expect("checked above");
        let mut arena_oom = false;
        // Snapshot primitive slices so the closure doesn't need to
        // pattern-match the &prims borrow inside.
        let traps_slice: Option<&[crate::kms::vk::ops::traps::Trapezoid]> = match &prims {
            TrapsOrTris::Traps(t) => Some(t),
            TrapsOrTris::Tris(_) => None,
        };
        let tris_slice: Option<&[crate::kms::vk::ops::traps::Triangle]> = match &prims {
            TrapsOrTris::Tris(t) => Some(t),
            TrapsOrTris::Traps(_) => None,
        };
        let result = self
            .scheduler
            .record_paint_batch_op(vk_arc, pool_handle, |vk, batch, cb| {
                // ---- Build coverage mask in MaskScratch (GPU) ----
                //
                // Upload per-instance data into the open batch's
                // arena (40 B × n_traps for traps; 24 B × n_tris for
                // tris), bind it as a vertex buffer, draw one unit
                // quad per instance into MaskScratch with additive
                // blend. The pipeline / vertex layout is picked at
                // pipeline-snapshot time outside the closure.
                use crate::kms::vk::trap_pipeline::{TrapInstanceData, TriangleInstanceData};
                let (prim_pipeline, prim_layout) = trap_pipeline_snapshot;
                let (instance_stride, n_instances, needed): (usize, u32, u64) =
                    if let Some(t) = traps_slice {
                        let stride = std::mem::size_of::<TrapInstanceData>();
                        let n = t.len() as u32;
                        (stride, n, u64::from(n) * (stride as u64))
                    } else {
                        let t = tris_slice.expect("either traps or tris is set");
                        let stride = std::mem::size_of::<TriangleInstanceData>();
                        let n = t.len() as u32;
                        (stride, n, u64::from(n) * (stride as u64))
                    };
                let alloc = match batch.upload_arena_mut().alloc(needed, 4) {
                    Ok(a) => a,
                    Err(e) => {
                        log::warn!(
                            "vk render_traps: arena alloc {needed} bytes (primitive \
                             instances) failed: {e:?} — GPU upload will fail without \
                             poisoning batch"
                        );
                        arena_oom = true;
                        return Ok(());
                    }
                };
                // SAFETY: alloc.mapped_ptr is HOST_VISIBLE |
                // HOST_COHERENT, mapped at alloc.buffer +
                // alloc.offset, valid for `needed` bytes. We write
                // each instance struct (size 40 or 24, no padding —
                // asserted by the const _ in trap_pipeline.rs)
                // sequentially via copy_nonoverlapping. The 4-byte
                // arena alignment matches `f32` alignment.
                let base = alloc.mapped_ptr.as_ptr();
                if let Some(traps) = traps_slice {
                    for (i, t) in traps.iter().enumerate() {
                        let inst = t.to_instance_data();
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                inst.as_bytes().as_ptr(),
                                base.add(i * instance_stride),
                                instance_stride,
                            );
                        }
                    }
                } else {
                    let tris = tris_slice.expect("either traps or tris is set");
                    for (i, t) in tris.iter().enumerate() {
                        let inst = t.to_instance_data();
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                inst.as_bytes().as_ptr(),
                                base.add(i * instance_stride),
                                instance_stride,
                            );
                        }
                    }
                }

                // 1. Barrier MaskScratch <current_layout> →
                //    COLOR_ATTACHMENT_OPTIMAL. Source stage/access
                //    are conditional on the source layout (pre-task
                //    note 6): UNDEFINED → (TOP_OF_PIPE, NONE) — no
                //    prior op to wait on, LOAD_OP_CLEAR discards
                //    contents; SHADER_READ_ONLY_OPTIMAL →
                //    (FRAGMENT_SHADER, SHADER_SAMPLED_READ); else
                //    (ALL_COMMANDS, SHADER_SAMPLED_READ) defensive.
                let src_layout = mask_scratch.current_layout();
                let (src_stage, src_access) = match src_layout {
                    ash::vk::ImageLayout::UNDEFINED => (
                        ash::vk::PipelineStageFlags2::TOP_OF_PIPE,
                        ash::vk::AccessFlags2::NONE,
                    ),
                    ash::vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL => (
                        ash::vk::PipelineStageFlags2::FRAGMENT_SHADER,
                        ash::vk::AccessFlags2::SHADER_SAMPLED_READ,
                    ),
                    _ => (
                        ash::vk::PipelineStageFlags2::ALL_COMMANDS,
                        ash::vk::AccessFlags2::SHADER_SAMPLED_READ,
                    ),
                };
                let color_range = ash::vk::ImageSubresourceRange::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1);
                let to_attach = [ash::vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(src_stage)
                    .src_access_mask(src_access)
                    .dst_stage_mask(ash::vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                    .dst_access_mask(ash::vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
                    .old_layout(src_layout)
                    .new_layout(ash::vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .image(mask_image)
                    .subresource_range(color_range)];
                let dep = ash::vk::DependencyInfo::default().image_memory_barriers(&to_attach);
                crate::vk_count!(cmd_pipeline_barrier2);
                unsafe { vk.device.cmd_pipeline_barrier2(cb, &dep) };

                // 2. cmdBeginRendering with LOAD_OP_CLEAR. The
                //    render area is in MaskScratch-LOCAL coords
                //    starting at (0, 0) — writes the coverage mask
                //    to the top-left of MaskScratch, matching the
                //    pre-gpu-trap CPU-upload convention so the
                //    surrounding composite's mask sampling
                //    (mask_origin = -bbox_x, -bbox_y for full_dst
                //    or 0, 0 otherwise) lands at the right pixels.
                //    The fragment shader translates back to
                //    absolute coords by adding bbox_origin_pixel
                //    for the edge math.
                let bbox_render_area = ash::vk::Rect2D {
                    offset: ash::vk::Offset2D { x: 0, y: 0 },
                    extent: ash::vk::Extent2D {
                        width: bbox_w,
                        height: bbox_h,
                    },
                };
                let clear = ash::vk::ClearValue {
                    color: ash::vk::ClearColorValue {
                        float32: [0.0, 0.0, 0.0, 0.0],
                    },
                };
                let color_attachment = ash::vk::RenderingAttachmentInfo::default()
                    .image_view(mask_view)
                    .image_layout(ash::vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .load_op(ash::vk::AttachmentLoadOp::CLEAR)
                    .store_op(ash::vk::AttachmentStoreOp::STORE)
                    .clear_value(clear);
                let color_attachments = [color_attachment];
                let rendering_info = ash::vk::RenderingInfo::default()
                    .render_area(bbox_render_area)
                    .layer_count(1)
                    .color_attachments(&color_attachments);
                crate::vk_count!(cmd_begin_rendering);
                unsafe { vk.device.cmd_begin_rendering(cb, &rendering_info) };

                // 3. Bind pipeline + per-instance vertex buffer.
                unsafe {
                    crate::vk_count!(cmd_bind_pipeline);
                    vk.device.cmd_bind_pipeline(
                        cb,
                        ash::vk::PipelineBindPoint::GRAPHICS,
                        prim_pipeline,
                    );
                    vk.device
                        .cmd_bind_vertex_buffers(cb, 0, &[alloc.buffer], &[alloc.offset]);
                }

                // 4. Push constants (shared layout: VERTEX +
                //    FRAGMENT visibility).
                let pc = TrapDrawPushConsts {
                    mask_extent: [mask_extent.width as f32, mask_extent.height as f32],
                    bbox_origin_pixel: [bbox_x as f32, bbox_y as f32],
                    bbox_size_pixel: [bbox_w as f32, bbox_h as f32],
                    _pad: [0.0; 2],
                };
                unsafe {
                    crate::vk_count!(cmd_push_constants);
                    vk.device.cmd_push_constants(
                        cb,
                        prim_layout,
                        ash::vk::ShaderStageFlags::VERTEX | ash::vk::ShaderStageFlags::FRAGMENT,
                        0,
                        pc.as_bytes(),
                    );
                }

                // 5. Viewport + scissor (dynamic state).
                let viewport = ash::vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: mask_extent.width as f32,
                    height: mask_extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                };
                unsafe {
                    crate::vk_count!(cmd_set_viewport);
                    vk.device.cmd_set_viewport(cb, 0, &[viewport]);
                    crate::vk_count!(cmd_set_scissor);
                    vk.device.cmd_set_scissor(cb, 0, &[bbox_render_area]);
                }

                // 6. Draw: 4 verts (unit quad via TRIANGLE_STRIP)
                //    × n_instances.
                crate::vk_count!(cmd_draw);
                unsafe { vk.device.cmd_draw(cb, 4, n_instances, 0, 0) };

                // 7. End rendering.
                crate::vk_count!(cmd_end_rendering);
                unsafe { vk.device.cmd_end_rendering(cb) };

                // 8. Barrier COLOR_ATTACHMENT → SHADER_READ_ONLY
                //    for the upcoming composite that samples the
                //    mask at binding 1.
                let to_read = [ash::vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(ash::vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                    .src_access_mask(ash::vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
                    .dst_stage_mask(ash::vk::PipelineStageFlags2::FRAGMENT_SHADER)
                    .dst_access_mask(ash::vk::AccessFlags2::SHADER_SAMPLED_READ)
                    .old_layout(ash::vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .new_layout(ash::vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .image(mask_image)
                    .subresource_range(color_range)];
                let dep = ash::vk::DependencyInfo::default().image_memory_barriers(&to_read);
                crate::vk_count!(cmd_pipeline_barrier2);
                unsafe { vk.device.cmd_pipeline_barrier2(cb, &dep) };
                // Layout-tracking update deferred to AFTER the
                // final fallible record step (record_render_composite).
                // The cmd_pipeline_barrier2 above is RECORDED into
                // the CB but only executes when the CB is submitted.
                // If a subsequent record step fails the batch is
                // poisoned and the CB never submits, so the GPU
                // never sees these barriers — advancing the CPU
                // layout-tracking here would diverge from GPU
                // reality. See `set_current_layout` after
                // `record_render_composite` below.

                // ---- Composite: src ⊗ MaskScratch → dst (shared) ----
                let descriptor_set = render_cache.allocate_descriptor_for_views_into(
                    batch.descriptor_arena_mut(),
                    src_view,
                    mask_view,
                    dst_readback_view,
                )?;

                if let Some(color) = src_clear_color {
                    record_solid_color_clear(vk, cb, solid_src_image, color);
                }
                // Disjoint/Conjoint: snapshot dst into the readback
                // scratch so the shader can sample it at binding 2.
                // Mirrors try_vk_render_composite's sequencing.
                if let Some(rb) = dst_readback {
                    rb.record_copy_from(
                        cb,
                        dst_mirror.vk_image,
                        dst_mirror.current_layout(),
                        dst_format,
                        dst_mirror.extent,
                    );
                }
                let composite_result = vk_render::record_render_composite(
                    vk,
                    cb,
                    dst_mirror,
                    pipeline,
                    pipeline_layout,
                    descriptor_set,
                    &attrs,
                    &rects,
                    scissor,
                );
                // gpu-trap T2 P2 fix-up: advance MaskScratch's
                // CPU-tracked layout to SHADER_READ_ONLY_OPTIMAL ONLY
                // after the final fallible record step (record_render_composite)
                // has succeeded. If any prior step in this closure
                // failed via `?`, control never reaches here and the
                // batch will poison — the recorded COLOR_ATTACHMENT →
                // SHADER_READ_ONLY barrier never executes on the GPU,
                // so advancing CPU layout-tracking would diverge from
                // GPU reality. T3: both arms now go through the GPU
                // path, so the deferred update fires unconditionally
                // on composite-record success.
                if composite_result.is_ok() {
                    mask_scratch.set_current_layout(ash::vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                }
                composite_result
            });
        if arena_oom {
            return false;
        }
        match result {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk render_traps: record failed on dst xid {dst_xid:#x}: \
                     {e:?} — falling back to pixman"
                );
                false
            }
        }
    }

    /// Sub-phase 4.1.4.7 helper. Vulkan-direct RENDER `CompositeGlyphs`.
    ///
    /// Only the rendercheck-relevant case is supported: SolidFill src
    /// and drawable dst, glyphsets in `A8` or `A1` format. Glyphs intern
    /// into the shared 4.1.4.5 atlas (same key namespace as text runs —
    /// `(font_xid = gs_xid, codepoint = glyph_id)`); A1 bitmaps are
    /// expanded to A8 host-side because the atlas is `R8_UNORM`.
    ///
    /// Op is ignored — the text pipeline always premul-srcover blends.
    /// rendercheck's CompositeGlyphs cases use `Over`, which matches
    /// the pipeline's blend state. Other ops fall through to pixman.
    #[allow(clippy::too_many_arguments)]
    fn try_vk_render_composite_glyphs(
        &mut self,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        host_gs: u32,
        src_x: i16,
        src_y: i16,
        items: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> bool {
        use crate::kms::vk::{glyph::GlyphKey, ops::text as vk_text};

        // PictOp `Over` (3) is the natural fit for the text pipeline's
        // pre-mul srcover blend state. `Src` (1) overrides dst rather
        // than blending — incorrect if the run overlaps existing
        // pixels. Conservative: only handle `Over`.
        if op != 3 {
            log::debug!("vk text bail: op={op} (only Over=3 supported)");
            return false;
        }

        // SolidFill src only.
        let foreground_premul = match self.pictures.get(&host_src) {
            Some(PictureState::SolidFill { premul, .. }) => *premul,
            Some(other) => {
                log::debug!(
                    "vk text bail: src 0x{host_src:x} is {:?}, expected SolidFill",
                    std::mem::discriminant(other)
                );
                return false;
            }
            None => {
                log::debug!("vk text bail: src 0x{host_src:x} not registered");
                return false;
            }
        };

        let (dst_xid, _clip) = match self.pictures.get(&host_dst) {
            Some(PictureState::Drawable { host_xid, clip, .. }) => (*host_xid, clip.clone()),
            Some(other) => {
                log::debug!(
                    "vk text bail: dst 0x{host_dst:x} is {:?}, expected Drawable",
                    std::mem::discriminant(other)
                );
                return false;
            }
            None => {
                log::debug!("vk text bail: dst 0x{host_dst:x} not registered");
                return false;
            }
        };

        if !self.glyphsets.contains_key(&host_gs) {
            log::debug!("vk text bail: glyphset 0x{host_gs:x} not registered");
            return false;
        }

        // 3E: acquire batch resources up-front (gated by
        // renderer_failed). Atlas upload (intern) and the recorder
        // BOTH route through this — single source for vk_arc /
        // pool_handle.
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            log::debug!(
                "vk text bail: paint_resources unavailable (renderer_failed or vk/pool absent)"
            );
            return false;
        };
        if self.glyph_atlas.is_none() || self.text_pipeline.is_none() {
            log::debug!(
                "vk text bail: atlas_init={} pipeline_init={}",
                self.glyph_atlas.is_some(),
                self.text_pipeline.is_some()
            );
            return false;
        }

        // Mirror format check — atlas + text pipeline target BGRA.
        let mirror_format = if let Some(w) = self.windows.get(&dst_xid) {
            w.vk_mirror.as_ref().map(|m| m.format)
        } else if let Some(p) = self.pixmaps.get(&dst_xid) {
            p.vk_mirror.as_ref().map(|m| m.format)
        } else {
            None
        };
        if mirror_format != Some(ash::vk::Format::B8G8R8A8_UNORM) {
            log::debug!(
                "vk text bail: dst mirror format {:?} != BGRA (dst_xid=0x{dst_xid:x})",
                mirror_format
            );
            return false;
        }

        // Walk the items list (same parser as `composite_glyphs_onto`)
        // to build a `Vec<TextGlyph>`. Any unsupported glyph format
        // makes us bail to pixman so the run renders consistently.
        let id_size: usize = match minor {
            23 => 1,
            24 => 2,
            _ => 4,
        };
        // Per X RENDER protocol, `xSrc`/`ySrc` are the SOURCE picture
        // sampling origin — not the dst pen. The dst pen starts at
        // (0, 0); the first glyph-element's `deltax`/`deltay` sets the
        // absolute pen position, subsequent elements accumulate. Adding
        // `src_x`/`src_y` to the pen shifts every glyph by (xSrc, ySrc)
        // because GTK conventionally sets xSrc/ySrc equal to the first
        // delta (which becomes the desired absolute pen). For the
        // gtk3-demo TreeView this displaced each row's label into the
        // NEXT row, which was then erased by the next FillRect for
        // that row's background (2026-05-11 diagnosis). `x_off`/`y_off`
        // are dispatcher-supplied padding (always 0 here) and are kept
        // for parity with the trait signature.
        let _ = (src_x, src_y);
        let mut pen_x = i32::from(x_off);
        let mut pen_y = i32::from(y_off);
        let mut pos: usize = 0;
        let mut active_gs_xid = host_gs;
        let mut glyphs_to_draw: Vec<vk_text::TextGlyph> = Vec::new();
        // Throwaway A8 buffer for A1→A8 expansion; reused per glyph.
        let mut a8_scratch: Vec<u8> = Vec::new();

        while pos + 8 <= items.len() {
            let count = items[pos] as usize;
            if count == 255 {
                if pos + 8 <= items.len() {
                    let new_xid = u32::from_le_bytes([
                        items[pos + 4],
                        items[pos + 5],
                        items[pos + 6],
                        items[pos + 7],
                    ]);
                    if new_xid != 0 && self.glyphsets.contains_key(&new_xid) {
                        active_gs_xid = new_xid;
                    }
                }
                pos += 8;
                continue;
            }
            let dx = i16::from_le_bytes([items[pos + 4], items[pos + 5]]) as i32;
            let dy = i16::from_le_bytes([items[pos + 6], items[pos + 7]]) as i32;
            pen_x += dx;
            pen_y += dy;

            let payload_start = pos + 8;
            let payload_bytes = count * id_size;
            let padded = (payload_bytes + 3) & !3;
            if payload_start + padded > items.len() {
                break;
            }

            let Some(active_gs) = self.glyphsets.get(&active_gs_xid) else {
                pos += 8 + padded;
                continue;
            };
            let active_gs_xid_for_key = active_gs_xid;

            for i in 0..count {
                let id_off = payload_start + i * id_size;
                let glyph_id: u32 = match id_size {
                    1 => u32::from(items[id_off]),
                    2 => u32::from(u16::from_le_bytes([items[id_off], items[id_off + 1]])),
                    _ => u32::from_le_bytes([
                        items[id_off],
                        items[id_off + 1],
                        items[id_off + 2],
                        items[id_off + 3],
                    ]),
                };
                let Some(glyph) = active_gs.glyphs.get(&glyph_id) else {
                    continue;
                };

                let gw = glyph.width as u32;
                let gh = glyph.height as u32;
                let dst_x = pen_x - i32::from(glyph.x);
                let dst_y = pen_y - i32::from(glyph.y);

                if gw > 0 && gh > 0 {
                    let pixels: &[u8] = match glyph.format {
                        GlyphSetFormat::A8 => &glyph.pixels,
                        GlyphSetFormat::A1 => {
                            // Wire A1: rows MSB-first, 32-bit padded.
                            // Expand into row-major A8 (0 or 255).
                            let wire_stride = (gw as usize).div_ceil(32) * 4;
                            a8_scratch.clear();
                            a8_scratch.resize((gw * gh) as usize, 0);
                            for row in 0..(gh as usize) {
                                let src_off = row * wire_stride;
                                if src_off + wire_stride > glyph.pixels.len() {
                                    break;
                                }
                                for col in 0..(gw as usize) {
                                    let byte = glyph.pixels[src_off + col / 8];
                                    // X11 A1 is LSB-first within each byte (per
                                    // glyph protocol; differs from pixman's wire
                                    // format which is MSB. The existing pixman
                                    // path relies on pixman_image_create A1, which
                                    // accepts MSB-first — but `parse_add_glyphs`
                                    // strips no bit-ordering, so we read the
                                    // wire as-is). Use MSB-first: bit `7-(col%8)`.
                                    let bit = (byte >> (7 - (col & 7))) & 1;
                                    a8_scratch[row * (gw as usize) + col] =
                                        if bit != 0 { 0xFF } else { 0 };
                                }
                            }
                            &a8_scratch
                        }
                        // ARGB32-source glyphs are pre-converted to A8 in
                        // `parse_add_glyphs`, so the stored format is A8 —
                        // we should never see ARGB32 here. Be defensive.
                        GlyphSetFormat::Argb32 | GlyphSetFormat::Other => {
                            return false;
                        }
                    };

                    let key = GlyphKey {
                        font_xid: active_gs_xid_for_key,
                        codepoint: glyph_id,
                    };
                    let atlas = self.glyph_atlas.as_mut().expect("checked above");
                    let Some(entry) = atlas.intern(key, gw, gh, 0, 0, pixels, pool_handle) else {
                        // Atlas full or upload failed — fall back.
                        return false;
                    };
                    glyphs_to_draw.push(vk_text::TextGlyph {
                        entry,
                        dst_x,
                        dst_y,
                    });
                }

                pen_x += i32::from(glyph.x_off);
                pen_y += i32::from(glyph.y_off);
            }

            pos += 8 + padded;
        }

        if glyphs_to_draw.is_empty() {
            return true;
        }

        // Re-borrow the mirror mutably for the recording. The
        // closure also captures atlas + pipeline (read-only) from
        // disjoint fields. self.scheduler.record_paint_op is the
        // remaining mutable borrow; field-disjoint, so the borrow
        // checker accepts.
        let mirror = if let Some(w) = self.windows.get_mut(&dst_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&dst_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(mirror) = mirror else {
            return false;
        };
        let atlas = self.glyph_atlas.as_ref().expect("checked above");
        let pipeline = self.text_pipeline.as_ref().expect("checked above");

        match self
            .scheduler
            .record_paint_op(vk_arc, pool_handle, |vk, cb| {
                vk_text::record_text_run(
                    vk,
                    cb,
                    mirror,
                    atlas,
                    pipeline,
                    &glyphs_to_draw,
                    foreground_premul,
                )
            }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk render_composite_glyphs: record failed on dst xid {dst_xid:#x}: {e:?} \
                     — falling back to pixman"
                );
                false
            }
        }
    }

    /// Sub-phase 4.1.4.6 helper. Confirms the drawable has a
    /// `vk_mirror` whose layout is sampleable. The transitional
    /// version of this also flushed pixman → mirror inline, but
    /// once new mirrors are cleared on creation and PutImage
    /// covers every depth (incl. R8 / depth-1), there's no
    /// situation where the mirror lags pixman — every Vk write
    /// goes straight to the mirror, and pixman fall-through paths
    /// (still around for unported ops) call `mark_full_damage`
    /// which `MirrorUploader` syncs on the next composite frame.
    /// Until then, sampling a never-touched mirror after a
    /// pixman-only write returns the cleared zero contents — that
    /// only matters for the very first frame after creation, and
    /// real clients don't `RenderComposite` on a zero-content
    /// drawable.
    fn ensure_drawable_mirror_sampleable(&self, host_xid: u32) -> bool {
        let m = if let Some(w) = self.windows.get(&host_xid) {
            w.vk_mirror.as_ref()
        } else if let Some(p) = self.pixmaps.get(&host_xid) {
            p.vk_mirror.as_ref()
        } else {
            None
        };
        m.is_some()
    }

    /// Sub-phase 4.1.4.6 helper. Build the per-rect plan + scissor
    /// for `try_vk_render_composite`. Returns `None` if all rects
    /// fall outside the clip (caller treats that as success — no
    /// pixels to draw — and returns `Ok(())` from the trait
    /// method). Otherwise returns the survivors plus a single
    /// scissor rectangle that bounds the picture clip; the
    /// per-pipeline blend handles the actual rect masking through
    /// the draw quad geometry.
    #[allow(clippy::too_many_arguments)]
    fn build_render_composite_inputs(
        &self,
        clip: &Option<Vec<Rectangle16>>,
        src_x: i16,
        src_y: i16,
        mask_x: i16,
        mask_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> Option<(
        Vec<crate::kms::vk::ops::render::CompositeRect>,
        ash::vk::Rect2D,
    )> {
        if width == 0 || height == 0 {
            return None;
        }
        // Compute scissor as the union of the picture clip's
        // rectangles, or the unit-screen if no clip is set. This
        // is coarse — the right thing is one scissor *change* per
        // sub-rect, but the parent plan accepts that as a 4.1.4.8
        // tightening.
        let scissor = match clip {
            Some(rects) if !rects.is_empty() => {
                let mut x0 = i32::MAX;
                let mut y0 = i32::MAX;
                let mut x1 = i32::MIN;
                let mut y1 = i32::MIN;
                for r in rects {
                    let rx0 = i32::from(r.x);
                    let ry0 = i32::from(r.y);
                    let rx1 = rx0 + i32::from(r.width);
                    let ry1 = ry0 + i32::from(r.height);
                    x0 = x0.min(rx0);
                    y0 = y0.min(ry0);
                    x1 = x1.max(rx1);
                    y1 = y1.max(ry1);
                }
                if x1 <= x0 || y1 <= y0 {
                    return None;
                }
                let x0 = x0.max(0);
                let y0 = y0.max(0);
                ash::vk::Rect2D {
                    offset: ash::vk::Offset2D { x: x0, y: y0 },
                    extent: ash::vk::Extent2D {
                        width: u32::try_from((x1 - x0).max(0)).unwrap_or(0),
                        height: u32::try_from((y1 - y0).max(0)).unwrap_or(0),
                    },
                }
            }
            _ => ash::vk::Rect2D {
                offset: ash::vk::Offset2D::default(),
                extent: ash::vk::Extent2D {
                    width: u32::MAX,
                    height: u32::MAX,
                },
            },
        };
        let rects = vec![crate::kms::vk::ops::render::CompositeRect {
            src_x: i32::from(src_x),
            src_y: i32::from(src_y),
            mask_x: i32::from(mask_x),
            mask_y: i32::from(mask_y),
            dst_x: i32::from(dst_x),
            dst_y: i32::from(dst_y),
            width: u32::from(width),
            height: u32::from(height),
        }];
        Some((rects, scissor))
    }

    /// Sub-phase 4.1.4.6 helper. Vulkan-direct RENDER `Composite`.
    ///
    /// `op` is one of the 13 standard PictOps. `src` and `mask`
    /// pictures resolve via [`RenderPic`] to either a Drawable
    /// mirror, a `SolidFill` colour, or (for mask only) the
    /// `RenderPic::None` sentinel which the recorder handles by
    /// binding the backend-shared white-mask scratch.
    ///
    /// Out of scope: transform, `component_alpha`, `alpha_map`,
    /// non-`None` repeat. Caller pre-flights those and falls
    /// through to pixman.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn try_vk_render_composite(
        &mut self,
        op: u8,
        src: RenderPic,
        mask: RenderPic,
        dst_xid: u32,
        rects: &[crate::kms::vk::ops::render::CompositeRect],
        scissor: ash::vk::Rect2D,
        src_repeat: Repeat,
        mask_repeat: Repeat,
        src_xform: Option<PictTransform>,
        mask_xform: Option<PictTransform>,
        mask_component_alpha: bool,
        src_filter: &PictureFilter,
    ) -> bool {
        use crate::kms::vk::{
            ops::render as vk_render,
            render_pipeline::{StdPictOp, record_solid_color_clear},
        };

        if rects.is_empty() {
            return true;
        }

        // Convolution-filter early dispatch (phase 2 of the
        // convolution-filter work, plan
        // 2026-05-15-render-convolution-filter.md). When the source
        // picture has `SetPictureFilter convolution`, the destination
        // is BGRA, the op is Over, there is no mask, and the source
        // transform / repeat are identity / None, route to the
        // single-pass convolution pipeline. Anything else falls
        // through to the standard LINEAR-sampler path below
        // (kernel is silently ignored — matches pre-phase-2
        // behaviour, hard-edged shadows).
        if matches!(src_filter, PictureFilter::Convolution { .. }) {
            // Diagnostic — every gating predicate we evaluate, so a
            // single `RUST_LOG=debug` run shows why a convolution-
            // source composite lands on the standard path instead
            // (vs. the assumption that the convolution path is
            // engaging silently).
            let src_is_drawable = matches!(src, RenderPic::Drawable(_));
            let mask_is_none = matches!(mask, RenderPic::None);
            let xform_identity = src_xform.is_none();
            let repeat_none = matches!(src_repeat, Repeat::None);
            let pipeline_ready = self.convolution_pipeline.is_some();
            log::debug!(
                "vk composite: convolution-source gating dst=0x{dst_xid:x} op={op} \
                 src_drawable={src_is_drawable} mask_none={mask_is_none} \
                 xform_identity={xform_identity} repeat_none={repeat_none} \
                 pipeline_ready={pipeline_ready}"
            );
            if src_is_drawable
                && mask_is_none
                && op == 3
                && xform_identity
                && repeat_none
                && pipeline_ready
                && let Some(t) = self
                    .try_vk_render_composite_convolution(src, dst_xid, rects, scissor, src_filter)
            {
                return t;
            }
        }

        let Some(std_op) = StdPictOp::from_u8(op) else {
            log::debug!("vk composite bail: unsupported op={op} (dst=0x{dst_xid:x})");
            return false;
        };

        // Self-composite (src or mask aliases dst) needs a staging
        // image; not supported in this commit.
        let src_xid_if_drawable = match src {
            RenderPic::Drawable(xid) => Some(xid),
            _ => None,
        };
        let mask_xid_if_drawable = match mask {
            RenderPic::Drawable(xid) => Some(xid),
            _ => None,
        };
        if src_xid_if_drawable == Some(dst_xid) || mask_xid_if_drawable == Some(dst_xid) {
            log::debug!(
                "vk composite bail: self-composite src/mask aliases dst (dst=0x{dst_xid:x})"
            );
            return false;
        }

        // 3F-1: acquire batch resources up-front (gated by
        // renderer_failed). Replaces the raw vk + ops_pool reads;
        // the renderer_failed gate inside paint_resources() is the
        // same one fill/copy/image/text now go through.
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            log::debug!(
                "vk composite bail: paint_resources unavailable (renderer_failed or vk/pool absent) (dst=0x{dst_xid:x})"
            );
            return false;
        };
        if self.render_pipelines.is_none()
            || self.solid_src_image.is_none()
            || self.solid_mask_image.is_none()
            || self.white_mask_image.is_none()
        {
            log::debug!("vk composite bail: pipelines/solid scratches uninit (dst=0x{dst_xid:x})");
            return false;
        }
        let needs_dst_readback = std_op.needs_dst_readback();
        if needs_dst_readback && self.dst_readback.is_none() {
            log::debug!(
                "vk composite bail: op needs dst_readback but readback uninit (dst=0x{dst_xid:x} op={op})"
            );
            return false;
        }

        // Dst format check. B8G8R8A8 covers depth-24/32 mirrors; R8
        // covers depth-1/8 (a8 picture) — the pipeline cache builds
        // a parallel family for that attachment format.
        let (dst_format, dst_extent, dst_depth) = {
            let (m, depth) = if let Some(w) = self.windows.get(&dst_xid) {
                (w.vk_mirror.as_ref(), w.depth)
            } else if let Some(p) = self.pixmaps.get(&dst_xid) {
                (p.vk_mirror.as_ref(), p.depth)
            } else {
                (None, 0)
            };
            match m {
                Some(m) => (m.format, m.extent, depth),
                None => {
                    log::debug!(
                        "vk composite bail: dst mirror missing (dst=0x{dst_xid:x} \
                         is_window={} is_pixmap={})",
                        self.windows.contains_key(&dst_xid),
                        self.pixmaps.contains_key(&dst_xid),
                    );
                    return false;
                }
            }
        };
        if !matches!(
            dst_format,
            ash::vk::Format::B8G8R8A8_UNORM | ash::vk::Format::R8_UNORM
        ) {
            log::debug!(
                "vk composite bail: dst mirror format {dst_format:?} not BGRA/R8 (dst=0x{dst_xid:x})"
            );
            return false;
        }
        // R8 attachments are alpha-only (a8 pictures) so the
        // attachment alpha is always meaningful; for BGRA, alpha
        // is meaningful only on depth-32 (a8r8g8b8).
        let dst_has_alpha = dst_format == ash::vk::Format::R8_UNORM || dst_depth == 32;
        // Synchronous pixman→mirror flush for any Drawable src/mask
        // that has unflushed pixman damage or is still in
        // `UNDEFINED` layout. Without this the Vk Composite would
        // sample stale frame bytes (mirror behind pixman) or
        // undefined contents (mirror never written). Per the
        // family-port directive we don't gate on stale-mirror —
        // we flush.
        if let Some(xid) = src_xid_if_drawable
            && !self.ensure_drawable_mirror_sampleable(xid)
        {
            let in_windows = self.windows.contains_key(&xid);
            let in_pixmaps = self.pixmaps.contains_key(&xid);
            let win_mirror = self
                .windows
                .get(&xid)
                .map(|w| w.vk_mirror.is_some())
                .unwrap_or(false);
            let pix_mirror = self
                .pixmaps
                .get(&xid)
                .map(|p| p.vk_mirror.is_some())
                .unwrap_or(false);
            let pix_depth = self.pixmaps.get(&xid).map(|p| p.depth).unwrap_or(0);
            log::debug!(
                "vk composite bail: src mirror not sampleable (src=0x{xid:x} dst=0x{dst_xid:x} \
                 in_windows={in_windows} in_pixmaps={in_pixmaps} win_mirror={win_mirror} \
                 pix_mirror={pix_mirror} pix_depth={pix_depth})"
            );
            return false;
        }
        if let Some(xid) = mask_xid_if_drawable
            && !self.ensure_drawable_mirror_sampleable(xid)
        {
            log::debug!(
                "vk composite bail: mask mirror not sampleable (mask=0x{xid:x} dst=0x{dst_xid:x})"
            );
            return false;
        }
        // Drawable src must be `B8G8R8A8_UNORM` (depth-24/32) or
        // `R8_UNORM` (a8 picture). For R8 src we bind a swizzled
        // view (a = R, rgb = 0) so the shader sees a (0, 0, 0, alpha)
        // sample — matching the X RENDER convention that components
        // missing from the picture format default to 0 (rgb) and the
        // alpha default lives in the stored byte.
        if let Some(xid) = src_xid_if_drawable {
            let f = if let Some(w) = self.windows.get(&xid) {
                w.vk_mirror.as_ref().map(|m| m.format)
            } else if let Some(p) = self.pixmaps.get(&xid) {
                p.vk_mirror.as_ref().map(|m| m.format)
            } else {
                None
            };
            if !matches!(
                f,
                Some(ash::vk::Format::B8G8R8A8_UNORM | ash::vk::Format::R8_UNORM)
            ) {
                log::debug!(
                    "vk composite bail: src mirror format {f:?} not BGRA/R8 (src=0x{xid:x} dst=0x{dst_xid:x})"
                );
                return false;
            }
        }

        // Resolve & build pipeline.
        let pipeline = match self.render_pipelines.as_mut().expect("checked above").get(
            std_op,
            dst_format,
            dst_has_alpha,
            mask_component_alpha,
        ) {
            Ok(p) => p,
            Err(e) => {
                log::warn!(
                    "vk render_composite: pipeline build failed for op {op}: {e:?} — \
                     falling back to pixman"
                );
                return false;
            }
        };
        let pipeline_layout = self
            .render_pipelines
            .as_ref()
            .expect("checked above")
            .pipeline_layout();

        let solid_src_view = self
            .solid_src_image
            .as_ref()
            .expect("checked above")
            .image_view();
        let solid_mask_view = self
            .solid_mask_image
            .as_ref()
            .expect("checked above")
            .image_view();
        let white_mask_view = self
            .white_mask_image
            .as_ref()
            .expect("checked above")
            .image_view();

        // Resolve src view + extent + (optional) clear colour. Track
        // any per-source affine that the picture itself induces (e.g.
        // a gradient's axis projection) so we can compose it onto the
        // user transform later.
        let mut src_clear_color: Option<[f32; 4]> = None;
        let src_view;
        let src_extent;
        let src_picture_xform: Option<crate::kms::vk::ops::render::AffineXform>;
        let mut src_is_synthetic_1x1 = false;
        match src {
            RenderPic::Drawable(xid) => {
                let (m_format, extent, depth) = {
                    let (m, depth) = if let Some(w) = self.windows.get(&xid) {
                        (w.vk_mirror.as_ref(), w.depth)
                    } else if let Some(p) = self.pixmaps.get(&xid) {
                        (p.vk_mirror.as_ref(), p.depth)
                    } else {
                        (None, 0)
                    };
                    let Some(m) = m else { return false };
                    (m.format, m.extent, depth)
                };
                // Per X RENDER, missing components default — alpha is 1
                // when the picture format has no alpha mask. Pick a
                // swizzled view that enforces this at sample time:
                //   * R8 mirror (depth 1/8, a8 picture) → mask view
                //     (a = R) so shader sees (0, 0, 0, alpha).
                //   * BGRA mirror with depth 24 (r8g8b8 picture) →
                //     no-alpha view (a = ONE) so the alpha byte
                //     stored in the mirror is ignored.
                //   * BGRA mirror with depth 32 (a8r8g8b8) → regular
                //     view; alpha byte is meaningful.
                let view = if m_format == ash::vk::Format::R8_UNORM {
                    let v = if let Some(w) = self.windows.get_mut(&xid) {
                        w.vk_mirror
                            .as_mut()
                            .map(|m| m.mask_image_view())
                            .transpose()
                    } else if let Some(p) = self.pixmaps.get_mut(&xid) {
                        p.vk_mirror
                            .as_mut()
                            .map(|m| m.mask_image_view())
                            .transpose()
                    } else {
                        Ok(None)
                    };
                    match v {
                        Ok(Some(v)) => v,
                        _ => return false,
                    }
                } else if depth == 24 {
                    let v = if let Some(w) = self.windows.get_mut(&xid) {
                        w.vk_mirror
                            .as_mut()
                            .map(|m| m.no_alpha_src_image_view())
                            .transpose()
                    } else if let Some(p) = self.pixmaps.get_mut(&xid) {
                        p.vk_mirror
                            .as_mut()
                            .map(|m| m.no_alpha_src_image_view())
                            .transpose()
                    } else {
                        Ok(None)
                    };
                    match v {
                        Ok(Some(v)) => v,
                        _ => return false,
                    }
                } else if let Some(w) = self.windows.get(&xid) {
                    w.vk_mirror.as_ref().expect("checked above").vk_image_view
                } else {
                    self.pixmaps
                        .get(&xid)
                        .and_then(|p| p.vk_mirror.as_ref())
                        .expect("checked above")
                        .vk_image_view
                };
                src_view = view;
                src_extent = extent;
                src_picture_xform = None;
            }
            RenderPic::Solid(color) => {
                src_view = solid_src_view;
                src_extent = ash::vk::Extent2D {
                    width: 1,
                    height: 1,
                };
                src_clear_color = Some(color);
                src_picture_xform = None;
                src_is_synthetic_1x1 = true;
            }
            RenderPic::Gradient(xid) => {
                let Some(PictureState::Gradient { gradient, .. }) = self.pictures.get(&xid) else {
                    return false;
                };
                src_view = gradient.image_view();
                src_extent = gradient.extent();
                src_picture_xform = Some(gradient.axis_projection);
            }
            RenderPic::None => return false,
        }

        // Resolve mask view + extent + (optional) clear colour.
        // For the no-mask case bind the white scratch.
        let mut mask_clear_color: Option<[f32; 4]> = None;
        let mask_view;
        let mask_extent;
        let mask_picture_xform: Option<crate::kms::vk::ops::render::AffineXform>;
        let mut mask_is_synthetic_1x1 = false;
        match mask {
            RenderPic::Drawable(xid) => {
                // Resolve mask format. R8 mirror → swizzle (a = R)
                // for the alpha-only mask. BGRA mirror with depth 24
                // (no alpha mask in the picture format) → swizzle
                // (a = ONE) so a depth-24 RGB picture used as a mask
                // gives mask.a = 1 per X RENDER's "alpha defaults
                // to 1" rule. BGRA mirror with depth 32 → regular
                // view (alpha is meaningful).
                let (m_format, depth) = {
                    let (m, depth) = if let Some(w) = self.windows.get(&xid) {
                        (w.vk_mirror.as_ref(), w.depth)
                    } else if let Some(p) = self.pixmaps.get(&xid) {
                        (p.vk_mirror.as_ref(), p.depth)
                    } else {
                        (None, 0)
                    };
                    let Some(m) = m else { return false };
                    mask_extent = m.extent;
                    (m.format, depth)
                };
                if m_format != ash::vk::Format::B8G8R8A8_UNORM
                    && m_format != ash::vk::Format::R8_UNORM
                {
                    return false;
                }
                let view = if m_format == ash::vk::Format::R8_UNORM {
                    let v = if let Some(w) = self.windows.get_mut(&xid) {
                        w.vk_mirror
                            .as_mut()
                            .map(|m| m.mask_image_view())
                            .transpose()
                    } else if let Some(p) = self.pixmaps.get_mut(&xid) {
                        p.vk_mirror
                            .as_mut()
                            .map(|m| m.mask_image_view())
                            .transpose()
                    } else {
                        Ok(None)
                    };
                    match v {
                        Ok(Some(v)) => v,
                        _ => return false,
                    }
                } else if depth == 24 {
                    let v = if let Some(w) = self.windows.get_mut(&xid) {
                        w.vk_mirror
                            .as_mut()
                            .map(|m| m.no_alpha_src_image_view())
                            .transpose()
                    } else if let Some(p) = self.pixmaps.get_mut(&xid) {
                        p.vk_mirror
                            .as_mut()
                            .map(|m| m.no_alpha_src_image_view())
                            .transpose()
                    } else {
                        Ok(None)
                    };
                    match v {
                        Ok(Some(v)) => v,
                        _ => return false,
                    }
                } else if let Some(w) = self.windows.get(&xid) {
                    w.vk_mirror.as_ref().expect("checked above").vk_image_view
                } else {
                    self.pixmaps
                        .get(&xid)
                        .and_then(|p| p.vk_mirror.as_ref())
                        .expect("checked above")
                        .vk_image_view
                };
                mask_view = view;
                mask_picture_xform = None;
            }
            RenderPic::Solid(color) => {
                mask_view = solid_mask_view;
                mask_extent = ash::vk::Extent2D {
                    width: 1,
                    height: 1,
                };
                mask_clear_color = Some(color);
                mask_picture_xform = None;
                mask_is_synthetic_1x1 = true;
            }
            RenderPic::Gradient(xid) => {
                let Some(PictureState::Gradient { gradient, .. }) = self.pictures.get(&xid) else {
                    return false;
                };
                mask_view = gradient.image_view();
                mask_extent = gradient.extent();
                mask_picture_xform = Some(gradient.axis_projection);
            }
            RenderPic::None => {
                mask_view = white_mask_view;
                mask_extent = ash::vk::Extent2D {
                    width: 1,
                    height: 1,
                };
                mask_picture_xform = None;
                mask_is_synthetic_1x1 = true;
            }
        }

        // 3F-1 / 5-T4: For Disjoint/Conjoint ops the shader reads the
        // dst pixel through binding 2; we copy dst → scratch inside
        // the CB below and bind the scratch's sampleable view here.
        // For standard ops the binding is unused — bind the
        // white-mask scratch to satisfy the descriptor layout.
        //
        // The pre-Phase-5 pre-flush gate (needs_grow → ProtocolBarrier)
        // is gone: DstReadback now uses `ensure_returning_old`, which
        // hands the old image to the scheduler's defer-release flow.
        // The old image survives any in-flight CB that references it.
        let dst_readback_view = if needs_dst_readback {
            // The scratch's &mut borrow MUST end BEFORE
            // `self.scheduler.defer_resource_release` borrows
            // `&mut self`. Use a tight block so the `as_mut()` binding
            // drops at the closing brace, then reborrow for view
            // extraction.
            let retired = {
                let scratch = self.dst_readback.as_mut().expect("checked above");
                match scratch.ensure_returning_old(dst_format, dst_extent.width, dst_extent.height)
                {
                    Ok(r) => r,
                    Err(e) => {
                        log::warn!("vk render_composite: dst readback ensure failed: {e:?}");
                        return false;
                    }
                }
            }; // <-- scratch's &mut borrow ends here.
            if let Some(old) = retired {
                self.scheduler
                    .defer_resource_release(vk_arc.clone(), pool_handle, old);
            }
            // Reborrow self.dst_readback for view extraction now that
            // the earlier &mut and the scheduler borrow have ended.
            let scratch = self.dst_readback.as_mut().expect("checked above");
            match scratch.view(dst_format, dst_has_alpha) {
                Ok(Some(v)) => v,
                Ok(None) => return false,
                Err(e) => {
                    log::warn!("vk render_composite: dst readback view build failed: {e:?}");
                    return false;
                }
            }
        } else {
            white_mask_view
        };

        // Pull mut refs. Split-borrow on disjoint fields lets us
        // hand all of these into the closure simultaneously.
        let dst_mirror = if let Some(w) = self.windows.get_mut(&dst_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&dst_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(dst_mirror) = dst_mirror else {
            return false;
        };
        let solid_src_image = self.solid_src_image.as_mut().expect("checked above");
        let solid_mask_image = self.solid_mask_image.as_mut().expect("checked above");
        let dst_readback = if needs_dst_readback {
            Some(self.dst_readback.as_mut().expect("checked above"))
        } else {
            None
        };

        // Compose the user (picture) transform with the picture's
        // intrinsic transform if any (gradients carry an axis
        // projection). Order: applied dst → user → intrinsic →
        // src-pixel. The shader's affine is `src = M * (origin +
        // dst_offset, 1)`, so when we have an intrinsic `I` and a
        // user `U`, the combined matrix is `I * U`.
        let user_src_xform = pixman_transform_to_affine(src_xform.as_ref(), src_extent);
        let user_mask_xform = pixman_transform_to_affine(mask_xform.as_ref(), mask_extent);
        let combined_src_xform = match src_picture_xform {
            Some(intrinsic) => compose_affines(intrinsic, user_src_xform),
            None => user_src_xform,
        };
        let combined_mask_xform = match mask_picture_xform {
            Some(intrinsic) => compose_affines(intrinsic, user_mask_xform),
            None => user_mask_xform,
        };

        // Synthetic 1×1 scratches (Solid src/mask, no-mask white) need
        // PAD: the single texel is meant to cover the whole rect, and
        // REPEAT_NONE would zero every dst pixel beyond uv 1. For
        // gradients the LUT has dimension 1 in the cross-axis but the
        // axis_projection maps freely along the gradient axis — the
        // user-specified repeat governs how out-of-range t values are
        // sampled (clients pass NORMAL/PAD/REFLECT for tiling/
        // mirroring/clamping behaviour). Honour the user repeat in the
        // shader; the LUT covers t ∈ [0, 1] which is what apply_repeat
        // expects.
        let effective_src_repeat = if src_is_synthetic_1x1 {
            crate::kms::vk::render_pipeline::REPEAT_PAD
        } else {
            repeat_to_shader_const(src_repeat)
        };
        let effective_mask_repeat = if mask_is_synthetic_1x1 {
            crate::kms::vk::render_pipeline::REPEAT_PAD
        } else {
            repeat_to_shader_const(mask_repeat)
        };

        let attrs = vk_render::CompositeAttrs {
            src_extent,
            mask_extent,
            src_repeat: effective_src_repeat,
            mask_repeat: effective_mask_repeat,
            src_xform: combined_src_xform,
            mask_xform: combined_mask_xform,
        };

        // 3F-1: descriptor allocation moves into the closure where
        // `&mut PaintBatch` is available — `batch.descriptor_arena_mut()`
        // returns the per-batch arena. The set lives until batch
        // retirement (NOT until the next render-composite call), so
        // multiple render-composites in one batch don't trample each
        // other's descriptors the way the shared-pool path would.
        //
        // `render_cache` is a shared borrow on self.render_pipelines
        // — disjoint from the &mut self.scheduler that record_paint_batch_op
        // takes, and from the &mut captures (dst_mirror / solid_src /
        // solid_mask / dst_readback) which are all in disjoint fields.
        let render_cache = self.render_pipelines.as_ref().expect("checked above");
        let result = self
            .scheduler
            .record_paint_batch_op(vk_arc, pool_handle, |vk, batch, cb| {
                let descriptor_set = render_cache.allocate_descriptor_for_views_into(
                    batch.descriptor_arena_mut(),
                    src_view,
                    mask_view,
                    dst_readback_view,
                )?;
                if let Some(c) = src_clear_color {
                    record_solid_color_clear(vk, cb, solid_src_image, c);
                }
                if let Some(c) = mask_clear_color {
                    record_solid_color_clear(vk, cb, solid_mask_image, c);
                }
                // Disjoint/Conjoint: snapshot dst into the readback
                // scratch, then restore dst to its current layout so
                // record_render_composite can transition it normally.
                if let Some(rb) = dst_readback {
                    rb.record_copy_from(
                        cb,
                        dst_mirror.vk_image,
                        dst_mirror.current_layout(),
                        dst_format,
                        dst_mirror.extent,
                    );
                }
                vk_render::record_render_composite(
                    vk,
                    cb,
                    dst_mirror,
                    pipeline,
                    pipeline_layout,
                    descriptor_set,
                    &attrs,
                    rects,
                    scissor,
                )
            });
        match result {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk render_composite: record failed on dst xid {dst_xid:#x}: \
                     {e:?} — falling back to pixman"
                );
                false
            }
        }
    }

    /// Phase 2 of the convolution-filter work
    /// (`docs/superpowers/plans/2026-05-15-render-convolution-filter.md`).
    /// Composite a Drawable source through its
    /// [`PictureFilter::Convolution`] kernel onto `dst_xid`.
    ///
    /// Caller has already gated on: filter is Convolution, src is
    /// Drawable, no mask, op == Over, identity src transform,
    /// Repeat::None, [`convolution_pipeline`] is `Some`.
    ///
    /// Returns:
    /// - `Some(true)` — convolution path took the work.
    /// - `Some(false)` — structural failure (dst mirror missing, src
    ///   mirror not sampleable, record failed). Caller surfaces this
    ///   to its caller; the standard render path would have failed
    ///   for the same reason.
    /// - `None` — the kernel falls outside the shader's supported
    ///   range, or the dst / src mirror format is unexpected. Caller
    ///   falls through to the standard LINEAR-sampler path (kernel
    ///   silently ignored, matching pre-phase-2 behaviour).
    ///
    /// [`PictureFilter::Convolution`]: PictureFilter::Convolution
    /// [`convolution_pipeline`]: KmsBackend::convolution_pipeline
    fn try_vk_render_composite_convolution(
        &mut self,
        src: RenderPic,
        dst_xid: u32,
        rects: &[crate::kms::vk::ops::render::CompositeRect],
        scissor: ash::vk::Rect2D,
        src_filter: &PictureFilter,
    ) -> Option<bool> {
        use crate::kms::vk::{
            convolution_pipeline::{ConvolutionKernelData, KERNEL_UBO_ALIGNMENT},
            ops::{render as vk_render, render::AffineXform},
        };

        let RenderPic::Drawable(src_xid) = src else {
            return Some(false);
        };
        let PictureFilter::Convolution {
            width,
            height,
            weights,
        } = src_filter
        else {
            return None;
        };
        let kernel_data = match ConvolutionKernelData::from_parsed(*width, *height, weights) {
            Some(k) => k,
            None => {
                log::debug!(
                    "vk convolution composite: kernel out of range ({width}×{height}), \
                     falling back to LINEAR (dst=0x{dst_xid:x})"
                );
                return None;
            }
        };

        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            log::debug!(
                "vk convolution composite bail: paint_resources unavailable (dst=0x{dst_xid:x})"
            );
            return Some(false);
        };
        // Caller in `try_vk_render_composite` already gated on
        // `self.convolution_pipeline.is_some()` — no defensive recheck.

        // dst mirror format + extent.
        let (dst_format, dst_extent) = {
            let m = if let Some(w) = self.windows.get(&dst_xid) {
                w.vk_mirror.as_ref()
            } else if let Some(p) = self.pixmaps.get(&dst_xid) {
                p.vk_mirror.as_ref()
            } else {
                None
            };
            let Some(m) = m else {
                log::debug!(
                    "vk convolution composite bail: dst mirror missing (dst=0x{dst_xid:x})"
                );
                return Some(false);
            };
            (m.format, m.extent)
        };
        if dst_format != ash::vk::Format::B8G8R8A8_UNORM {
            // Convolution pipeline is BGRA-only for phase 2.
            return None;
        }
        let _ = dst_extent; // surface in attrs below via the dst_mirror.

        if !self.ensure_drawable_mirror_sampleable(src_xid) {
            return Some(false);
        }

        // src view + extent. Convolution pipeline expects a BGRA
        // mirror (matches the typical xfwm4 shadow source: BGRA
        // server-owned pixmap painted via PutImage by the
        // compositor). R8 / depth-1 sources fall back.
        let (src_view, src_extent) = {
            let m = if let Some(w) = self.windows.get(&src_xid) {
                w.vk_mirror.as_ref()
            } else if let Some(p) = self.pixmaps.get(&src_xid) {
                p.vk_mirror.as_ref()
            } else {
                None
            };
            let Some(m) = m else {
                return Some(false);
            };
            if m.format != ash::vk::Format::B8G8R8A8_UNORM {
                return None;
            }
            (m.vk_image_view, m.extent)
        };

        // Split-borrow on disjoint fields: `convolution_pipeline`
        // (shared), `scheduler` (mut via record_paint_batch_op),
        // `windows`/`pixmaps` (mut via the dst_mirror grab).
        let dst_mirror = if let Some(w) = self.windows.get_mut(&dst_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&dst_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(dst_mirror) = dst_mirror else {
            return Some(false);
        };

        let conv_pipeline = self.convolution_pipeline.as_ref().expect("checked above");
        let pipeline = conv_pipeline.pipeline();
        let pipeline_layout = conv_pipeline.pipeline_layout();

        // Convolution shader reads `dst_origin`, `dst_size`,
        // `viewport`, `src_origin`, `src_extent`, `src_xform_*` — the
        // mask + repeat fields are unused. Identity src transform
        // (precondition), unit mask extent (descriptor has no mask
        // binding so the mask UV would never be sampled anyway).
        let attrs = vk_render::CompositeAttrs {
            src_extent,
            mask_extent: ash::vk::Extent2D {
                width: 1,
                height: 1,
            },
            src_repeat: crate::kms::vk::render_pipeline::REPEAT_NONE,
            mask_repeat: crate::kms::vk::render_pipeline::REPEAT_NONE,
            src_xform: AffineXform::IDENTITY,
            mask_xform: AffineXform::IDENTITY,
        };

        let kernel_size = std::mem::size_of::<ConvolutionKernelData>() as u64;
        let result = self
            .scheduler
            .record_paint_batch_op(vk_arc, pool_handle, |vk, batch, cb| {
                // Per-draw UBO slice. The arena chunks live until
                // batch retirement, so the descriptor's buffer
                // binding stays valid for the full CB lifetime.
                let alloc = batch
                    .upload_arena_mut()
                    .alloc(kernel_size, KERNEL_UBO_ALIGNMENT)
                    .map_err(|e| match e {
                        crate::kms::scheduler::batch_upload_arena::ArenaError::Vk(r) => r,
                        crate::kms::scheduler::batch_upload_arena::ArenaError::NoMemoryType => {
                            ash::vk::Result::ERROR_OUT_OF_HOST_MEMORY
                        }
                    })?;
                // SAFETY: `alloc.mapped_ptr` is mapped for `alloc.size`
                // bytes (= `kernel_size` here); we copy exactly that
                // many bytes from `kernel_data`'s in-memory image.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        kernel_data.as_bytes().as_ptr(),
                        alloc.mapped_ptr.as_ptr(),
                        kernel_size as usize,
                    );
                }
                let descriptor_set = conv_pipeline.allocate_descriptor_for_views_into(
                    batch.descriptor_arena_mut(),
                    src_view,
                    alloc.buffer,
                    alloc.offset,
                    kernel_size,
                )?;
                vk_render::record_render_composite(
                    vk,
                    cb,
                    dst_mirror,
                    pipeline,
                    pipeline_layout,
                    descriptor_set,
                    &attrs,
                    rects,
                    scissor,
                )
            });
        match result {
            Ok(()) => Some(true),
            Err(e) => {
                log::warn!("vk convolution composite: record failed on dst 0x{dst_xid:x}: {e:?}");
                Some(false)
            }
        }
    }

    fn window_under_cursor(&self) -> Option<u32> {
        // Top-levels are tracked in stacking order (bottom-to-top) on
        // self.top_level_order. Walk back-to-front so the topmost match
        // wins.
        let cx = self.cursor_x as f64;
        let cy = self.cursor_y as f64;
        for &window_id in self.top_level_order.iter().rev() {
            let Some(w) = self.windows.get(&window_id) else {
                continue;
            };
            if !w.mapped {
                continue;
            }
            // Coarse bounding-box check first.
            if cx < w.x as f64
                || cx >= w.x as f64 + w.width as f64
                || cy < w.y as f64
                || cy >= w.y as f64 + w.height as f64
            {
                continue;
            }
            // Input shape (kind=2) takes precedence; fall back to bounding (kind=0).
            let shape = self
                .shape_input
                .get(&window_id)
                .or_else(|| self.shape_bounding.get(&window_id));
            if let Some(rects) = shape {
                // Empty shape = window is unhittable.
                let inside = rects.iter().any(|r| {
                    let rx = w.x as f64 + r.x as f64;
                    let ry = w.y as f64 + r.y as f64;
                    cx >= rx && cx < rx + r.width as f64 && cy >= ry && cy < ry + r.height as f64
                });
                if !inside {
                    continue;
                }
            }
            return Some(window_id);
        }
        None
    }

    /// Diagnostic-only: dump the window tree at click time so we can
    /// reason about hit-test correctness without changing routing.
    /// Active under `RUST_LOG=yserver::kms::backend=trace`.
    fn log_hit_test_diagnostic(&self) {
        if !log::log_enabled!(log::Level::Trace) {
            return;
        }
        let cx = self.cursor_x as f64;
        let cy = self.cursor_y as f64;
        let root_id = self.window_id;
        log::trace!("hit-test: cursor=({cx:.0},{cy:.0}) root_container=0x{root_id:x}");
        // Walk top-level stacking order from bottom (first painted) to
        // top (last painted) — same order as the compositor.
        let top_levels = self.top_level_order.clone();
        for tl in &top_levels {
            let w = &self.windows[tl];
            let hit = cx >= w.x as f64
                && cx < (w.x as f64 + w.width as f64)
                && cy >= w.y as f64
                && cy < (w.y as f64 + w.height as f64);
            log::trace!(
                "  top-level 0x{tl:x} mapped={} geo=({},{}, {}x{}) children={}{}",
                w.mapped,
                w.x,
                w.y,
                w.width,
                w.height,
                w.children.len(),
                if hit { " HIT" } else { "" }
            );
            if hit && w.mapped {
                self.log_descend_diagnostic(*tl, w.x as f64, w.y as f64, cx, cy, 2);
            }
        }
    }

    fn log_descend_diagnostic(
        &self,
        parent: u32,
        parent_origin_x: f64,
        parent_origin_y: f64,
        cx: f64,
        cy: f64,
        indent: usize,
    ) {
        let pad = " ".repeat(indent * 2);
        let Some(p) = self.windows.get(&parent) else {
            return;
        };
        for &child_id in &p.children {
            let Some(c) = self.windows.get(&child_id) else {
                continue;
            };
            let child_x = parent_origin_x + c.x as f64;
            let child_y = parent_origin_y + c.y as f64;
            let hit = cx >= child_x
                && cx < child_x + c.width as f64
                && cy >= child_y
                && cy < child_y + c.height as f64;
            log::trace!(
                "{pad}child 0x{child_id:x} mapped={} parent_rel=({},{}, {}x{}) abs=({},{}) children={}{}",
                c.mapped,
                c.x,
                c.y,
                c.width,
                c.height,
                child_x as i32,
                child_y as i32,
                c.children.len(),
                if hit { " HIT" } else { "" }
            );
            if hit && c.mapped {
                self.log_descend_diagnostic(child_id, child_x, child_y, cx, cy, indent + 1);
            }
        }
    }

    /// Apply X11 ConfigureWindow `stack_mode` to a window's position
    /// inside its parent's stacking list (or `top_level_order` if it's
    /// a top-level). Implements the common Above (0) / Below (1) modes;
    /// TopIf (2), BottomIf (3), Opposite (4) fall back to Above/Below
    /// without the conditional check (sufficient for fvwm/xterm popups).
    fn restack_window(&mut self, host_xid: u32, stack_mode: u8, sibling: Option<u32>) {
        let parent_xid = match self.windows.get(&host_xid).and_then(|w| w._parent) {
            Some(p) => p,
            None => return,
        };
        let stack: &mut Vec<u32> = if parent_xid == self.window_id {
            &mut self.top_level_order
        } else {
            match self.windows.get_mut(&parent_xid) {
                Some(p) => &mut p.children,
                None => return,
            }
        };
        // Remove the current entry; we'll reinsert at the right position.
        let Some(_pos) = stack.iter().position(|&x| x == host_xid) else {
            return;
        };
        stack.retain(|&x| x != host_xid);

        // Find sibling position if specified.
        let sibling_pos = sibling.and_then(|sib| stack.iter().position(|&x| x == sib));

        match stack_mode {
            // Above: place above sibling, or at top if no sibling.
            0 | 2 | 4 => {
                if let Some(sp) = sibling_pos {
                    stack.insert(sp + 1, host_xid);
                } else {
                    stack.push(host_xid);
                }
            }
            // Below: place below sibling, or at bottom if no sibling.
            1 | 3 => {
                if let Some(sp) = sibling_pos {
                    stack.insert(sp, host_xid);
                } else {
                    stack.insert(0, host_xid);
                }
            }
            _ => {
                stack.push(host_xid); // unknown mode → treat as Above
            }
        }
    }

    fn serialize_modifiers(&self) -> u16 {
        let state = &self.xkb_state.0;
        let flags = xkbcommon::xkb::STATE_MODS_EFFECTIVE;
        let mut mask: u16 = 0;
        if state.mod_name_is_active("Shift", flags) {
            mask |= 0x01;
        }
        if state.mod_name_is_active("Lock", flags) {
            mask |= 0x02;
        }
        if state.mod_name_is_active("Control", flags) {
            mask |= 0x04;
        }
        if state.mod_name_is_active("Mod1", flags) {
            mask |= 0x08;
        }
        if state.mod_name_is_active("Mod2", flags) {
            mask |= 0x10;
        }
        if state.mod_name_is_active("Mod3", flags) {
            mask |= 0x20;
        }
        if state.mod_name_is_active("Mod4", flags) {
            mask |= 0x40;
        }
        if state.mod_name_is_active("Mod5", flags) {
            mask |= 0x80;
        }
        mask
    }

    /// Take the libinput context out of `self`. The standalone yserver
    /// binary hands the context to `input_thread::run` so libinput
    /// dispatch happens off the core thread; events arrive at the
    /// backend through `Backend::on_host_input` instead.
    pub fn take_input_ctx(&mut self) -> Option<crate::input::SendContext> {
        self.input_ctx.take()
    }

    /// Update the local xkbcommon state from a libinput key event and
    /// build the cooked `HostKeyEvent` for fanout.
    ///
    /// `raw.keycode` is already the X11 keycode (evdev + 8) as supplied
    /// by the libinput thread. The xkb update needs the same value.
    fn cook_host_key(&mut self, raw: HostKeyEvent) -> HostKeyEvent {
        let xkb_keycode = xkbcommon::xkb::Keycode::new(u32::from(raw.keycode));
        let direction = if raw.pressed {
            xkbcommon::xkb::KeyDirection::Down
        } else {
            xkbcommon::xkb::KeyDirection::Up
        };
        self.xkb_state.0.update_key(xkb_keycode, direction);
        HostKeyEvent {
            state: self.serialize_modifiers(),
            root_x: self.cursor_x as i16,
            root_y: self.cursor_y as i16,
            event_x: self.cursor_x as i16,
            event_y: self.cursor_y as i16,
            time: Self::current_time_ms(),
            ..raw
        }
    }

    fn process_pointer_absolute(
        &mut self,
        server_state: &yserver_core::server::ServerState,
        x: f32,
        y: f32,
    ) {
        let new_x = x.clamp(0.0, self.fb_w as f32 - 1.0);
        let new_y = y.clamp(0.0, self.fb_h as f32 - 1.0);
        // When the hardware cursor plane is active, the kernel
        // positions the overlay independently of compositor cadence
        // — `move_cursor` is one ioctl per CRTC, microseconds, no
        // GPU touch. We do NOT dirty the screen in that case, so
        // pointer-only motion costs zero composite frames.
        //
        // Without the HW plane, the cursor is rendered as a quad in
        // the composite scene and every position change has to ride
        // the next vsync-paced composite+flip. `mark_all_outputs_dirty`
        // is the only thing that drives that next flip — the
        // `maybe_composite` in-flight gate caps the actual rate at
        // vsync, so many pointer events between flips just bump dirty
        // generations idempotently.
        if new_x != self.cursor_x || new_y != self.cursor_y {
            self.cursor_x = new_x;
            self.cursor_y = new_y;
            // Crossing into a window with a different `cursor`
            // attribute changes the effective cursor without any
            // explicit DefineCursor — refresh first so the HW plane
            // doesn't keep showing the previous window's cursor.
            // No-op (single hash lookup) when the cursor hasn't
            // changed.
            self.hw_cursor_refresh();
            if self.hw_cursor_active() {
                self.hw_cursor_move();
            } else {
                self.mark_all_outputs_dirty();
            }
        }
        self.dispatch_motion_event(server_state);
    }

    /// Compute event-window-relative coords for an event whose `host_xid`
    /// is the topmost mapped top-level under the cursor. Per X11 spec
    /// `event_x` / `event_y` are relative to the event window
    /// (`host_xid`); the host backend gets these from the X server, but
    /// on KMS we have to compute them by subtracting the top-level's
    /// origin from `cursor_x` / `cursor_y` (which are root-relative).
    fn event_relative_coords(&self, host_xid: u32) -> (i16, i16) {
        if let Some(w) = self.windows.get(&host_xid) {
            let ex = (self.cursor_x as i32) - (w.x as i32);
            let ey = (self.cursor_y as i32) - (w.y as i32);
            (
                ex.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                ey.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            )
        } else {
            // host_xid == 0 (no window under cursor) — fall back to root
            // coords; nested.rs treats event_x/y as a positional hint and
            // re-derives target coords from its own tree walk anyway.
            (self.cursor_x as i16, self.cursor_y as i16)
        }
    }

    fn emit_pointer(&mut self, ev: HostPointerEvent) {
        // Buffer; the input thread drains and dispatches outside the
        // backend lock. See the doc on `pending_pointer_events`.
        self.pending_pointer_events.push(ev);
    }

    fn current_time_ms() -> u32 {
        crate::clock::server_time_ms()
    }

    /// Synthesize an EnterNotify/LeaveNotify on `host_xid` with the
    /// given `detail` (NotifyAncestor / Virtual / Inferior / Nonlinear
    /// / NonlinearVirtual), `crossing_mode` (Normal=0 / Grab=1 /
    /// Ungrab=2), and `child` (immediate descendant on the path to
    /// source/destination for virtual intermediates; `ResourceId(0)` /
    /// X11 `None` for source/destination endpoints).
    fn emit_crossing(
        &mut self,
        host_xid: u32,
        kind: PointerEventKind,
        detail: u8,
        crossing_mode: u8,
        child: u32,
        state: u16,
    ) {
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        let ev = HostPointerEvent {
            kind,
            host_xid,
            detail,
            time: Self::current_time_ms(),
            root_x: self.cursor_x as i16,
            root_y: self.cursor_y as i16,
            event_x,
            event_y,
            state,
            crossing_mode,
            child,
        };
        self.emit_pointer(ev);
    }

    /// Spec-correct Normal-mode crossing chain for a top-level
    /// transition. Walks the window tree from the previous pointer
    /// window to the new one via [`crossings::normal_mode_crossings`]
    /// — generates Leave/Enter pairs with proper detail codes
    /// (NotifyInferior / NotifyVirtual / etc.) and child fields, so
    /// WMs that select Enter/Leave on root can tell "cursor went into
    /// a descendant" (NotifyInferior) from "cursor stayed on bare
    /// root" (no crossing at all) — required for e16's hover-popup
    /// gating.
    ///
    /// Falls back to a single Leave/Enter pair with detail=0 if either
    /// the previous or new host_xid isn't in `xid_map` — that case is
    /// the first-motion bootstrap before any top-level has been seen.
    fn update_pointer_window(
        &mut self,
        server_state: &yserver_core::server::ServerState,
        new_xid: u32,
        mask: u16,
    ) {
        if self.prev_pointer_window == Some(new_xid) {
            return;
        }

        let prev_host = self.prev_pointer_window;
        // The KMS root container (self.window_id) is yserver's own
        // top-level scaffolding window — never registered in xid_map
        // (only client-created windows are). When `window_under_cursor`
        // returns the root container, treat it as the X11 ROOT_WINDOW
        // for crossing-chain purposes so `update_pointer_window` can
        // emit a proper Leave-on-root with detail=NotifyInferior when
        // the cursor enters a top-level (the e16 hover-popup repro).
        let root_container_host = self.window_id;
        let resolve_host_to_nested = |host: u32, xid_map: &HostXidMap| -> Option<ResourceId> {
            if host == root_container_host {
                Some(yserver_core::resources::ROOT_WINDOW)
            } else {
                xid_map.get(&host).copied()
            }
        };
        let prev_id = prev_host.and_then(|p| resolve_host_to_nested(p, &self.xid_map));
        let new_id = resolve_host_to_nested(new_xid, &self.xid_map);

        if let (Some(from), Some(to)) = (prev_id, new_id) {
            let events = yserver_core::crossings::normal_mode_crossings(server_state, from, to);
            for ev in events {
                // ROOT_WINDOW has no host_xid recorded in server-state
                // (it's yserver's own scaffolding, not a client window),
                // so route Leave/Enter events on root to the KMS root
                // container host_xid (self.window_id). For all other
                // nested ResourceIds, look up the registered host_xid.
                let win_host_xid = if ev.window == yserver_core::resources::ROOT_WINDOW {
                    self.window_id
                } else {
                    server_state
                        .resources
                        .window(ev.window)
                        .and_then(|w| w.host_xid.map(|h| h.as_raw()))
                        .unwrap_or(new_xid)
                };
                let kind = match ev.kind {
                    yserver_core::crossings::CrossingKind::Enter => PointerEventKind::EnterNotify,
                    yserver_core::crossings::CrossingKind::Leave => PointerEventKind::LeaveNotify,
                };
                self.emit_crossing(win_host_xid, kind, ev.detail, 0, ev.child.0, mask);
            }
        } else {
            // First-motion bootstrap (no prev top-level recorded yet)
            // or unmapped host_xid — fall back to a single Leave/Enter
            // pair with detail=0. Less spec-correct but matches the
            // legacy behavior; doesn't regress anything that was
            // working before.
            if let Some(prev) = prev_host {
                self.emit_crossing(prev, PointerEventKind::LeaveNotify, 0, 0, 0, mask);
            }
            self.emit_crossing(new_xid, PointerEventKind::EnterNotify, 0, 0, 0, mask);
        }

        self.prev_pointer_window = Some(new_xid);
    }

    fn dispatch_motion_event(&mut self, server_state: &yserver_core::server::ServerState) {
        // Fall back to the root container so server.rs can deliver
        // to root-window subscribers (e16's right-click-desktop menu,
        // fvwm3's root bindings) when the cursor is over the
        // wallpaper / no top-level window.
        let host_xid = self.window_under_cursor().unwrap_or(self.window_id);
        // X11 KeyButMask: low byte modifiers, bits 8..=12 button1..button5.
        let mask = self.serialize_modifiers() | self.button_mask;
        self.update_pointer_window(server_state, host_xid, mask);
        self.emit_motion_only(host_xid, mask);
    }

    /// Emit a MotionNotify on `host_xid` without computing any
    /// crossing chain. Used by [`dispatch_motion_event`] (after it has
    /// called [`update_pointer_window`]) and by [`warp_pointer`]
    /// (which doesn't have access to `ServerState` from the Backend
    /// trait and so can't compute the chain today — warp-driven
    /// crossings are filed as a known followup).
    fn emit_motion_only(&mut self, host_xid: u32, mask: u16) {
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        let ev = HostPointerEvent {
            kind: PointerEventKind::MotionNotify,
            host_xid,
            detail: 0,
            time: Self::current_time_ms(),
            root_x: self.cursor_x as i16,
            root_y: self.cursor_y as i16,
            event_x,
            event_y,
            state: mask,
            crossing_mode: 0,
            child: 0,
        };
        self.emit_pointer(ev);
    }

    fn process_pointer_button(
        &mut self,
        code: u32,
        pressed: bool,
        server_state: &yserver_core::server::ServerState,
    ) {
        let detail = match code {
            0x110 => 1, // BTN_LEFT
            0x111 => 3, // BTN_RIGHT
            0x112 => 2, // BTN_MIDDLE
            0x113 => 8, // BTN_SIDE
            0x114 => 9, // BTN_EXTRA
            // yserver-synthetic scroll codes — see SYNTH_SCROLL_* in
            // yserver_core::core_loop::message. libinput emits scroll as
            // axis events; the libinput thread fans them out into press+
            // release pairs of these codes, mapped here to X11 buttons.
            0x180 => 4, // SYNTH_SCROLL_UP
            0x181 => 5, // SYNTH_SCROLL_DOWN
            0x182 => 6, // SYNTH_SCROLL_LEFT
            0x183 => 7, // SYNTH_SCROLL_RIGHT
            _ => {
                log::debug!("unmapped libinput button code 0x{code:x}, dropping");
                return;
            }
        };
        log::debug!("libinput button code=0x{code:x} pressed={pressed} → X11 detail={detail}");
        if pressed {
            self.log_hit_test_diagnostic();
        }
        // Fall back to the root container so server.rs can deliver
        // to root-window subscribers (e16's right-click-desktop menu,
        // fvwm3's root bindings) when the cursor is over the
        // wallpaper / no top-level window.
        let host_xid = self.window_under_cursor().unwrap_or(self.window_id);
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        // X11 KeyButMask: low byte modifier mask, bits 8..=12 for
        // ButtonNMask. Per X11 spec, the `state` field describes the
        // logical button state IMMEDIATELY BEFORE the event takes
        // effect, so:
        //   ButtonPress: button bit not yet set
        //   ButtonRelease: button bit still set
        //   MotionNotify: all currently-held buttons
        // Without these bits, fvwm's drag-detection on MotionNotify
        // sees state=0 and treats motion-during-press as idle motion.
        let button_bit = match detail {
            1 => 0x0100, // Button1Mask
            2 => 0x0200,
            3 => 0x0400,
            4 => 0x0800,
            5 => 0x1000,
            _ => 0,
        };
        let modifier_mask = self.serialize_modifiers();
        let state = if pressed {
            modifier_mask | self.button_mask
        } else {
            modifier_mask | self.button_mask | button_bit
        };
        // Update held-button state AFTER computing the event's `state`,
        // so subsequent motions see the new mask.
        if pressed {
            self.button_mask |= button_bit;
        } else {
            self.button_mask &= !button_bit;
        }
        let time = crate::clock::server_time_ms();
        let kind = if pressed {
            PointerEventKind::ButtonPress
        } else {
            PointerEventKind::ButtonRelease
        };
        let ptr_event = HostPointerEvent {
            kind,
            host_xid,
            detail,
            time,
            root_x: self.cursor_x as i16,
            root_y: self.cursor_y as i16,
            event_x,
            event_y,
            state,
            crossing_mode: 0,
            child: 0,
        };
        self.emit_pointer(ptr_event);
        // G3: spec-correct implicit-grab crossings. X11 protocol:
        // a press that creates an implicit pointer grab walks Leaves
        // up from the focus window to the deepest common ancestor of
        // focus + grab, then Enters down to the grab window; release
        // walks the symmetric Ungrab pairs back. Detail codes
        // (NotifyAncestor / NotifyVirtual / NotifyInferior /
        // NotifyNonlinear / NotifyNonlinearVirtual) come from the
        // pure helper `crossings::implicit_grab_crossings`. The
        // crossing-mode field marks the activation: 1 = NotifyGrab
        // on press, 2 = NotifyUngrab on release.
        let post_state = self.serialize_modifiers() | self.button_mask;
        let press_mode: u8 = if pressed { 1 } else { 2 };

        // Resolve focus + grab to nested ResourceIds via xid_map.
        let grab_id = self.xid_map.get(&host_xid).copied();
        let focus_id = self
            .prev_pointer_window
            .and_then(|prev| self.xid_map.get(&prev).copied());

        match (focus_id, grab_id) {
            (Some(focus), Some(grab)) => {
                let events =
                    yserver_core::crossings::implicit_grab_crossings(server_state, focus, grab);
                // When `focus == grab`, `implicit_grab_crossings`
                // returns empty by design — X11 spec section 11
                // (Input grab) treats the activation as a "warp"
                // from the focus window to the grab window, and a
                // warp to the same window emits no crossings. The
                // earlier code emitted a spurious Leave→Enter pair
                // here, which GTK's gesture-drag controller in
                // caja-desktop interpreted as "drag aborted; drag
                // restart from anchor (0,0)", producing a rubber-
                // band selection from screen origin on every click.
                for ev in events {
                    let win_host_xid = server_state
                        .resources
                        .window(ev.window)
                        .and_then(|w| w.host_xid.map(|h| h.as_raw()))
                        .unwrap_or(host_xid);
                    let kind = match ev.kind {
                        yserver_core::crossings::CrossingKind::Enter => {
                            PointerEventKind::EnterNotify
                        }
                        yserver_core::crossings::CrossingKind::Leave => {
                            PointerEventKind::LeaveNotify
                        }
                    };
                    self.emit_crossing(
                        win_host_xid,
                        kind,
                        ev.detail,
                        press_mode,
                        ev.child.0,
                        post_state,
                    );
                }
            }
            _ => {
                // Either focus or grab isn't a known nested window;
                // emit nothing. The empty-Vec interpretation above
                // is also what the unknown-id case approximates —
                // we can't compute the crossing chain without a
                // resolved focus/grab pair, and emitting a spurious
                // in-place pair was the bug we just removed.
            }
        }
    }

    /// Acquire a swapchain buffer per output, composite all visible windows
    /// onto it (translated into per-output scanout coordinates), draw the
    /// software cursor, and submit the flip. Called by the epoll loop on
    /// page-flip completion or on a timer.
    /// Allocate a per-window VkImage mirror if Vulkan is up. Best-
    /// effort: any failure logs and returns `None` so the window
    /// itself stays alive (drawing falls back to pixman-only for
    /// that window through 4.1.3-4.1.5; the user-visible effect is
    /// the same as today's PixmanShadow path).
    fn allocate_window_mirror(
        &self,
        width: u16,
        height: u16,
    ) -> Option<crate::kms::vk::target::DrawableImage> {
        let vkctx = self.vk.as_ref()?;
        if width == 0 || height == 0 {
            return None;
        }
        match crate::kms::vk::target::DrawableImage::new_server_owned_window(
            std::sync::Arc::clone(vkctx),
            u32::from(width),
            u32::from(height),
        ) {
            Ok(mut img) => {
                if let Some(pool) = self.ops_command_pool.as_ref() {
                    crate::vk_count!(init_clear_window);
                    if let Err(e) = img.initialize_clear(pool.handle()) {
                        log::warn!("window mirror initialize_clear failed: {e:?}");
                    }
                }
                Some(img)
            }
            Err(e) => {
                log::warn!(
                    "DrawableImage::new_server_owned_window({width}x{height}): {e} — \
                     window will run pixman-only"
                );
                None
            }
        }
    }

    /// Allocate a per-pixmap VkImage mirror. Same Option semantics
    /// as [`Self::allocate_window_mirror`].
    ///
    /// pixmap-pool T3: try the `PixmapPool` first; on hit we
    /// reconstruct a `DrawableImage` from the recycled triple and
    /// skip `initialize_clear` (the first paint overwrites the whole
    /// image). On miss, fall through to the fresh-allocation path.
    fn allocate_pixmap_mirror(
        &self,
        width: u32,
        height: u32,
        depth: u8,
    ) -> Option<crate::kms::vk::target::DrawableImage> {
        let vkctx = self.vk.as_ref()?;
        if width == 0 || height == 0 {
            return None;
        }

        // Pool keys on (width, height, format); derive format from
        // depth via the shared helper so this never drifts from
        // `new_server_owned_pixmap`'s mapping.
        let format = crate::kms::vk::target::DrawableImage::format_for_pixmap_depth(depth);
        let key = crate::kms::vk::pixmap_pool::PixmapPoolKey {
            width,
            height,
            format,
        };
        if let Some(pool) = self.pixmap_pool.as_ref()
            && let Some(entry) = pool.try_take(key)
        {
            // Pool hit: construct DrawableImage from the entry, then
            // clear it. X11 clients (xfdesktop's thumbnail tiles, for
            // one) rely on the Xorg/Xephyr convention of zero-filled
            // new pixmaps — the spec says "undefined" but every real
            // server zeroes, and clients composite smaller content
            // onto the tile expecting the rest to be transparent.
            // Skipping clear leaked the previous tenant's pixels
            // through wherever the first paint didn't cover.
            let mut img = crate::kms::vk::target::DrawableImage::new_from_pool(
                std::sync::Arc::clone(vkctx),
                entry,
                format,
                ash::vk::Extent2D { width, height },
            );
            if let Some(cmd_pool) = self.ops_command_pool.as_ref() {
                crate::vk_count!(init_clear_pixmap);
                if let Err(e) = img.initialize_clear(cmd_pool.handle()) {
                    log::warn!("pooled pixmap initialize_clear failed: {e:?}");
                }
            }
            return Some(img);
        }

        // Pool miss — fall through to fresh allocation.
        match crate::kms::vk::target::DrawableImage::new_server_owned_pixmap(
            std::sync::Arc::clone(vkctx),
            width,
            height,
            depth,
        ) {
            Ok(mut img) => {
                if let Some(pool) = self.ops_command_pool.as_ref() {
                    crate::vk_count!(init_clear_pixmap);
                    if let Err(e) = img.initialize_clear(pool.handle()) {
                        log::warn!("pixmap mirror initialize_clear failed: {e:?}");
                    }
                }
                Some(img)
            }
            Err(e) => {
                log::warn!(
                    "DrawableImage::new_server_owned_pixmap({width}x{height} d{depth}): \
                     {e} — pixmap will run pixman-only"
                );
                None
            }
        }
    }

    // `run_mirror_uploads_for_frame` (the pixman → mirror pump)
    // deleted in 4.1.5. Mirrors are now the canonical store; nothing
    // pumps into them from the host.

    /// Bump dirty_gen on every output. Used by call sites that
    /// don't yet know which outputs were affected (any
    /// non-window-scoped change). Phase 1: equivalent in blast
    /// radius to the old global boolean per-screen flag. Phase 2+
    /// can narrow producers that have window/region context.
    fn mark_all_outputs_dirty(&mut self) {
        for layout in &mut self.outputs {
            layout.damage.bump_dirty();
        }
    }

    /// Bump dirty on every output whose rect intersects `old` ∪ `new`.
    /// Use this from geometry-change call sites (configure, restack,
    /// reparent, unmap, destroy, map). `old == new` is fine — bumps
    /// only the intersecting outputs. An empty rect (w=0 or h=0)
    /// overlaps nothing and is safely ignored.
    fn mark_window_dirty_with_old_rect(&mut self, old: Rect, new: Rect) {
        for layout in &mut self.outputs {
            let lr = layout.rect();
            let lr_x2 = lr.x.saturating_add(lr.w);
            let lr_y2 = lr.y.saturating_add(lr.h);
            let old_overlaps = old.w > 0
                && old.h > 0
                && old.x < lr_x2
                && lr.x < old.x.saturating_add(old.w)
                && old.y < lr_y2
                && lr.y < old.y.saturating_add(old.h);
            let new_overlaps = new.w > 0
                && new.h > 0
                && new.x < lr_x2
                && lr.x < new.x.saturating_add(new.w)
                && new.y < lr_y2
                && lr.y < new.y.saturating_add(new.h);
            if old_overlaps || new_overlaps {
                layout.damage.bump_dirty();
            }
        }
    }

    /// Non-blocking poll of the in-flight queue. Marks GPU- and
    /// scanout-retirement bits and drains fully-retired frames.
    /// Called at the top of `composite_and_flip` and from the
    /// pageflip-complete handler.
    ///
    /// Uses index-based access via `InFlight::get_mut` rather than
    /// `frames_mut()`: the loop body reads `self.vk` and
    /// `self.scanout_pools`, which can't coexist with a held
    /// `&mut self.scheduler.in_flight` borrow. The two-pass pattern
    /// (snapshot via get_mut, compute, write back via get_mut)
    /// avoids the borrow split.
    fn poll_in_flight(&mut self) {
        // 4-T3: drain any signaled paint batches first. Paint
        // batches retire independently of output frames — they
        // can signal even when no composite is in flight (e.g.,
        // a ProtocolBarrier flush in the middle of a paint cycle
        // that completes before the next composite).
        if let Err(e) = self.scheduler.poll_retired_paint_batches() {
            log::error!(
                "poll_in_flight: paint-batch retirement poll failed: {e:?}; \
                 latching renderer_failed"
            );
            self.renderer_failed = true;
            // Continue with output-frame polling — the latched
            // renderer_failed flag stops new paint work, but
            // existing scanout frames still need their pageflip
            // tracking to complete cleanly.
        }

        let n = self.scheduler.in_flight.len();
        for i in 0..n {
            // Pass 1: snapshot the polling inputs.
            let (composite_fence, output_idx, bo_slot, gpu_done, scanout_done) = {
                let f = self.scheduler.in_flight.get_mut(i).unwrap();
                (
                    f.output_frame.composite_fence,
                    f.output_frame.output_idx,
                    f.output_frame.bo_slot,
                    f.gpu_retired,
                    f.scanout_retired,
                )
            };

            // Compute the new bools outside the borrow.
            //
            // GPU retirement. Phase 1: null fence is the "already
            // retired" sentinel (vkQueueWaitIdle inside the submit
            // path drained the queue). Phase 4 replaces the null
            // with a real signalled-by-submit fence or timeline
            // value, and this branch becomes a true non-blocking
            // status check.
            let new_gpu = if gpu_done || composite_fence == ash::vk::Fence::null() {
                // Already retired or null-sentinel: no real fence to check.
                true
            } else if let Some(vk) = self.vk.as_ref() {
                // SAFETY: composite_fence was created by `vk.device`
                // and remains valid as long as VK is initialised; the
                // `if let Some(vk)` guard establishes that VK is not
                // torn down. Non-blocking status query.
                let status = unsafe { vk.device.get_fence_status(composite_fence) };
                matches!(status, Ok(true))
            } else {
                gpu_done
            };

            // Scanout retirement. The BoPhase machine in `vk/scanout.rs`
            // transitions the BO to Free on the pageflip-complete event.
            // A frame whose bo_slot is `None` (no-VK test path) is
            // trivially scanout-retired.
            let new_scanout = if scanout_done {
                true
            } else {
                self.scanout_pools
                    .get(output_idx)
                    .and_then(|p| p.as_ref())
                    .and_then(|p| bo_slot.and_then(|s| p.bos.get(s)))
                    .map(|b| matches!(b.state.phase, crate::kms::vk::scanout::BoPhase::Free))
                    .unwrap_or(true)
            };

            // Pass 2: write back.
            let f = self.scheduler.in_flight.get_mut(i).unwrap();
            let prev_gpu = f.gpu_retired;
            f.gpu_retired = new_gpu;
            f.scanout_retired = new_scanout;
            if !prev_gpu && new_gpu && composite_fence != ash::vk::Fence::null() {
                log::trace!(
                    "in_flight: gpu_retired (fence) frame_id={} output_idx={}",
                    f.output_frame.frame_id,
                    f.output_frame.output_idx,
                );
            }
        }

        // Release pool slots for EVERY fully-retired frame whose slot
        // is not yet released — independent of FIFO drain order. The
        // earlier `take_while(|f| f.fully_retired())` shape blocked
        // pool release on the head of the in-flight queue: with two
        // outputs, one lagging frame on output A could hold pool slots
        // hostage for already-retired frames on output B, exhausting
        // the per-output CompositePoolRing → composite frames deferred
        // → black screen. The fix: walk all frames, release each
        // retired-and-not-yet-released frame's slot, set
        // pool_released=true. drain_retired() stays FIFO for the
        // broader frame lifecycle (other resources are layered on
        // submission order, see InFlight::push doc).
        //
        // (Diagnosed by codex 2026-05-13 from the MATE-on-3E log
        // showing "vk composite: deferred frames ... pool_ring_exhausted"
        // mounting up while nothing else fails.)
        let to_release: Vec<(usize, usize, usize)> = self
            .scheduler
            .in_flight
            .frames()
            .enumerate()
            .filter(|(_, f)| f.fully_retired() && !f.pool_released)
            .map(|(idx, f)| {
                (
                    idx,
                    f.output_frame.output_idx,
                    f.output_frame.composite_pool_slot,
                )
            })
            .collect();
        for (frame_idx, output_idx, pool_slot) in to_release {
            let ring = self
                .outputs
                .get_mut(output_idx)
                .and_then(|o| o.composite_pools.as_mut());
            debug_assert!(
                ring.is_some(),
                "in-flight frame for output {output_idx} has no composite_pools ring \
                 (submitted frames should always have one)"
            );
            if let Some(ring) = ring {
                ring.release(pool_slot);
                if let Some(f) = self.scheduler.in_flight.get_mut(frame_idx) {
                    f.pool_released = true;
                }
            }
        }

        let drained = self.scheduler.in_flight.drain_retired();
        if drained > 0 {
            log::trace!("in_flight: drained {} fully-retired frame(s)", drained);
        }
    }

    /// True iff any output has unpresented damage and no pending flip.
    fn any_output_needs_composite(&self) -> bool {
        self.outputs.iter().any(|l| l.damage.needs_composite())
    }

    pub fn composite_and_flip(&mut self) -> io::Result<()> {
        if self.renderer_failed || self.shutting_down {
            // Renderer is in fatal state or backend is shutting down;
            // skip paint+composite. The backend is alive enough to
            // drain pageflip-completes and process input — clients
            // still see the X server, they just see the last good
            // frame on screen.
            return Ok(());
        }

        // Phase 4.1.5: pixman no longer feeds the mirrors; drawing
        // ops fill them directly through Vk. The pre-composite
        // upload pass is gone.

        // Poll the in-flight queue non-blocking. Marks GPU- and
        // scanout-retirement bits and drains any fully-retired frames
        // before we decide whether any output needs compositing.
        self.poll_in_flight();

        if let Some(summary) = self.composite_defer_stats.maybe_flush() {
            log::info!("{summary}");
        }

        // Skip the whole pass when no output has unpresented damage.
        // Producers (request handlers, host input, host-X11 fanout)
        // call `mark_dirty` / `mark_all_outputs_dirty` to bump dirty
        // generations; per-output state is advanced by `record_submit`
        // (on flip submit) and `record_present` (on pageflip-complete),
        // so an idle server costs zero composite work per vsync.
        if !self.any_output_needs_composite() {
            return Ok(());
        }

        // No Vulkan / no ops pool → composite path is unavailable.
        let vk_arc = match self.vk.as_ref() {
            Some(v) => v.clone(),
            None => {
                log::debug!("composite cycle: no Vulkan; skipping");
                return Ok(());
            }
        };
        let pool_handle = match self.ops_command_pool.as_ref() {
            Some(p) => p.handle(),
            None => {
                log::debug!("composite cycle: no ops pool; skipping");
                return Ok(());
            }
        };
        let frame_id = self.scheduler.open_batch(vk_arc, pool_handle);
        log::debug!(
            "composite cycle frame_id={} in_flight_len={}",
            frame_id,
            self.scheduler.in_flight.len()
        );

        let top_levels: Vec<u32> = self.top_level_order.clone();

        // Pre-filter visible top-levels per output (spec §2.5: avoid
        // descending whole off-screen subtrees).
        let visible_per_output: Vec<Vec<u32>> = self
            .outputs
            .iter()
            .map(|layout| {
                let bbox = layout.rect();
                top_levels
                    .iter()
                    .copied()
                    .filter(|&id| self.window_intersects(id, bbox))
                    .collect()
            })
            .collect();

        // Flush paint BEFORE the per-output composite loop. Until
        // 3B starts migrating recorders, the batch is Idle on every
        // cycle and this is a cheap state transition.
        if let Err(e) = self
            .flush_if_needed(crate::kms::scheduler::paint_batch::BatchFlushReason::VisibleComposite)
        {
            // flush_if_needed already latched renderer_failed and
            // logged the underlying Vk error. Propagate to the
            // event loop; future composite ticks early-return at
            // the top of this function via the renderer_failed gate.
            return Err(std::io::Error::other(format!(
                "PaintBatch::submit_and_wait failed: {e:?}"
            )));
        }

        // Audit assertion (debug-only): no batch may be open during
        // the per-output composite loop. Recorders that would
        // auto-open one here would race with composite (the F2
        // finding from codex's review).
        debug_assert!(
            self.scheduler.current_batch_state().is_none(),
            "paint batch leaked into composite loop"
        );

        #[allow(clippy::needless_range_loop)] // index needed to split &mut/& borrows on self
        for layout_idx in 0..self.outputs.len() {
            let visible = &visible_per_output[layout_idx];

            // Per-output dirty check: skip outputs that have nothing
            // new to present. A producer that bumped while a flip was
            // pending will have dirty_gen > last_presented_gen; an
            // output that was already caught up has nothing to do.
            if !self.outputs[layout_idx].damage.needs_composite() {
                continue;
            }

            // Skip any output whose previous atomic flip hasn't yet
            // completed (no pageflip-complete event for it yet).
            // Submitting a new flip on a CRTC that already has one
            // pending returns -EBUSY from the kernel, and on the
            // PixmanShadow path that cascades into bo state drift —
            // the next pageflip-complete arrives for the *old* flip
            // (different bo), so our state machine and the kernel's
            // view of "what's on screen" diverge. Better to skip the
            // frame; the next pageflip-complete will retrigger
            // composite_and_flip and we'll catch up cleanly.
            //
            // Note: post-T7, the per-output `needs_composite()` check
            // above already returns false when `damage.flip_pending`
            // is true, so under normal operation this BO-phase check
            // is a defense-in-depth guard against damage/BoPhase
            // divergence rather than the primary gate.
            let vk_flip_pending = self
                .scanout_pools
                .get(layout_idx)
                .and_then(|p| p.as_ref())
                .map(|p| {
                    p.bos.iter().any(|b| {
                        matches!(
                            b.state.phase,
                            crate::kms::vk::scanout::BoPhase::Submitted
                                | crate::kms::vk::scanout::BoPhase::Pending
                        )
                    })
                })
                .unwrap_or(false);
            let dumb_flip_pending = self.outputs[layout_idx].swapchain.submitted_idx().is_some();
            // Invariant: a skipped output keeps its dirty state.
            // Enforced upstream by `OutputDamageState::record_submit`
            // / `record_present` debug_asserts (one signals flip-
            // pending, the other clears it) — nothing in this loop
            // body can clear `dirty_gen` between the early-continue
            // above and the skip below. A dedicated assert here
            // would be vacuous given that ordering; left as a
            // comment.
            if vk_flip_pending || dumb_flip_pending {
                log::debug!(
                    "composite: skip output {} (vk_flip_pending={} dumb_flip_pending={})",
                    self.outputs[layout_idx].output.connector_name,
                    vk_flip_pending,
                    dumb_flip_pending
                );
                continue;
            }

            // Phase 4.1.5: Vk composite is the sole path. If a free
            // bo isn't available the frame is skipped — no pixman
            // fallback. The next pageflip-complete event will retrigger
            // composite_and_flip.
            let Some((bo_idx, pool_slot)) = self.try_vulkan_composite_flip(layout_idx, visible)
            else {
                log::debug!(
                    "composite: deferring frame on output {} until a Free bo is available",
                    self.outputs[layout_idx].output.connector_name
                );
                continue;
            };
            // record_submit was added in task 7; keep it here.
            self.outputs[layout_idx].damage.record_submit();
            log::debug!(
                "composite: submitted flip on output {} (visible={}, submitted_gen={})",
                self.outputs[layout_idx].output.connector_name,
                visible.len(),
                self.outputs[layout_idx].damage.last_submitted_gen(),
            );

            // Push an InFlightFrame for this successful submit.
            //
            // Phase-1 retirement is "two-stage shaped," not truly two-stage yet.
            // The GPU side uses a *placeholder* `vk::Fence::null()` — `null` is
            // treated as "already GPU-retired" in `poll_in_flight`. This is
            // correct in phase 1 because:
            //   - Paint ops drain the queue via `run_one_shot_op` (vk/ops/mod.rs)
            //     BEFORE composite submits, so by the time `record_submit` runs
            //     there is no in-flight paint work on the queue.
            //   - The composite submit itself goes through KMS with IN_FENCE_FD,
            //     and we don't touch the targeted BO until `BoPhase::Free` after
            //     pageflip-complete — so scanout retirement transitively implies
            //     GPU completion.
            // Phase 4 swaps `null` for a real signalled-by-composite-submit
            // fence (or a timeline counter) and removes the hot-path waitIdle.
            let submitted_gen = self.outputs[layout_idx].damage.last_submitted_gen();
            self.scheduler
                .in_flight
                .push(crate::kms::scheduler::in_flight::InFlightFrame {
                    output_frame: crate::kms::scheduler::output_frame::OutputFrame::new(
                        layout_idx,
                        frame_id,
                        submitted_gen,
                        Some(bo_idx),
                        pool_slot, // real pool slot now, not placeholder
                        ash::vk::Fence::null(),
                    ),
                    gpu_retired: false,
                    scanout_retired: false,
                    pool_released: false,
                });
        }

        Ok(())
    }

    /// Lazy-init the composite pool ring for this output. Returns
    /// `None` if Vulkan or the compositor pipeline isn't up
    /// (test path).
    fn ensure_composite_pools(
        &mut self,
        layout_idx: usize,
    ) -> Option<&mut crate::kms::scheduler::composite_pool_ring::CompositePoolRing> {
        if self.outputs[layout_idx].composite_pools.is_none() {
            let vk = self.vk.as_ref()?.clone();
            match crate::kms::scheduler::composite_pool_ring::CompositePoolRing::new(
                vk,
                crate::kms::vk::pipeline::MAX_DESCRIPTOR_SETS_PER_FRAME,
            ) {
                Ok(ring) => {
                    self.outputs[layout_idx].composite_pools = Some(ring);
                }
                Err(e) => {
                    log::warn!(
                        "composite: failed to create descriptor pool ring for output {}: {e:?}",
                        self.outputs[layout_idx].output.connector_name
                    );
                    return None;
                }
            }
        }
        self.outputs[layout_idx].composite_pools.as_mut()
    }

    /// VkComposite path (sub-phase 4.1.3.4): build a
    /// [`CompositeScene`] from the window tree, pick a Free
    /// `ScanoutBo`, record the per-window quad-draw composite pass,
    /// submit, atomic-flip with explicit fences. Returns
    /// `Some((bo_idx, pool_slot))` with the scanout BO index and the
    /// descriptor pool slot acquired from the per-output ring, or
    /// `None` if the composite + flip did not happen (no free BO, no
    /// Vulkan, ring exhausted, or error).
    fn try_vulkan_composite_flip(
        &mut self,
        layout_idx: usize,
        visible: &[u32],
    ) -> Option<(usize, usize)> {
        if self.renderer_failed || self.shutting_down {
            return None;
        }
        use crate::kms::vk::{compositor, scanout::BoPhase};

        // Clone the Arc immediately so the immutable self.vk borrow
        // doesn't extend across the mutable ensure_composite_pools
        // call below.
        let vkctx = self.vk.as_ref()?.clone();
        self.compositor_pipeline.as_ref()?; // existence check; borrow drops here

        // Read-only check for a Free BO. Mutable re-borrow happens
        // later (for record_and_present_composite).
        let bo_idx = {
            let pool = self
                .scanout_pools
                .get(layout_idx)
                .and_then(|p| p.as_ref())?;
            pool.bos.iter().position(|b| b.state.phase == BoPhase::Free)
        };
        let Some(bo_idx) = bo_idx else {
            let name = &self.outputs[layout_idx].output.connector_name;
            log::debug!("vk composite: no Free bo in pool for output {name} — deferring frame");
            self.composite_defer_stats
                .note(CompositeDeferKind::NoFreeBo, name);
            return None;
        };

        // Acquire a descriptor pool slot from this output's ring.
        let pool_slot = {
            let ring = self.ensure_composite_pools(layout_idx)?;
            ring.acquire()
        };
        let Some(pool_slot) = pool_slot else {
            let name = &self.outputs[layout_idx].output.connector_name;
            log::debug!(
                "vk composite: descriptor pool ring exhausted for output {name} — deferring frame"
            );
            self.composite_defer_stats
                .note(CompositeDeferKind::PoolRingExhausted, name);
            return None;
        };

        let descriptor_pool = self.outputs[layout_idx]
            .composite_pools
            .as_ref()
            .expect("ensure_composite_pools just succeeded")
            .pool_at(pool_slot);

        let scene = self.build_composite_scene(layout_idx, visible);

        // Take the pipeline reference here; it's independent of
        // self.scanout_pools and self.outputs[idx].
        let pipeline = self.compositor_pipeline.as_ref()?;
        let pool_mut = self
            .scanout_pools
            .get_mut(layout_idx)
            .and_then(|p| p.as_mut())?;
        let bo = &mut pool_mut.bos[bo_idx];
        let result = compositor::record_and_present_composite(
            &vkctx,
            &self.device,
            &self.outputs[layout_idx].output,
            bo,
            pipeline,
            descriptor_pool,
            &scene,
        );

        match result {
            Ok(()) => Some((bo_idx, pool_slot)),
            Err(e) => {
                log::warn!(
                    "vk composite: record_and_present_composite failed on output {}: {e} \
                     — skipping frame",
                    self.outputs[layout_idx].output.connector_name
                );
                // Error-path pool release. `record_and_present_composite`
                // can fail BEFORE or AFTER `vkQueueSubmit2` (see
                // `vk/compositor.rs` — pre-submit: layout / fb / record
                // errors; post-submit: `export_signaled_fd` /
                // `submit_flip_with_fences`). Post-submit, the GPU may
                // still be reading descriptor sets allocated from
                // `pool_slot`. Resetting the pool then would invalidate
                // sets in active use — phase-4-unsafe.
                //
                // Conservative fix: drain the queue before releasing.
                // Atomic-commit rejection is the typical trigger and is
                // rare; a `queue_wait_idle` here is acceptable. The hot
                // path (Ok branch) is unchanged.
                unsafe {
                    let _ = vkctx.device.queue_wait_idle(vkctx.graphics_queue);
                }
                if let Some(ring) = self.outputs[layout_idx].composite_pools.as_mut() {
                    ring.release(pool_slot);
                }
                None
            }
        }
    }

    /// Walk the window tree (in stacking order, depth-first
    /// back-to-front) and build a flat list of quads to draw,
    /// translated into per-output scanout coordinates. Cursor +
    /// bg_pixmap are deferred to follow-up sub-tasks; for 4.1.3.4's
    /// first commit, only the bg color clear + window mirrors are
    /// rendered. Visible regression: no cursor, no wallpaper —
    /// follow-up commits add them back.
    fn build_composite_scene(
        &self,
        layout_idx: usize,
        visible: &[u32],
    ) -> crate::kms::vk::compositor::CompositeScene {
        use crate::kms::vk::compositor::{CompositeDraw, CompositeScene};

        let layout_x = self.outputs[layout_idx].x;
        let layout_y = self.outputs[layout_idx].y;

        // Background clear color: bg_pixel (X11 0xRRGGBB) → linear
        // [r, g, b, a]. Falls back to mid-grey so unset roots stand
        // out, mirroring `paint_output`'s default.
        let bg = self.bg_pixel.unwrap_or(0x0050_5050);
        let bg_color = [
            ((bg >> 16) & 0xFF) as f32 / 255.0,
            ((bg >> 8) & 0xFF) as f32 / 255.0,
            (bg & 0xFF) as f32 / 255.0,
            1.0,
        ];

        let mut draws: Vec<CompositeDraw> = Vec::new();
        let output_w = i32::from(self.outputs[layout_idx].width);
        let output_h = i32::from(self.outputs[layout_idx].height);

        // Background pixmap (e.g. Esetroot wallpaper) draws first
        // so windows paint on top. The pixmap is sized to the
        // virtual-screen extent (fb_w × fb_h); each output samples
        // the slice that corresponds to its layout offset. If the
        // client frees the pixmap (Esetroot pattern), the mirror
        // disappears with it; the rescue path will move with the
        // picture_rescued_images cleanup.
        if let Some(pm) = self.bg_pixmap
            && let Some(pm_state) = self.pixmaps.get(&pm.as_raw())
            && let Some(mirror) = pm_state.vk_mirror.as_ref()
        {
            let layout_w = self.outputs[layout_idx].width;
            let layout_h = self.outputs[layout_idx].height;
            let pm_w = mirror.extent.width as f32;
            let pm_h = mirror.extent.height as f32;
            if pm_w > 0.0 && pm_h > 0.0 {
                draws.push(CompositeDraw {
                    image_view: mirror.vk_image_view,
                    dst_origin: [0.0, 0.0],
                    dst_size: [f32::from(layout_w), f32::from(layout_h)],
                    src_origin: [layout_x as f32 / pm_w, layout_y as f32 / pm_h],
                    src_size: [f32::from(layout_w) / pm_w, f32::from(layout_h) / pm_h],
                    alpha_passthrough: false,
                });
            }
        }

        for &top in visible {
            self.walk_subtree_into_draws(top, -layout_x, -layout_y, output_w, output_h, &mut draws);
        }

        // Cursor draws above all windows. Pixman implicitly clipped
        // off-screen writes, so multi-output cursor crossing edges
        // showed its visible portion on each scanout. Vulkan
        // viewport+scissor implicitly clips the same way, so a
        // straight quad at scanout-relative coords reproduces it.
        //
        // Skip when the DRM hardware cursor plane is active — the
        // kernel overlay draws on top of the scanout independently
        // of this composite pass. Without the gate the cursor
        // double-draws (HW overlay + compositor quad), with the
        // GPU quad trailing by one composite cadence.
        if !self.hw_cursor_active()
            && let Some(cursor_xid) = self.effective_cursor()
            && let Some(cs) = self.cursors.get(&cursor_xid)
            && let Some(mirror) = cs.vk_mirror.as_ref()
        {
            let cw = cs.extent.width as f32;
            let ch = cs.extent.height as f32;
            let cx = self.cursor_x as i32 - i32::from(cs.hot_x) - layout_x;
            let cy = self.cursor_y as i32 - i32::from(cs.hot_y) - layout_y;
            draws.push(CompositeDraw {
                image_view: mirror.vk_image_view,
                dst_origin: [cx as f32, cy as f32],
                dst_size: [cw, ch],
                src_origin: [0.0, 0.0],
                src_size: [1.0, 1.0],
                alpha_passthrough: true,
            });
        }

        CompositeScene { bg_color, draws }
    }

    /// Push quads for `window` and (recursively, in stacking order)
    /// each of its mapped descendants. Translation by `(ox, oy)` is
    /// applied at the top-level so children's parent-relative
    /// coordinates accumulate through the recursion without each
    /// frame computing absolute origins via `absolute_origin`.
    ///
    /// `output_w`/`output_h` carry the destination scanout extent —
    /// any window whose absolute rect doesn't intersect
    /// `[0, output_w) × [0, output_h)` is skipped (AABB cull,
    /// design §"Frame composite pass" step 2). Recursion into
    /// children continues regardless: in current X11/pixman
    /// semantics a child can overflow its parent's bounding box, so
    /// a culled parent doesn't necessarily imply culled descendants.
    fn walk_subtree_into_draws(
        &self,
        window_id: u32,
        ox: i32,
        oy: i32,
        output_w: i32,
        output_h: i32,
        out: &mut Vec<crate::kms::vk::compositor::CompositeDraw>,
    ) {
        use crate::kms::vk::compositor::CompositeDraw;
        let Some(window) = self.windows.get(&window_id) else {
            log::debug!("walk diag: skip xid={window_id:#x} (not in windows map)");
            return;
        };
        if !window.mapped {
            log::debug!("walk diag: skip xid={window_id:#x} (unmapped)");
            return;
        }
        // Skip windows with an explicitly-empty SHAPE region (rare).
        if self
            .shape_bounding
            .get(&window_id)
            .is_some_and(|r| r.is_empty())
        {
            log::debug!("walk diag: skip xid={window_id:#x} (shape_bounding empty)");
            return;
        }
        let abs_x = i32::from(window.x) + ox;
        let abs_y = i32::from(window.y) + oy;
        let w = i32::from(window.width);
        let h = i32::from(window.height);

        // AABB cull: skip the push when the window's absolute rect
        // lies entirely outside the output. Children still recurse.
        let on_screen = w > 0
            && h > 0
            && abs_x + w > 0
            && abs_y + h > 0
            && abs_x < output_w
            && abs_y < output_h;

        if on_screen && let Some(mirror) = window.vk_mirror.as_ref() {
            // SHAPE bounding region cuts the window's quad. Without a
            // bounding entry we draw the whole mirror; with one we
            // emit one quad per visible rect (in window-local coords)
            // so pixels outside the shape simply aren't drawn —
            // whatever was behind shows through. Fixes black-corner
            // artifacts on e16 popups / WM frames whose mirror has
            // bg-fill content under un-shaped corners.
            let push_rect = |out: &mut Vec<CompositeDraw>, rx: i32, ry: i32, rw: i32, rh: i32| {
                if rw <= 0 || rh <= 0 {
                    return;
                }
                let dx0 = abs_x + rx;
                let dy0 = abs_y + ry;
                if dx0 + rw <= 0 || dy0 + rh <= 0 || dx0 >= output_w || dy0 >= output_h {
                    return;
                }
                let inv_w = 1.0 / w as f32;
                let inv_h = 1.0 / h as f32;
                out.push(CompositeDraw {
                    image_view: mirror.vk_image_view,
                    dst_origin: [dx0 as f32, dy0 as f32],
                    dst_size: [rw as f32, rh as f32],
                    src_origin: [rx as f32 * inv_w, ry as f32 * inv_h],
                    src_size: [rw as f32 * inv_w, rh as f32 * inv_h],
                    // L1 task A.16: window-mirror draws pass α
                    // through. Every paint path (A.3..A.15) now
                    // lands α=0xFF on depth-24 painted pixels, so
                    // pass-through reveals only what's been
                    // painted — unpainted regions stay
                    // transparent and the composite scene's
                    // src-over blend exposes the bg-pixmap (or
                    // the prior frame) underneath, eliminating
                    // the marco black-rim regression.
                    alpha_passthrough: true,
                });
            };
            match self.shape_bounding.get(&window_id) {
                None => push_rect(out, 0, 0, w, h),
                Some(rects) => {
                    for r in rects {
                        push_rect(
                            out,
                            i32::from(r.x),
                            i32::from(r.y),
                            i32::from(r.width),
                            i32::from(r.height),
                        );
                    }
                }
            }
        }
        // Children draw above their parent in stacking order.
        for &child_id in &window.children {
            self.walk_subtree_into_draws(child_id, abs_x, abs_y, output_w, output_h, out);
        }
    }

    /// Paint a single output's scanout image. Translates virtual-screen
    /// coordinates by `(-layout.x, -layout.y)` so layout `(layout.x,
    /// layout.y)` lands at scanout `(0, 0)`. Pixman implicitly clips writes
    /// outside the destination image.
    /// Return whether the (mapped) top-level window's bounding box overlaps
    /// `rect` in virtual-screen coordinates. Used by the per-output painter
    /// to skip whole off-screen subtrees.
    fn window_intersects(&self, window_id: u32, rect: Rect) -> bool {
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };
        if !window.mapped {
            return false;
        }
        let (ox, oy) = self.absolute_origin(window_id);
        // absolute_origin returns the top-left in virtual-screen coords; for a
        // top-level the parent is the root, so this equals (window.x, window.y).
        #[allow(clippy::cast_possible_truncation)]
        let wx = ox as i32;
        #[allow(clippy::cast_possible_truncation)]
        let wy = oy as i32;
        let wx2 = wx.saturating_add(i32::from(window.width));
        let wy2 = wy.saturating_add(i32::from(window.height));
        let bx2 = rect.x.saturating_add(rect.w);
        let by2 = rect.y.saturating_add(rect.h);
        wx < bx2 && rect.x < wx2 && wy < by2 && rect.y < wy2
    }

    /// Resolve the effective cursor for the window currently under the
    /// pointer. Walks the window's parent chain looking for the
    /// closest non-None (non-zero) cursor attribute. Falls back to
    /// `self.active_cursor` (the root container's cursor) and finally
    /// to `None` (no cursor drawn).
    fn effective_cursor(&self) -> Option<u32> {
        // Start at the deepest window the pointer is inside, then walk
        // up. window_under_cursor returns a top-level; we descend into
        // children using the current cursor coordinates.
        let mut current = self.window_under_cursor();
        let cx = self.cursor_x as f64;
        let cy = self.cursor_y as f64;
        if let Some(top) = current {
            // Walk down to deepest descendant containing cursor.
            current = Some(self.descend_for_cursor(top, cx, cy));
        }
        let mut node = current;
        while let Some(xid) = node {
            if let Some(w) = self.windows.get(&xid) {
                if w.cursor != 0 {
                    return Some(w.cursor);
                }
                node = w._parent;
                continue;
            }
            break;
        }
        self.active_cursor
    }

    fn descend_for_cursor(&self, window: u32, cx: f64, cy: f64) -> u32 {
        let Some(w) = self.windows.get(&window) else {
            return window;
        };
        // Build absolute origin by walking up.
        let (ox, oy) = self.absolute_origin(window);
        for &child_id in w.children.iter().rev() {
            let Some(c) = self.windows.get(&child_id) else {
                continue;
            };
            if !c.mapped {
                continue;
            }
            let cx0 = ox + c.x as f64;
            let cy0 = oy + c.y as f64;
            if cx >= cx0 && cx < cx0 + c.width as f64 && cy >= cy0 && cy < cy0 + c.height as f64 {
                return self.descend_for_cursor(child_id, cx, cy);
            }
        }
        window
    }

    fn absolute_origin(&self, window: u32) -> (f64, f64) {
        let mut ox = 0.0;
        let mut oy = 0.0;
        let mut node = Some(window);
        while let Some(xid) = node {
            if let Some(w) = self.windows.get(&xid) {
                ox += w.x as f64;
                oy += w.y as f64;
                node = w._parent.filter(|p| *p != self.window_id);
            } else {
                break;
            }
        }
        (ox, oy)
    }

    /// Render a string of character bytes onto a drawable using the current font.
    /// Each byte is treated as a character index into the loaded font.
    fn render_text_string(
        &mut self,
        host_xid: u32,
        foreground: u32,
        x: i32,
        y: i32,
        text: &[u8],
    ) -> io::Result<()> {
        let chars: Vec<char> = text.iter().map(|&b| b as char).collect();
        self.render_text_chars(host_xid, foreground, x, y, &chars)
    }

    fn render_text_chars(
        &mut self,
        host_xid: u32,
        foreground: u32,
        x: i32,
        y: i32,
        text: &[char],
    ) -> io::Result<()> {
        let Some(font_xid) = self.current_font else {
            return Ok(());
        };

        // Phase 1: render all glyphs into owned pixel buffers while holding
        // the RefCell borrow.  We must drop the borrow before phase 2 so that
        // with_image_mut (which requires &mut self) can be called.
        let mut rendered: Vec<RenderedGlyph> = Vec::new();
        let mut cursor_x = x;

        {
            let Some(fs) = self.fonts.get(&font_xid) else {
                return Ok(());
            };
            let face = fs.face.borrow();
            let char_cache = &fs.char_info_cache;

            for &ch in text {
                let Some(ci) = char_cache.get(&ch) else {
                    cursor_x += 6;
                    continue;
                };

                let _ = face
                    .0
                    .load_char(ch as usize, freetype::face::LoadFlag::RENDER);
                let glyph = face.0.glyph();
                let bitmap = glyph.bitmap();

                if bitmap.width() > 0 && bitmap.rows() > 0 {
                    let w = bitmap.width() as usize;
                    let h = bitmap.rows() as usize;
                    let stride = bitmap.pitch();
                    let buf = bitmap.buffer();

                    let mut pixels = vec![0u8; w * h];
                    for row in 0..h {
                        let src = if stride >= 0 {
                            row * stride as usize
                        } else {
                            (h - 1 - row) * (stride as isize).unsigned_abs()
                        };
                        pixels[row * w..row * w + w].copy_from_slice(&buf[src..src + w]);
                    }

                    rendered.push(RenderedGlyph {
                        dst_x: cursor_x + glyph.bitmap_left(),
                        dst_y: y - glyph.bitmap_top(),
                        w,
                        h,
                        pixels,
                        advance: ci.character_width as i32,
                        codepoint: ch as u32,
                    });
                }
                cursor_x += ci.character_width as i32;
            }
        } // RefCell borrow released here

        // Phase 4.1.5: Vulkan-only text path. Each glyph is interned
        // in the shared atlas; a text-pipeline draw quads them onto
        // the target's mirror with src-over blending. Atlas full or
        // missing mirror silently drops the run.
        self.try_vk_text_run(host_xid, font_xid, foreground, &rendered);
        Ok(())
    }

    /// Return the scanout framebuffer dimensions.
    pub fn fb_dimensions(&self) -> (u16, u16) {
        (self.fb_w, self.fb_h)
    }

    /// Build the RANDR output records for every connected DRM output.
    ///
    /// ID allocation per spec §2.6.1:
    /// - outputs `1..=N`
    /// - CRTCs `(N+1)..=2N`
    /// - modes `2N+1..` deduped by `(width, height, vrefresh)`
    #[must_use]
    pub fn randr_outputs(&self) -> Vec<yserver_core::randr::RandrOutput> {
        use yserver_core::randr::RandrOutput;
        let n = self.outputs.len();
        let mut mode_ids: HashMap<(u16, u16, u32), u32> = HashMap::new();
        #[allow(clippy::cast_possible_truncation)]
        let mut next_mode_id: u32 = (2 * n + 1) as u32;
        self.outputs
            .iter()
            .enumerate()
            .map(|(i, layout)| {
                let vrefresh = layout.output.picked.vrefresh;
                let key = (layout.width, layout.height, vrefresh);
                let mode_id = *mode_ids.entry(key).or_insert_with(|| {
                    let id = next_mode_id;
                    next_mode_id += 1;
                    id
                });
                #[allow(clippy::cast_possible_truncation)]
                let output_id = (i + 1) as u32;
                #[allow(clippy::cast_possible_truncation)]
                let crtc_id = (n + i + 1) as u32;
                RandrOutput {
                    name: layout.output.connector_name.clone(),
                    output_id,
                    crtc_id,
                    mode_id,
                    x: i16::try_from(layout.x).unwrap_or(i16::MAX),
                    y: i16::try_from(layout.y).unwrap_or(i16::MAX),
                    width: layout.width,
                    height: layout.height,
                    vrefresh,
                }
            })
            .collect()
    }

    /// Return the raw libinput fd for epoll registration, if available.
    pub fn input_fd(&self) -> Option<std::os::unix::io::RawFd> {
        self.input_ctx.as_ref().map(|ctx| ctx.fd())
    }

    /// Return the DRM device fd for epoll registration.
    pub fn drm_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::fd::{AsFd, AsRawFd};
        self.device.as_fd().as_raw_fd()
    }

    /// Drain pending page-flip events, advance per-output state, and
    /// kick off the next composite. Each pageflip-complete corresponds
    /// to either a Vulkan-fed flip (advance the [`ScanoutBoPool`] state
    /// machine: `Retiring → Free`, `OnScreen → Retiring`, `Pending →
    /// OnScreen`) or a dumb-buffer flip (advance the legacy
    /// `Swapchain::complete`). We disambiguate by checking whether the
    /// pool currently has a `Pending` bo.
    pub fn drain_page_flips_and_composite(&mut self) -> io::Result<()> {
        use ::drm::control::crtc;
        let mut flipped: Vec<crtc::Handle> = Vec::new();
        drm::page_flip::drain_events(&self.device, |c| flipped.push(c))?;

        // Log every pageflip-complete at debug; first one per output at
        // info so quiet runs still show the cycle started.
        for c in &flipped {
            if let Some(idx) = self.outputs.iter().position(|o| &o.output.crtc == c) {
                if !self.first_pageflip_logged[idx] {
                    log::info!(
                        "pageflip-complete: first event on output {} (CRTC {c:?})",
                        self.outputs[idx].output.connector_name
                    );
                    self.first_pageflip_logged[idx] = true;
                } else {
                    log::debug!(
                        "pageflip-complete on output {}",
                        self.outputs[idx].output.connector_name
                    );
                }
            }
        }

        for c in flipped {
            let Some(output_idx) = self.outputs.iter().position(|o| o.output.crtc == c) else {
                log::warn!("page-flip event for unknown CRTC {c:?}");
                continue;
            };

            // Vulkan-fed flip identification: the pool has a `Pending`
            // bo iff we submitted via the Vulkan path for this output.
            let was_vk_flip = self
                .scanout_pools
                .get(output_idx)
                .and_then(|p| p.as_ref())
                .map(|p| {
                    p.bos
                        .iter()
                        .any(|b| b.state.phase == crate::kms::vk::scanout::BoPhase::Pending)
                })
                .unwrap_or(false);

            if was_vk_flip {
                if let Some(pool) = self
                    .scanout_pools
                    .get_mut(output_idx)
                    .and_then(|p| p.as_mut())
                {
                    advance_pool_on_pageflip_complete(pool);
                }
            } else {
                let layout = &mut self.outputs[output_idx];
                if let Some(idx) = layout.swapchain.submitted_idx() {
                    layout
                        .swapchain
                        .complete(idx)
                        .map_err(|e| io::Error::other(format!("swapchain.complete: {e}")))?;
                }
            }

            // Advance per-output damage state: the flip completed, so
            // last_presented_gen catches up to last_submitted_gen.
            // After this, needs_composite() returns false until a
            // producer bumps dirty_gen again.
            self.outputs[output_idx].damage.record_present();
        }
        // Poll in-flight queue after all flip completions are processed.
        // Pageflip-complete transitions BOs to Free (via
        // advance_pool_on_pageflip_complete above), so scanout retirement
        // is now observable.
        self.poll_in_flight();
        // Always composite on flip completion (self-driving at vsync)
        self.composite_and_flip()
    }

    /// Diagnostic: dump the current scanout BO contents to a PPM file
    /// in the process's cwd (`./yserver-scanout-N.ppm`, N
    /// auto-incremented). Triggered by SIGUSR1. Picks the bo currently
    /// in `OnScreen` (preferred) or `Pending` / `Submitted` /
    /// `Recording` (fallbacks) phase — i.e. whatever is closest to
    /// "what's on the monitor right now".
    pub fn do_dump_scanout(&mut self) -> io::Result<()> {
        use crate::kms::vk::scanout::BoPhase;
        use std::sync::atomic::{AtomicU32, Ordering};

        let Some(vk) = self.vk.as_ref().cloned() else {
            return Err(io::Error::other("no vulkan context"));
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return Err(io::Error::other("no ops command pool"));
        };

        // Pick the best-available bo *per pool* — each pool corresponds
        // to one DRM output (CRTC), so a multi-monitor setup wants one
        // dump per pool. Within a pool prefer "currently on screen"
        // and fall through to recording / submitted so we can still
        // dump mid-pageflip-pending.
        let preferred = [
            BoPhase::OnScreen,
            BoPhase::Pending,
            BoPhase::Submitted,
            BoPhase::Recording,
        ];
        let mut chosen: Vec<(usize, usize)> = Vec::new();
        for (pi, pool) in self.scanout_pools.iter().enumerate() {
            let Some(pool) = pool.as_ref() else {
                continue;
            };
            for phase in preferred {
                if let Some(bi) = pool.bos.iter().position(|b| b.state.phase == phase) {
                    chosen.push((pi, bi));
                    break;
                }
            }
        }
        if chosen.is_empty() {
            return Err(io::Error::other("no non-Free scanout bo found"));
        }

        // Shared run counter so concurrent SIGUSR1 dumps don't clobber.
        static DUMP_COUNT: AtomicU32 = AtomicU32::new(0);
        let n = DUMP_COUNT.fetch_add(1, Ordering::Relaxed);

        let mut last_err: Option<io::Error> = None;
        let mut wrote_any = false;
        for (pool_idx, bo_idx) in chosen {
            match self.dump_scanout_one(&vk, pool_handle, pool_idx, bo_idx, n) {
                Ok(()) => wrote_any = true,
                Err(e) => {
                    log::warn!("do_dump_scanout: pool {pool_idx} failed: {e}");
                    last_err = Some(e);
                }
            }
        }
        if !wrote_any {
            return Err(last_err.unwrap_or_else(|| io::Error::other("scanout dump failed")));
        }
        Ok(())
    }

    fn dump_scanout_one(
        &mut self,
        vk: &std::sync::Arc<crate::kms::vk::device::VkContext>,
        pool_handle: ash::vk::CommandPool,
        pool_idx: usize,
        bo_idx: usize,
        run: u32,
    ) -> io::Result<()> {
        use crate::kms::vk::ops::run_one_shot_op;

        let pool = self.scanout_pools[pool_idx].as_mut().unwrap();
        let bo = &mut pool.bos[bo_idx];
        let width = bo.width;
        let height = bo.height;
        let pitch = bo.pitch;
        let image = bo.vk_image;
        let staging_buffer = bo.vk_transfer.staging_buffer;
        let staging_mapped = bo.vk_transfer.staging_mapped;
        let staging_size = bo.vk_transfer.staging_size;

        // Record image→staging copy via the chain. Source layout is
        // `COLOR_ATTACHMENT_OPTIMAL` (composite leaves it there) or
        // sometimes `PRESENT_SRC_KHR` after a flip — use GENERAL on
        // the dst side which is permissive enough not to fight either.
        // run_one_shot_op submits and waits via a per-op VkFence
        // (5-T1), so when it returns the staging buffer is
        // host-coherent and ready to read.
        let run_result = run_one_shot_op(vk, pool_handle, |vk, cb| {
            let pre = [ash::vk::ImageMemoryBarrier2::default()
                .src_stage_mask(ash::vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(ash::vk::AccessFlags2::MEMORY_WRITE)
                .dst_stage_mask(ash::vk::PipelineStageFlags2::COPY)
                .dst_access_mask(ash::vk::AccessFlags2::TRANSFER_READ)
                .old_layout(ash::vk::ImageLayout::GENERAL)
                .new_layout(ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .image(image)
                .subresource_range(
                    ash::vk::ImageSubresourceRange::default()
                        .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )];
            let pre_dep = ash::vk::DependencyInfo::default().image_memory_barriers(&pre);
            crate::vk_count!(cmd_pipeline_barrier2);
            unsafe { vk.device.cmd_pipeline_barrier2(cb, &pre_dep) };

            let region = [ash::vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(
                    ash::vk::ImageSubresourceLayers::default()
                        .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_offset(ash::vk::Offset3D::default())
                .image_extent(ash::vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })];
            unsafe {
                crate::vk_count!(cmd_copy_image_to_buffer);
                vk.device.cmd_copy_image_to_buffer(
                    cb,
                    image,
                    ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    staging_buffer,
                    &region,
                );
            }

            let post = [ash::vk::ImageMemoryBarrier2::default()
                .src_stage_mask(ash::vk::PipelineStageFlags2::COPY)
                .src_access_mask(ash::vk::AccessFlags2::TRANSFER_READ)
                .dst_stage_mask(ash::vk::PipelineStageFlags2::ALL_COMMANDS)
                .dst_access_mask(ash::vk::AccessFlags2::MEMORY_WRITE)
                .old_layout(ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(ash::vk::ImageLayout::GENERAL)
                .image(image)
                .subresource_range(
                    ash::vk::ImageSubresourceRange::default()
                        .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )];
            let post_dep = ash::vk::DependencyInfo::default().image_memory_barriers(&post);
            crate::vk_count!(cmd_pipeline_barrier2);
            unsafe { vk.device.cmd_pipeline_barrier2(cb, &post_dep) };
            Ok(())
        });
        if let Err(e) = run_result {
            log::error!(
                "dump_scanout_one: run_one_shot_op returned fatal {e:?}; \
                 latching renderer_failed — KMS renderer disabled until restart"
            );
            self.renderer_failed = true;
            return Err(io::Error::other(format!("scanout copy submit: {e:?}")));
        }

        let path = format!("./yserver-scanout-{run}-out{pool_idx}.ppm");

        let raw =
            unsafe { std::slice::from_raw_parts(staging_mapped.as_ptr(), staging_size as usize) };

        use std::io::Write;
        let mut file = std::fs::File::create(&path)?;
        file.write_all(format!("P6\n{width} {height}\n255\n").as_bytes())?;
        let mut row_buf = vec![0u8; (width * 3) as usize];
        for y in 0..height as usize {
            let row_start = y * pitch as usize;
            for x in 0..width as usize {
                let pi = row_start + x * 4;
                // BO format is XRGB8888 → memory bytes (LE) BGRX. PPM
                // wants RGB.
                let b = raw[pi];
                let g = raw[pi + 1];
                let r = raw[pi + 2];
                let dst = x * 3;
                row_buf[dst] = r;
                row_buf[dst + 1] = g;
                row_buf[dst + 2] = b;
            }
            file.write_all(&row_buf)?;
        }
        log::info!(
            "do_dump_scanout: wrote {path} ({width}x{height}, bo phase {:?})",
            self.scanout_pools[pool_idx].as_ref().unwrap().bos[bo_idx]
                .state
                .phase
        );
        Ok(())
    }

    /// Shutdown step 4 (per docs/known-issues.md "P0: KMS teardown..."):
    /// drain DRM pageflip-complete events until no scanout BO is in
    /// `BoPhase::Pending` (i.e., the kernel has finished honouring
    /// every flip we submitted before shutdown began). The atomic
    /// `disable_output` commit in step 5 only succeeds when KMS has
    /// no in-flight flips on the connector.
    ///
    /// Polls the DRM fd with `nix::poll::poll` (POLLIN, 50 ms timeout
    /// per iteration), drains via the existing
    /// `drm::page_flip::drain_events`, and re-checks. Bails after
    /// `MAX_WAIT_MS` total elapsed with a warn log — at that point
    /// something is genuinely stuck and proceeding to the atomic
    /// disable is the least-bad option (it may still fail, but we
    /// avoid hanging the shutdown path indefinitely).
    ///
    /// Caller MUST NOT have called `ScanoutBoPool::drain_all_pending`
    /// before this — that force-resets BO state to Free and would
    /// make `has_pending_pageflip` lie.
    fn drain_pending_pageflips_for_shutdown(&mut self) -> io::Result<()> {
        use ::drm::control::crtc;
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
        use std::{os::fd::AsFd, time::Instant};

        const POLL_INTERVAL_MS: u8 = 50;
        const MAX_WAIT_MS: u128 = 500;

        let started = Instant::now();
        loop {
            let any_pending = self
                .scanout_pools
                .iter()
                .filter_map(|p| p.as_ref())
                .any(|p| p.has_pending_pageflip());
            if !any_pending {
                return Ok(());
            }
            if started.elapsed().as_millis() >= MAX_WAIT_MS {
                log::warn!(
                    "shutdown: drain_pending_pageflips_for_shutdown timed out after {MAX_WAIT_MS} ms; \
                     proceeding to atomic disable_output anyway (it may fail)"
                );
                return Ok(());
            }

            // Wait for the DRM fd to have an event ready. Crucial:
            // `drm::Device::receive_events()` is a blocking `read()`,
            // so we MUST only call drain_events when poll reports
            // POLLIN. A bare `Ok(_)` would include Ok(0) (timeout, no
            // readiness) and a subsequent blocking read could hang
            // past the 500 ms ceiling.
            let fd_borrow = self.device.as_fd();
            let mut fds = [PollFd::new(fd_borrow, PollFlags::POLLIN)];
            let timeout = PollTimeout::from(POLL_INTERVAL_MS);
            let ready = match poll(&mut fds, timeout) {
                Ok(0) => false,
                Ok(_) => fds[0]
                    .revents()
                    .map(|r| r.contains(PollFlags::POLLIN))
                    .unwrap_or(false),
                Err(nix::errno::Errno::EINTR) => false,
                Err(e) => {
                    log::warn!("shutdown: drain_pending_pageflips_for_shutdown poll failed: {e}");
                    return Ok(());
                }
            };
            if !ready {
                continue;
            }

            // Drain whatever events are available; transition Pending → OnScreen
            // for each completing CRTC. Re-uses the existing per-event handler
            // from drain_page_flips_and_composite, but without the composite-and-flip
            // tail-call (shutting_down gates that out anyway).
            let mut flipped: Vec<crtc::Handle> = Vec::new();
            if let Err(e) = drm::page_flip::drain_events(&self.device, |c| flipped.push(c)) {
                log::warn!("shutdown: drain_events failed: {e}");
                return Ok(());
            }
            for c in flipped {
                let Some(output_idx) = self.outputs.iter().position(|o| o.output.crtc == c) else {
                    continue;
                };
                if let Some(pool) = self
                    .scanout_pools
                    .get_mut(output_idx)
                    .and_then(|p| p.as_mut())
                {
                    advance_pool_on_pageflip_complete(pool);
                }
                // Keep damage state internally consistent during the
                // bounded shutdown loop (codex review nit). Not load-
                // bearing for process exit; cheap.
                self.outputs[output_idx].damage.record_present();
            }
        }
    }

    /// Disarm every scanout BO in the given pool index so its `Drop`
    /// becomes a no-op. Called for outputs whose atomic
    /// `disable_output` failed — KMS may still hold the FB, so
    /// user-side `destroy_framebuffer` from `ScanoutBo::Drop` would
    /// produce the `atomic remove_fb failed with -22` kernel WARN
    /// that strands Wayland host sessions. Process-exit DRM-fd close
    /// reaps the GEM/FB; VkDevice teardown releases the userspace
    /// handles. This Drop deliberately skips per-object cleanup
    /// (vkDestroyImage, destroy_framebuffer, close_buffer, etc.) —
    /// it's a deliberate last-resort leak, not a normal teardown.
    fn disarm_scanout_pool(&mut self, output_idx: usize) {
        let Some(pool) = self
            .scanout_pools
            .get_mut(output_idx)
            .and_then(|p| p.as_mut())
        else {
            return;
        };
        for bo in &mut pool.bos {
            bo.disarm();
        }
    }

    /// Disable each DRM output (CRTC + plane) for clean shutdown.
    /// Logs any per-output error and returns the last one so callers
    /// still see a failure, while attempting to tear down everything.
    pub fn disable_output(&mut self) -> io::Result<()> {
        // 6-step teardown per codex pinpoint (docs/known-issues.md
        // "P0: KMS teardown..."). The previous implementation
        // collapsed steps 3+4 by force-resetting BO state via
        // drain_all_pending BEFORE the atomic disable, which lied
        // BOs to Free while KMS still had FBs bound → atomic disable
        // EINVAL → kernel `atomic remove_fb failed with -22` warning
        // and Wayland host compositors saw no outputs.

        // Step 1: Stop submitting new composites. composite_and_flip
        // and try_vulkan_composite_flip both early-return when this
        // is true.
        self.shutting_down = true;

        // Step 2: Flush + retire the open PaintBatch (best-effort —
        // BatchFlushReason::Shutdown is a non-strict reason; failures
        // are logged but don't abort the rest of shutdown).
        if let Err(e) =
            self.flush_if_needed(crate::kms::scheduler::paint_batch::BatchFlushReason::Shutdown)
        {
            log::warn!("shutdown: PaintBatch flush failed: {e:?}");
        }

        // Step 3: Wait for any submitted Vulkan work to complete.
        // After this, GPU is idle; KMS may still have pageflips
        // pending in its own queue, but no new Vk submits can race
        // shutdown (step 1 stopped them) and no in-flight Vk work
        // can race the upcoming atomic commit.
        if let Some(vk) = self.vk.as_ref()
            && let Err(e) = unsafe { vk.device.device_wait_idle() }
        {
            log::warn!("shutdown: vkDeviceWaitIdle: {e}");
        }

        // 4-T5: drain any paint batches that didn't finish
        // retiring through the composite-tick poll. After
        // vkDeviceWaitIdle their fences are signaled, so
        // each wait_for_completion returns immediately —
        // we're just running the CB-free + resource-release
        // + fence-destroy sequence on the host side.
        if let Err(e) = self.scheduler.drain_submitted_paint_batches() {
            log::warn!(
                "shutdown: drain_submitted_paint_batches failed ({e:?}); \
                 remaining batches will fire the leak warning on Drop"
            );
        }

        // pixmap-pool T4: every PooledPixmapReturn BatchResource has
        // fired by now (scheduler drain walked retire_resources). The
        // pool's buckets hold entries to destroy. PixmapPool::Drop is
        // the defensive fallback if this path is skipped.
        if let Some(pool) = self.pixmap_pool.as_ref() {
            let strong = Arc::strong_count(pool);
            if strong > 1 {
                log::warn!(
                    "shutdown: PixmapPool strong_count={strong} > 1 at drain time; \
                     a BatchResource may be leaking past scheduler drain"
                );
            }
            pool.drain();
        }

        // Step 4: Drain DRM pageflip completions per output until no
        // bo is in BoPhase::Pending. Bounded by a 500 ms ceiling so a
        // genuinely stuck kernel doesn't hang shutdown. DO NOT
        // force-reset BO state here — has_pending_pageflip must
        // observe the real KMS state, not a userspace lie.
        if let Err(e) = self.drain_pending_pageflips_for_shutdown() {
            log::warn!("shutdown: drain_pending_pageflips: {e}");
        }

        // Step 5: Now safe to issue the atomic disable_output per
        // output. With no Pending flips, the kernel will accept the
        // commit instead of returning EINVAL. Track per-output
        // success so step 6 can skip force-reset on outputs whose
        // disable failed (those still have KMS bindings).
        let mut last_err: Option<io::Error> = None;
        let mut disable_ok: Vec<bool> = Vec::with_capacity(self.outputs.len());
        for layout in &self.outputs {
            match drm::modeset::disable_output(&self.device, &layout.output) {
                Ok(()) => disable_ok.push(true),
                Err(e) => {
                    log::warn!(
                        "disable_output failed for {}: {e}",
                        layout.output.connector_name
                    );
                    disable_ok.push(false);
                    last_err = Some(e);
                }
            }
        }

        // Step 6: For each output whose atomic disable succeeded, KMS
        // has released its hold on the framebuffer; force-resetting
        // BO state (close any straggler fence fds) is now safe and
        // RAII drops the scanout pool when KmsBackend itself drops.
        //
        // For an output whose disable FAILED, KMS may still hold the
        // FB. Plain "skip drain_all_pending" is NOT enough — when
        // KmsBackend drops shortly after this returns, scanout_pools
        // drops, which drops each ScanoutBo, whose Drop unconditionally
        // calls destroy_framebuffer + close_buffer(gem) (see
        // scanout.rs Drop impl). That's the exact RMFB-while-KMS-holds-FB
        // path that produces `atomic remove_fb failed with -22` and
        // strands Wayland host sessions. So for failed outputs we
        // `disarm_scanout_pool(idx)` — each ScanoutBo's `disarmed`
        // flag flips to true, and its Drop becomes a no-op. The
        // resources leak until DRM-fd close at process exit, at which
        // point the kernel reaps GEM + FB as part of its normal
        // device-fd cleanup. Vulkan resources leak similarly via Vk
        // device drop. Codex flagged this in the v2 review.
        for (idx, success) in disable_ok.iter().copied().enumerate() {
            if success {
                if let Some(vk) = self.vk.as_ref()
                    && let Some(p) = self.scanout_pools.get_mut(idx).and_then(|p| p.as_mut())
                {
                    p.drain_all_pending(vk);
                }
            } else {
                // Disarm BOTH the Vulkan scanout pool AND the dumb
                // swapchain buffers for this output. Either or both
                // may have submitted FBs that KMS still holds after
                // the failed atomic disable. Letting either Drop run
                // its normal teardown (destroy_framebuffer + GEM
                // close for ScanoutBo; destroy_framebuffer + munmap
                // + destroy_dumb_buffer for Buffer) reintroduces the
                // `atomic remove_fb failed with -22` UAF that the
                // 6-step sequence is meant to prevent. Codex review
                // of T2 caught the swapchain gap.
                self.disarm_scanout_pool(idx);
                if let Some(layout) = self.outputs.get_mut(idx) {
                    layout.swapchain.disarm();
                }
            }
        }

        last_err.map_or(Ok(()), Err)
    }
}

/// XLFD glob match per X11 ListFonts semantics: `*` matches zero or more
/// characters (including `-`), `?` matches exactly one. Comparison is
/// ASCII case-insensitive — clients legitimately mix case (`-R-` for
/// roman slant against our lowercase `-r-` names).
fn xlfd_pattern_matches(pattern: &str, name: &str) -> bool {
    let pat = pattern.as_bytes();
    let s = name.as_bytes();
    let mut pi = 0usize;
    let mut si = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_si: usize = 0;
    while si < s.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi].eq_ignore_ascii_case(&s[si])) {
            pi += 1;
            si += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            star_pi = Some(pi);
            star_si = si;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// Map a fontconfig integer `weight` (`FC_WEIGHT_*`) to the X11 XLFD
/// weight name. Buckets follow the canonical fontconfig→XLFD mapping
/// used by Xft and traditional X font path synthesis.
fn xlfd_weight(w: i32) -> &'static str {
    match w {
        ..=49 => "thin",
        50..=74 => "light",
        75..=89 => "book",
        90..=139 => "medium",
        140..=189 => "demibold",
        190..=209 => "bold",
        _ => "black",
    }
}

/// Map a fontconfig integer `slant` to the single-letter XLFD slant.
fn xlfd_slant(s: i32) -> &'static str {
    match s {
        100 => "i", // italic
        110 => "o", // oblique
        _ => "r",   // roman
    }
}

/// Map a fontconfig integer `spacing` to the single-letter XLFD spacing.
fn xlfd_spacing(s: i32) -> &'static str {
    match s {
        100 => "m", // monospaced
        110 => "c", // charcell
        _ => "p",   // proportional
    }
}

/// Make a fontconfig string safe to drop into a single XLFD field:
/// dashes are field separators in XLFDs, so any embedded `-` is
/// replaced with a space. XLFD is case-insensitive but conventionally
/// lowercase.
fn sanitize_xlfd_field(s: &str) -> String {
    s.replace('-', " ").to_lowercase()
}

/// Enumerate the installed font set via fontconfig and synthesise one
/// XLFD per (face × pixel-size × charset) combination. Every entry is
/// a real font that `FontLoader::open_font` can resolve back to a
/// `FreeType::Face` — there are no stub XLFDs.
///
/// Charsets are limited to `iso8859-1` and `iso10646-1` because those
/// two are the universal subset every scalable font supports through
/// FreeType's char-by-char lookup. Locale-specific charsets (jisx*,
/// gb2312*, ksc*) need real fonts on disk to satisfy properly; rather
/// than stub them, we let libXt warn "Missing charset X" and proceed
/// with the iso* coverage that's guaranteed real.
fn build_font_catalog(fc: &fontconfig::Fontconfig) -> Vec<String> {
    // Pixel sizes that scalable fonts can satisfy and that Xt clients
    // commonly request. Pointsize is `pixel * 10` (X11 convention:
    // pointsize is decipoints).
    const PIXEL_SIZES: &[u32] = &[8, 10, 12, 14, 16, 18, 24];
    const CHARSETS: &[&str] = &["iso8859-1", "iso10646-1"];

    let pat = fontconfig::Pattern::new(fc);
    let mut objs = fontconfig::ObjectSet::new(fc);
    objs.add(fontconfig::FC_FAMILY);
    objs.add(fontconfig::FC_FOUNDRY);
    objs.add(fontconfig::FC_WEIGHT);
    objs.add(fontconfig::FC_SLANT);
    objs.add(fontconfig::FC_SPACING);
    let set = fontconfig::list_fonts(&pat, Some(&objs));

    // Aliases the loader handles directly without an XLFD parse pass.
    let mut entries: Vec<String> = vec!["fixed".into(), "cursor".into(), "nil2".into()];
    let mut seen: HashSet<(String, String, i32, i32, i32)> = HashSet::new();

    for font in set.iter() {
        let Some(family) = font.get_string(fontconfig::FC_FAMILY) else {
            continue;
        };
        let foundry = font.get_string(fontconfig::FC_FOUNDRY).unwrap_or("misc");
        let weight = font.get_int(fontconfig::FC_WEIGHT).unwrap_or(80);
        let slant = font.get_int(fontconfig::FC_SLANT).unwrap_or(0);
        let spacing = font.get_int(fontconfig::FC_SPACING).unwrap_or(0);

        let key = (
            family.to_lowercase(),
            foundry.to_lowercase(),
            weight,
            slant,
            spacing,
        );
        if !seen.insert(key) {
            continue;
        }

        let foundry_x = sanitize_xlfd_field(foundry);
        let family_x = sanitize_xlfd_field(family);
        let weight_x = xlfd_weight(weight);
        let slant_x = xlfd_slant(slant);
        let spacing_x = xlfd_spacing(spacing);

        for &px in PIXEL_SIZES {
            // Average width estimate: ~0.6 × pixel size, in 1/10 px.
            // Approximation only — clients filter by name pattern, not
            // by this field, and QueryFont returns the real widths.
            let avg_width = (px * 6).clamp(1, 999);
            for &charset in CHARSETS {
                entries.push(format!(
                    "-{foundry_x}-{family_x}-{weight_x}-{slant_x}-normal--{px}-{}-75-75-{spacing_x}-{avg_width}-{charset}",
                    px * 10,
                ));
            }
        }
    }

    entries
}

impl Backend for KmsBackend {
    fn window_id(&self) -> u32 {
        self.window_id
    }

    fn dri3_open(&mut self, _drawable: u32) -> io::Result<std::os::fd::OwnedFd> {
        // Open a fresh fd at the render-node path per client. dup()'ing
        // a shared long-lived fd would give every client the same
        // kernel struct file, and libdrm_amdgpu maintains GEM handles
        // + contexts in per-struct-file state — the first client
        // populates it, the second crashes in `amdgpu_winsys_create`
        // hitting leftover handles. Xorg's `glamor_dri3_open_client`
        // does the same fresh-open dance for the same reason.
        let path = self.render_node_path.as_deref().ok_or_else(|| {
            io::Error::other("DRI3 unavailable — render node was not resolved at backend init")
        })?;
        crate::kms::render_node::open_fresh(path)
            .map_err(|e| io::Error::other(format!("open render-node {}: {e}", path.display())))
    }

    fn dri3_import_pixmap(
        &mut self,
        fd: std::os::fd::OwnedFd,
        width: u16,
        height: u16,
        stride: u32,
        offset: u32,
        modifier: u64,
        depth: u8,
        bpp: u8,
    ) -> io::Result<yserver_core::backend::PixmapHandle> {
        // Per design §3.2: import the dma-buf into a DrawableImage via
        // VK_EXT_image_drm_format_modifier and stash on a fresh
        // PixmapState. Pixmap exists as a real X resource — clients
        // can CopyArea / ChangePicture against it.
        let Some(vk) = self.vk.clone() else {
            return Err(io::Error::other("DRI3 import: Vulkan unavailable"));
        };
        let format = match (depth, bpp) {
            (24, 32) | (32, 32) => ash::vk::Format::B8G8R8A8_UNORM,
            _ => {
                return Err(io::Error::other(format!(
                    "DRI3 import: unsupported (depth={depth}, bpp={bpp}); Phase 4.2 RGB single-plane only"
                )));
            }
        };
        let drawable = crate::kms::vk::dri3::import_dmabuf(
            vk,
            fd,
            u32::from(width),
            u32::from(height),
            format,
            modifier,
            &[crate::kms::vk::dri3::DmabufPlane {
                offset: u64::from(offset),
                pitch: stride,
            }],
        )
        .map_err(|e| io::Error::other(format!("DRI3 import_dmabuf: {e:?}")))?;
        let host_xid = self.next_host_xid();
        self.pixmaps.insert(
            host_xid,
            PixmapState {
                handle: host_xid,
                width,
                height,
                depth,
                vk_mirror: Some(drawable),
            },
        );
        yserver_core::backend::PixmapHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("DRI3 import: failed to make PixmapHandle"))
    }

    fn dri3_fence_from_fd(&mut self, fence_xid: u32, fd: std::os::fd::OwnedFd) -> io::Result<()> {
        // Mesa's loader_dri3 sends an xshmfence (memfd-backed shared
        // memory + futex) — try that path FIRST. vkImportSemaphoreFdKHR
        // rejects xshmfence fds because they aren't sync_file. Mmap
        // first; fall through to Vulkan import only if the mmap fails
        // (i.e. the fd really is a sync_file).
        use std::os::fd::AsFd;
        if let Some(mapping) = crate::kms::xshmfence::FenceMapping::map(fd.as_fd()) {
            self.dri3_xshmfences.insert(fence_xid, mapping);
            log::debug!("DRI3 FenceFromFD 0x{fence_xid:x}: imported as xshmfence");
            return Ok(());
        }
        let Some(vk) = self.vk.as_ref() else {
            return Err(io::Error::other(
                "DRI3 FenceFromFD: fd isn't xshmfence and Vulkan is unavailable",
            ));
        };
        let semaphore = crate::kms::vk::sync::import_sync_file(vk, fd)
            .map_err(|e| io::Error::other(format!("import_sync_file: {e:?}")))?;
        if let Some(prev) = self.dri3_sync_resources.insert(fence_xid, semaphore) {
            unsafe { vk.device.destroy_semaphore(prev, None) };
        }
        Ok(())
    }

    fn dri3_trigger_fence(&mut self, fence_xid: u32) -> io::Result<()> {
        if let Some(mapping) = self.dri3_xshmfences.get(&fence_xid) {
            mapping.trigger();
            return Ok(());
        }
        // VkSemaphore-backed fences: signalling is done via queue
        // submit (or vkSignalSemaphore for timeline). For Phase 4.2
        // first-cut Copy path the GPU work is already serialized, so
        // a server-only `triggered=true` mirror in state.sync_fences
        // is sufficient — no GPU operation needed here.
        Ok(())
    }

    fn dri3_fd_from_fence(&mut self, fence_xid: u32) -> io::Result<std::os::fd::OwnedFd> {
        let Some(vk) = self.vk.as_ref() else {
            return Err(io::Error::other("DRI3 FDFromFence: Vulkan unavailable"));
        };
        let &semaphore = self.dri3_sync_resources.get(&fence_xid).ok_or_else(|| {
            io::Error::other(format!("DRI3 FDFromFence: unknown fence 0x{fence_xid:x}"))
        })?;
        crate::kms::vk::sync::export_sync_file(vk, semaphore)
            .map_err(|e| io::Error::other(format!("export_sync_file: {e:?}")))
    }

    fn dri3_import_syncobj(
        &mut self,
        syncobj_xid: u32,
        fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        let Some(vk) = self.vk.as_ref() else {
            return Err(io::Error::other("DRI3 ImportSyncobj: Vulkan unavailable"));
        };
        let semaphore = crate::kms::vk::sync::import_drm_syncobj(vk, fd)
            .map_err(|e| io::Error::other(format!("import_drm_syncobj: {e:?}")))?;
        if let Some(prev) = self.dri3_sync_resources.insert(syncobj_xid, semaphore) {
            unsafe { vk.device.destroy_semaphore(prev, None) };
        }
        Ok(())
    }

    fn dri3_free_syncobj(&mut self, syncobj_xid: u32) -> io::Result<()> {
        let Some(vk) = self.vk.as_ref() else {
            return Err(io::Error::other("DRI3 FreeSyncobj: Vulkan unavailable"));
        };
        if let Some(sem) = self.dri3_sync_resources.remove(&syncobj_xid) {
            unsafe { vk.device.destroy_semaphore(sem, None) };
        }
        Ok(())
    }

    fn dri3_signal_syncobj(&mut self, syncobj_xid: u32, value: u64) -> io::Result<()> {
        let Some(vk) = self.vk.as_ref() else {
            return Err(io::Error::other("DRI3 SignalSyncobj: Vulkan unavailable"));
        };
        let &semaphore = self.dri3_sync_resources.get(&syncobj_xid).ok_or_else(|| {
            io::Error::other(format!(
                "DRI3 SignalSyncobj: unknown syncobj 0x{syncobj_xid:x}"
            ))
        })?;
        crate::kms::vk::sync::signal_timeline(vk, semaphore, value)
            .map_err(|e| io::Error::other(format!("vkSignalSemaphore: {e:?}")))
    }

    fn dri3_export_pixmap(
        &mut self,
        host_xid: u32,
    ) -> io::Result<(u32, u16, u16, u16, u8, u8, std::os::fd::OwnedFd)> {
        let Some(vk) = self.vk.as_ref() else {
            return Err(io::Error::other("DRI3 export: Vulkan unavailable"));
        };
        let pixmap = self.pixmaps.get(&host_xid).ok_or_else(|| {
            io::Error::other(format!("DRI3 export: unknown pixmap 0x{host_xid:x}"))
        })?;
        let drawable = pixmap.vk_mirror.as_ref().ok_or_else(|| {
            io::Error::other(format!(
                "DRI3 export: pixmap 0x{host_xid:x} has no GPU mirror"
            ))
        })?;
        let bpp: u8 = match pixmap.depth {
            24 | 32 => 32,
            d => d,
        };
        let export = crate::kms::vk::dri3::export_dmabuf(vk, drawable)
            .map_err(|e| io::Error::other(format!("DRI3 export_dmabuf: {e:?}")))?;
        let stride16 = u16::try_from(export.stride).unwrap_or(u16::MAX);
        Ok((
            export.size,
            pixmap.width,
            pixmap.height,
            stride16,
            pixmap.depth,
            bpp,
            export.fd,
        ))
    }

    fn dri3_supported_modifiers(&self, _window: u32, depth: u8, bpp: u8) -> (Vec<u64>, Vec<u64>) {
        let Some(vk) = self.vk.as_ref() else {
            return (vec![0], vec![0]);
        };
        // Map (depth, bpp) to a vk::Format. Phase 4.2 RGB single-plane
        // scope means we only handle depth-24/32 BGRA today; other
        // depths fall back to LINEAR-only.
        let format = match (depth, bpp) {
            (24, 32) | (32, 32) => ash::vk::Format::B8G8R8A8_UNORM,
            _ => return (vec![0], vec![0]),
        };
        let screen = crate::kms::vk::dri3::supported_modifiers(vk, format);
        // Window-modifier list is a subset that the window's output
        // can flip-scanout. Per design §3.2 we filter against the
        // (format, modifier) pairs the kernel accepted via add_fb2 at
        // backend init. Phase 4.1 always uses LINEAR for scanout, so
        // the window list collapses to LINEAR here. A follow-up
        // (Task 23 + Task 29) populates `output.scanout_format_set`
        // from the real add_fb2 probe and widens this.
        let window: Vec<u64> = screen.iter().copied().filter(|&m| m == 0).collect();
        let window = if window.is_empty() { vec![0] } else { window };
        (window, screen)
    }

    fn present_capabilities(&self, _window: u32) -> yserver_core::backend::PresentCaps {
        // Phase 4.2.3 first cut: report the conservative "Copy-path
        // only" caps. Tasks 30-32 turn flip_path / async_may_tear on
        // once alien-BO scanout integration is wired. syncobj mirrors
        // Dri3Caps::syncobj (requires DRI3 timeline support — Tasks
        // 18-20 ship the imports but the Present-side submit-time
        // handshake lands later).
        yserver_core::backend::PresentCaps {
            flip_path: false,
            async_may_tear: false,
            syncobj: self.dri3_capabilities().syncobj,
        }
    }

    fn dri3_capabilities(&self) -> yserver_core::backend::Dri3Caps {
        // DRI3 entirely unavailable when render-node fd or Vulkan
        // weren't resolved at backend init.
        if self.render_node_fd.is_none() || self.vk.is_none() {
            return yserver_core::backend::Dri3Caps::unsupported();
        }
        let vk = self.vk.as_ref().expect("vk Some by branch above");
        let modifiers = vk.image_drm_format_modifier;
        // VK_KHR_external_semaphore_fd is unconditionally enabled at
        // device init; fence_fd / SYNC_FD handle type rides along
        // with it. syncobj uses the OPAQUE_FD + timeline-semaphore
        // path (also part of VK_KHR_external_semaphore_fd) which is
        // what Mesa actually prefers — and which the live vng smoke
        // showed is the only sync path Venus accepts. With syncobj
        // advertised, Mesa uses ImportSyncobj + PresentPixmapSynced
        // and we host-signal the release_syncobj at release_value
        // when the Copy completes, waking Mesa's vkAcquireNextImage.
        let fence_fd = true;
        let syncobj = true;
        // Version cap per design §4: with syncobj advertise (1, 4);
        // without it cap at (1, 3). fence_fd doesn't affect version.
        let version = if syncobj { (1, 4) } else { (1, 3) };
        yserver_core::backend::Dri3Caps {
            version,
            modifiers,
            fence_fd,
            syncobj,
        }
    }

    fn root_visual_xid(&self) -> u32 {
        self.root_visual_xid
    }

    fn argb_visual_xid(&self) -> Option<u32> {
        None
    }

    fn argb_colormap_xid(&self) -> Option<u32> {
        None
    }

    fn render_opcode(&self) -> Option<u8> {
        // X11 conventional major opcode for RENDER. Advertising RENDER as
        // present (with all 21 render_* trait methods stubbed below as
        // no-ops) is enough to flip fvwm3 from a two-level frame hierarchy
        // into a single-level one — without RENDER fvwm3 builds a deeper
        // frame, which makes GetGeometry on client windows return (0,0)
        // and traps FvwmPager's init loop. See:
        // docs/superpowers/notes/2026-05-04-phase6-5-fvwm3-trace.md.
        Some(133)
    }

    fn xkb_opcode(&self) -> Option<u8> {
        // KmsBackend has a real XKB extension surface backed by
        // xkbcommon (see crates/yserver/src/kms/xkb.rs and the
        // xkb_proxy impl below). Advertising 136 unblocks any
        // X11 client whose toolkit (GTK, Qt, xcb-xkb) requires
        // the extension to bring up its windowing layer.
        Some(136)
    }

    fn xkb_info(&self) -> Option<(u8, u8, u8)> {
        // (major_opcode, first_event, first_error). 85 / 162 are
        // the next free slots in yserver's local first-event /
        // first-error namespace (see crates/yserver-core/src/nested.rs
        // for the rest of the table).
        Some((136, 85, 162))
    }

    fn composite_opcode(&self) -> Option<u8> {
        // L2 plan B.4 — advertise COMPOSITE. The companion B.5/B.6a
        // give `name_window_pixmap` + `allocate_redirected_backing`
        // their real impls; the trio lands together so the protocol
        // layer never reaches an `Unsupported` after seeing this.
        Some(144)
    }

    fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32> {
        // Pass the format ID through as an opaque handle. For zero (no mask)
        // the caller (nested.rs) maps it to Some(0) directly; we only reach
        // here for nonzero values. Returning Some(ynest_fmt) is sufficient
        // for render_trapezoids to receive a non-None host_mask_format and
        // proceed; the actual format code is mapped to PIXMAN_a8 inside
        // render_trapezoids.
        if ynest_fmt == 0 {
            None
        } else {
            Some(ynest_fmt)
        }
    }

    fn ping(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    /// Translate a host input event into wire events and fan them out
    /// directly into `state`.
    ///
    /// Pointer events use the existing `process_pointer_*` methods,
    /// which buffer into `self.pending_pointer_events`. We drain that
    /// buffer immediately and route through
    /// `pointer_event_fanout_to_state`. The buffer is local to a
    /// single call — there is no cross-thread handoff.
    ///
    /// Key events update `xkb_state` here, then go through
    /// `key_event_fanout_to_state`. The event passed in by the libinput
    /// thread carries a placeholder modifier mask and cursor coords;
    /// we override both with the backend's authoritative values.
    fn on_host_input(
        &mut self,
        state: &mut yserver_core::server::ServerState,
        ev: yserver_core::core_loop::HostInputEvent,
    ) {
        use yserver_core::core_loop::{
            HostInputEvent, key_fanout::key_event_fanout_to_state,
            pointer_fanout::pointer_event_fanout_to_state,
        };

        match ev {
            HostInputEvent::PointerMotion { x, y, time: _ } => {
                self.process_pointer_absolute(state, x as f32, y as f32);
            }
            HostInputEvent::PointerButton {
                button,
                pressed,
                time: _,
            } => {
                self.process_pointer_button(u32::from(button), pressed, state);
            }
            HostInputEvent::Key(raw) => {
                let cooked = self.cook_host_key(raw);
                let _dropped = key_event_fanout_to_state(state, cooked);
                return;
            }
        }

        // Drain pointer events queued by the process_pointer_* call
        // and fan each one out directly into state. The buffer must
        // be empty after this — a future on_host_input call asserts
        // it via `take`.
        let pending = std::mem::take(&mut self.pending_pointer_events);
        for ev in pending {
            let _dropped = pointer_event_fanout_to_state(state, &self.xid_map, ev, true, false);
        }
    }

    /// Drain DRM completion events and submit the next composite/flip.
    /// Errors are logged; the trait method is infallible because the
    /// core loop has no useful place to propagate them yet (B3 stub —
    /// E3 wires this into the poller).
    fn on_page_flip_ready(&mut self, _state: &mut yserver_core::server::ServerState) {
        if let Err(e) = self.drain_page_flips_and_composite() {
            log::warn!("kms: drain_page_flips_and_composite: {e}");
        }
    }

    fn mark_dirty(&mut self) {
        self.mark_all_outputs_dirty();
    }

    fn maybe_composite(&mut self) -> io::Result<()> {
        if !self.any_output_needs_composite() {
            return Ok(());
        }
        // If a flip is already in flight on any output, the upcoming
        // pageflip-complete will retrigger composite_and_flip. Don't
        // double-submit — submitting on a CRTC with a flip pending
        // returns -EBUSY and confuses the swapchain state machine
        // (see the long comment at composite_and_flip's per-output
        // skip).
        let any_pending = self.outputs.iter().enumerate().any(|(idx, layout)| {
            if layout.swapchain.submitted_idx().is_some() {
                return true;
            }
            self.scanout_pools
                .get(idx)
                .and_then(|p| p.as_ref())
                .map(|p| {
                    p.bos.iter().any(|b| {
                        matches!(
                            b.state.phase,
                            crate::kms::vk::scanout::BoPhase::Submitted
                                | crate::kms::vk::scanout::BoPhase::Pending
                        )
                    })
                })
                .unwrap_or(false)
        });
        if any_pending {
            return Ok(());
        }
        self.composite_and_flip()
    }

    fn dump_scanout(&mut self) {
        if let Err(e) = self.do_dump_scanout() {
            log::warn!("dump_scanout: {e}");
        }
    }

    fn poll_fds(&self) -> Vec<(std::os::fd::RawFd, yserver_core::backend::BackendFdKind)> {
        let mut out = Vec::with_capacity(2);
        if let Some(fd) = self.input_fd() {
            out.push((fd, yserver_core::backend::BackendFdKind::Libinput));
        }
        out.push((self.drm_fd(), yserver_core::backend::BackendFdKind::Drm));
        out
    }

    fn create_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        _background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle> {
        let host_xid = self.next_host_xid();
        let depth = match visual {
            HostSubwindowVisual::CopyFromParent => 24,
            HostSubwindowVisual::Explicit { depth, .. } => depth,
        };
        let visual_xid = match visual {
            HostSubwindowVisual::CopyFromParent => 0,
            HostSubwindowVisual::Explicit { visual_xid, .. } => visual_xid,
        };
        // Allocate the mirror (`initialize_clear` leaves it
        // transparent black, layout SHADER_READ_ONLY_OPTIMAL). If the
        // client requested a `background_pixel`, paint it onto the
        // mirror so xclock-style apps that expect the server to
        // auto-clear see the right backdrop.
        let mut vk_mirror = self.allocate_window_mirror(width, height);
        if let (Some(mirror), Some(pixel)) = (vk_mirror.as_mut(), background_pixel)
            && let Err(e) = self.fill_mirror_solid(mirror, pixel)
        {
            log::warn!("create_subwindow: bg_pixel fill failed: {e:?}");
        }
        self.windows.insert(
            host_xid,
            WindowState {
                _nested_id: ResourceId(0x0000_0100),
                x,
                y,
                width,
                height,
                border_width,
                mapped: false,
                _override_redirect: false,
                _parent: Some(host_parent.as_raw()),
                children: Vec::new(),
                bg_pixel: background_pixel,
                bg_pixmap: None,
                cursor: 0,
                depth,
                visual: visual_xid,
                vk_mirror,
            },
        );
        let parent_raw = host_parent.as_raw();
        if parent_raw == self.window_id {
            // Top-level: append to stacking order (newly created → on top).
            self.top_level_order.push(host_xid);
        } else if let Some(parent) = self.windows.get_mut(&parent_raw) {
            parent.children.push(host_xid);
        }
        WindowHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create window handle"))
    }

    fn destroy_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        // Gather sibling info before removing
        let parent_xid = self.windows.get(&host_xid).and_then(|w| w._parent);
        let siblings = if let Some(parent) = parent_xid {
            self.windows
                .get(&parent)
                .map(|p| p.children.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // Snapshot the mapped rect before destruction so we can dirty the
        // right outputs. Unmapped windows are invisible, so no dirty needed.
        let destroy_rect = self.windows.get(&host_xid).and_then(|w| {
            if !w.mapped {
                return None;
            }
            let (ox, oy) = self.absolute_origin(host_xid);
            Some(Rect {
                x: ox as i32,
                y: oy as i32,
                w: i32::from(w.width),
                h: i32::from(w.height),
            })
        });

        // Phase-3B drawable-destruction barrier: a batched paint op
        // recorded earlier (3B T2/T3 migrated fill + copy) may still
        // reference this window's vk_mirror VkImage, and an in-flight
        // composite may still sample it. flush_if_needed submits the
        // batch and queue_wait_idles, draining BOTH before we drop
        // the WindowState.
        //
        // On strict-flush Err (renderer is failed / GPU may still
        // hold the image), DO NOT drop the window's vk_mirror —
        // PaintBatch::submit_and_wait path-2 may have left the
        // GPU referencing it. Leave the WindowState in place so
        // its mirror outlives the abandoned GPU work; backend
        // teardown's global device_wait_idle eventually drains.
        // Return the error so the client sees a protocol failure
        // (acceptable; the renderer is dead anyway).
        crate::vk_count!(pb_drawable_destroy);
        if let Err(e) = self
            .flush_if_needed(crate::kms::scheduler::paint_batch::BatchFlushReason::ProtocolBarrier)
        {
            log::error!(
                "destroy_window: pre-destruction flush failed ({e:?}); leaving WindowState in place to avoid UAF"
            );
            return Err(std::io::Error::other(format!(
                "destroy_window pre-flush failed: {e:?}"
            )));
        }

        if self.windows.remove(&host_xid).is_some() {
            // Update parent's children list (or top-level stacking order
            // if this was a top-level window).
            if let Some(parent_xid) = parent_xid {
                if parent_xid == self.window_id {
                    self.top_level_order.retain(|&c| c != host_xid);
                } else if let Some(parent) = self.windows.get_mut(&parent_xid) {
                    parent.children.retain(|&c| c != host_xid);
                }
            }
            self.shape_bounding.remove(&host_xid);
            self.shape_clip.remove(&host_xid);
            self.shape_input.remove(&host_xid);
        }
        self.xid_map.remove(&host_xid);

        // Dirty outputs that displayed this window (empty new = nothing to show).
        let empty = Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        if let Some(rect) = destroy_rect {
            self.mark_window_dirty_with_old_rect(rect, empty);
        }
        let _ = siblings;
        Ok(())
    }

    fn map_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        if let Some(window) = self.windows.get_mut(&host_xid) {
            window.mapped = true;
        }
        // Dirty outputs intersecting the newly-visible rect.
        // old = empty (window wasn't visible), new = current rect.
        let empty = Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        let map_rect = self.windows.get(&host_xid).map(|w| {
            let (ox, oy) = self.absolute_origin(host_xid);
            Rect {
                x: ox as i32,
                y: oy as i32,
                w: i32::from(w.width),
                h: i32::from(w.height),
            }
        });
        self.mark_window_dirty_with_old_rect(empty, map_rect.unwrap_or(empty));
        Ok(())
    }

    fn unmap_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        // Gather info before unmapping
        let info = self
            .windows
            .get(&host_xid)
            .map(|w| (w._parent, w.children.clone(), w.x, w.y, w.width, w.height));
        let Some((parent_xid, _children, _wx, _wy, _ww, _wh)) = info else {
            return Ok(());
        };
        // Snapshot screen-space rect before unmapping, so we dirty the
        // right outputs (absolute_origin relies on the parent chain).
        let pre_rect = {
            let (ox, oy) = self.absolute_origin(host_xid);
            self.windows.get(&host_xid).map(|w| Rect {
                x: ox as i32,
                y: oy as i32,
                w: i32::from(w.width),
                h: i32::from(w.height),
            })
        };

        // Unmap the window
        if let Some(window) = self.windows.get_mut(&host_xid) {
            window.mapped = false;
        }
        // Dirty outputs that were displaying this window.
        // new = empty (window is now invisible).
        let empty = Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        if let Some(rect) = pre_rect {
            self.mark_window_dirty_with_old_rect(rect, empty);
        }
        let _ = parent_xid;
        Ok(())
    }

    fn configure_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()> {
        let resized = self
            .windows
            .get(&host_xid)
            .is_some_and(|_w| config.width.is_some() || config.height.is_some());
        // Snapshot the pre-change screen-space rect for old∪new dirty
        // propagation. `absolute_origin` needs &self so we capture it
        // before the &mut borrow below.
        let pre_rect = self.windows.get(&host_xid).map(|w| {
            let (ox, oy) = self.absolute_origin(host_xid);
            Rect {
                x: ox as i32,
                y: oy as i32,
                w: i32::from(w.width),
                h: i32::from(w.height),
            }
        });
        // Apply scalar updates first; capture the resize dims so the
        // mirror reallocation below can run without holding a &mut
        // borrow on `windows` (allocate_window_mirror takes &self).
        let resize_dims = if let Some(window) = self.windows.get_mut(&host_xid) {
            if let Some(w) = config.width {
                window.width = w;
            }
            if let Some(h) = config.height {
                window.height = h;
            }
            if let Some(x) = config.x {
                window.x = x;
            }
            if let Some(y) = config.y {
                window.y = y;
            }
            if let Some(bw) = config.border_width {
                window.border_width = bw;
            }
            if resized {
                Some((window.width, window.height, window.bg_pixel))
            } else {
                None
            }
        } else {
            None
        };
        if let Some((w, h, bg_pixel)) = resize_dims {
            // On shrink, the old VkImage is freed; on grow, a fresh
            // larger one is allocated. Allocation is best-effort —
            // failure leaves vk_mirror=None for this window and the
            // composite pass simply skips it until the next resize
            // succeeds.
            let mut new_mirror = self.allocate_window_mirror(w, h);
            if let (Some(mirror), Some(pixel)) = (new_mirror.as_mut(), bg_pixel)
                && let Err(e) = self.fill_mirror_solid(mirror, pixel)
            {
                log::warn!("configure_window: bg_pixel fill on resize failed: {e:?}");
            }
            // Phase-3B drawable-destruction barrier: flush any
            // batched paint that referenced the old vk_mirror (and
            // drain any in-flight composite that samples it) before
            // dropping it via the reassignment below. This also
            // flushes the fresh fill on `new_mirror` recorded just
            // above, which is fine — the next composite picks it up.
            //
            // On strict-flush Err: DO NOT replace the mirror. The
            // GPU may still reference the old one (path-2 leak),
            // and new_mirror's fill submission is in an indeterminate
            // state. Keep the old mirror; `mem::forget` new_mirror
            // so its Drop doesn't free a VkImage the GPU may still
            // hold. Renderer is failed; resize visually fails too.
            crate::vk_count!(pb_window_resize);
            if let Err(e) = self.flush_if_needed(
                crate::kms::scheduler::paint_batch::BatchFlushReason::ProtocolBarrier,
            ) {
                log::error!(
                    "configure_window resize: pre-replace flush failed ({e:?}); keeping old mirror and leaking new_mirror to avoid UAF"
                );
                std::mem::forget(new_mirror);
            } else if let Some(window) = self.windows.get_mut(&host_xid) {
                window.vk_mirror = new_mirror;
            }
        }
        // Dirty outputs intersecting old ∪ new screen-space rect.
        // Must happen after the position/size fields are updated so
        // `absolute_origin` returns the post-change coords.
        let empty = Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        let pre = pre_rect.unwrap_or(empty);
        let post = self.windows.get(&host_xid).map_or(empty, |w| {
            let (ox, oy) = self.absolute_origin(host_xid);
            Rect {
                x: ox as i32,
                y: oy as i32,
                w: i32::from(w.width),
                h: i32::from(w.height),
            }
        });
        self.mark_window_dirty_with_old_rect(pre, post);
        // Apply X11 stack_mode + sibling: restack the window in its
        // parent's stacking list. Without this, fvwm's "raise menu" path
        // (ConfigureWindow stack=Above on a freshly-mapped popup) leaves
        // the window in HashMap-iteration order — which can hide the
        // popup behind unrelated top-levels.
        if let Some(stack_mode) = config.stack_mode {
            self.restack_window(host_xid, stack_mode, config.sibling);
            // Restack doesn't move the window but changes which pixels are
            // visible — dirty outputs intersecting the current position.
            self.mark_window_dirty_with_old_rect(post, post);
        }
        let _ = resized;
        Ok(())
    }

    fn reparent_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        new_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()> {
        // Snapshot pre-change absolute rect for old∪new dirty propagation.
        // `absolute_origin` needs &self, so we capture before the &mut borrow.
        let pre_rect = self.windows.get(&host_xid).map(|w| {
            let (ox, oy) = self.absolute_origin(host_xid);
            Rect {
                x: ox as i32,
                y: oy as i32,
                w: i32::from(w.width),
                h: i32::from(w.height),
            }
        });

        let Some(window) = self.windows.get_mut(&host_xid) else {
            return Ok(());
        };
        let old_parent = window._parent;
        window._parent = Some(new_parent);
        window.x = x;
        window.y = y;
        // Remove from old parent's stacking list (or top-level order).
        if let Some(old_parent_xid) = old_parent {
            if old_parent_xid == self.window_id {
                self.top_level_order.retain(|&c| c != host_xid);
            } else if let Some(parent) = self.windows.get_mut(&old_parent_xid) {
                parent.children.retain(|&c| c != host_xid);
            }
        }
        // Append to new parent's stacking list (top of stack — X11
        // ReparentWindow semantics).
        if new_parent == self.window_id {
            self.top_level_order.push(host_xid);
        } else if let Some(parent) = self.windows.get_mut(&new_parent) {
            parent.children.push(host_xid);
        }
        // Dirty outputs intersecting old ∪ new screen-space rect.
        // The new absolute position is computed after parent/x/y are updated.
        let empty = Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        let pre = pre_rect.unwrap_or(empty);
        let post = self.windows.get(&host_xid).map_or(empty, |w| {
            let (ox, oy) = self.absolute_origin(host_xid);
            Rect {
                x: ox as i32,
                y: oy as i32,
                w: i32::from(w.width),
                h: i32::from(w.height),
            }
        });
        self.mark_window_dirty_with_old_rect(pre, post);
        Ok(())
    }

    fn change_subwindow_attributes(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()> {
        let Some(window) = self.windows.get_mut(&host_xid) else {
            return Ok(());
        };
        let mut idx = 0;
        if value_mask & 0x01 != 0 && !values.is_empty() {
            // CWBackPixmap
            window.bg_pixmap = PixmapHandle::from_raw(values[idx]);
            idx += 1;
        }
        if value_mask & 0x02 != 0 && values.len() > idx {
            // CWBackPixel
            window.bg_pixel = Some(values[idx]);
        }
        Ok(())
    }

    fn update_host_event_mask(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _mask: u32,
        _enabled: bool,
    ) -> io::Result<()> {
        Ok(())
    }

    fn register_top_level(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.xid_map.insert(host_xid, nested_id);
        Ok(())
    }

    fn register_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.xid_map.insert(host_xid, nested_id);
        Ok(())
    }

    fn unregister_host_window(&mut self, host_xid: u32) {
        self.xid_map.remove(&host_xid);
    }

    fn xid_map(&self) -> &HostXidMap {
        &self.xid_map
    }

    fn name_window_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_window: WindowHandle,
    ) -> io::Result<PixmapHandle> {
        // L2 plan B.5: alias the existing redirected backing.
        // Refcount-only — no allocation. The backing was allocated
        // by B.6a (`allocate_redirected_backing`) at REDIRECT
        // activation; here each NameWindowPixmap bumps the count.
        let backing = self
            .host_window_to_backing
            .get(&host_window.as_raw())
            .copied()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "window is not redirected (no backing)",
                )
            })?;
        self.alias_registry.incref(backing);
        Ok(backing)
    }

    fn release_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        // L2 plan B.6c — drop the redirect's reason-1 hold. If this
        // was the last reference (no `NameWindowPixmap` alias
        // survives), free the underlying pixmap mirror. Also
        // unregister the `host_window → backing` mapping so a future
        // `name_window_pixmap` on the same window doesn't hand back
        // a stale handle.
        let raw = backing.as_raw();
        self.host_window_to_backing.retain(|_, h| h.as_raw() != raw);
        if self.alias_registry.decref(backing) {
            self.free_pixmap(origin, raw)?;
        }
        Ok(())
    }

    fn allocate_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> io::Result<PixmapHandle> {
        // L2 plan B.6a. Idempotency: if `host_window` already has a
        // backing (same redirect-activation came around twice via
        // recovery / retry), return the existing handle without
        // bumping the registry — the redirect's reason-1 hold is
        // still in place.
        if let Some(existing) = self
            .host_window_to_backing
            .get(&host_window.as_raw())
            .copied()
        {
            return Ok(existing);
        }
        // Allocate a regular pixmap mirror sized to the window's
        // current geometry+depth. We piggyback `create_pixmap` —
        // it owns the host XID allocation + mirror allocation +
        // insert into `self.pixmaps`.
        let backing = self.create_pixmap(origin, depth, width, height)?;
        self.alias_registry.insert(
            backing,
            AliasEntry {
                refcount: 1,
                width,
                height,
                depth,
            },
        );
        self.host_window_to_backing
            .insert(host_window.as_raw(), backing);
        Ok(backing)
    }

    fn create_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle> {
        let host_xid = self.next_host_xid();
        let mut vk_mirror = self.allocate_pixmap_mirror(u32::from(width), u32::from(height), depth);
        // Mark fresh pixmap mirror fully dirty (see CreateWindow for
        // rationale).
        if let Some(m) = vk_mirror.as_mut() {
            m.mark_full_damage();
        }
        self.pixmaps.insert(
            host_xid,
            PixmapState {
                handle: host_xid,
                width,
                height,
                depth,
                vk_mirror,
            },
        );
        PixmapHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create pixmap handle"))
    }

    fn free_pixmap(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        // pixmap-pool T2: no synchronous flush. The mirror's VkImage
        // may be referenced by commands in the currently-open paint
        // batch or in any in-flight batch on `submitted_paint_batches`.
        // We adopt the mirror's (image, view, memory) into the open
        // batch as a `PooledPixmapReturn` BatchResource; when that
        // batch retires (its fence signals), the BatchResource's
        // release returns the entry to the pool (or destroys if the
        // bucket is full / the key is ineligible).
        //
        // Replaces Phase 3B's drawable-destruction barrier flush at
        // this site.

        let Some(ps) = self.pixmaps.remove(&host_xid) else {
            return Ok(());
        };
        let Some(mirror) = ps.vk_mirror else {
            return Ok(());
        };

        // Picture rescue path stays unchanged: a live picture
        // referencing this pixmap takes the mirror so its alpha can
        // outlive the FreePixmap (fvwm cursor pattern).
        let mut mirror_opt = Some(mirror);
        for (&pic_xid, pic) in &self.pictures {
            if let PictureState::Drawable { host_xid: xid, .. } = pic
                && *xid == host_xid
                && let Some(m) = mirror_opt.take()
            {
                self.picture_rescued_images.insert(pic_xid, m);
                break;
            }
        }
        let Some(mirror) = mirror_opt else {
            // Rescue took ownership; nothing to pool.
            return Ok(());
        };

        // Every mirror with a live `VkImage` MUST go through
        // defer-release, not direct-drop (codex P0 round 3:
        // `DrawableImage::Drop` is non-waiting, so direct-dropping a
        // mirror after the synchronous flush is removed is UAF /
        // driver-crash risk for any in-flight VkImage). Eligibility
        // and bucket-cap rejection are handled INSIDE
        // `PooledPixmapReturn::release` via `try_return`'s `Err`
        // path: ineligible (oversize) and full-bucket entries are
        // destroyed by the BatchResource at batch-retire time — by
        // which point the open batch's fence has signalled and the
        // GPU is done with the image. This is the load-bearing UAF
        // avoidance.
        //
        // DRI3-imported mirrors are an exception: they're backed by
        // `ImageBacking::Imported` (client-owned dma-buf), and
        // `DrawableImage::into_pool_entry` panics for that variant —
        // pooling client-imported memory makes no sense. Route them
        // through the synchronous flush+drop fallback below, which
        // is also where pre-init / partial-init paths land.
        let imported = matches!(
            mirror.backing,
            crate::kms::vk::target::ImageBacking::Imported { .. }
        );
        let prereqs = (
            self.pixmap_pool.as_ref().cloned(),
            self.vk.as_ref().cloned(),
            self.ops_command_pool.as_ref().map(|p| p.handle()),
        );
        let (Some(pool), Some(vk_arc), Some(pool_handle)) = prereqs else {
            // No defer infrastructure — preserve the pre-T2 flush +
            // direct-drop behaviour for this rare path. Should never
            // trigger post-init for server-owned mirrors.
            crate::vk_count!(pb_image_dealloc_fallback);
            if let Err(e) = self.flush_if_needed(
                crate::kms::scheduler::paint_batch::BatchFlushReason::ProtocolBarrier,
            ) {
                log::error!(
                    "free_pixmap fallback path: pre-destruction flush failed ({e:?}); \
                     leaking mirror to avoid UAF"
                );
                // Leak rather than UAF. Renderer is already in a bad
                // state if this branch ran.
                std::mem::forget(mirror);
                return Err(std::io::Error::other(format!(
                    "free_pixmap fallback flush failed: {e:?}"
                )));
            }
            drop(mirror);
            return Ok(());
        };
        if imported {
            // Same fallback shape as missing-prereqs: synchronous
            // flush ensures the GPU is done with the imported image
            // before its Drop tears down image / view / memory and
            // releases the dma-buf fd.
            crate::vk_count!(pb_dmabuf_release);
            if let Err(e) = self.flush_if_needed(
                crate::kms::scheduler::paint_batch::BatchFlushReason::ProtocolBarrier,
            ) {
                log::error!(
                    "free_pixmap imported path: pre-destruction flush failed ({e:?}); \
                     leaking mirror to avoid UAF"
                );
                std::mem::forget(mirror);
                return Err(std::io::Error::other(format!(
                    "free_pixmap imported flush failed: {e:?}"
                )));
            }
            drop(mirror);
            return Ok(());
        }

        // Defer-release path (the common case for every server-owned
        // mirror on a Vulkan-up backend). Build the BatchResource —
        // eligibility + bucket-cap are evaluated INSIDE its
        // `release()`, not here.
        let key = crate::kms::vk::pixmap_pool::PixmapPoolKey {
            width: mirror.extent.width,
            height: mirror.extent.height,
            format: mirror.format,
        };
        let entry = mirror.into_pool_entry();
        let pooled_return = Box::new(crate::kms::vk::pixmap_pool::PooledPixmapReturn {
            pool,
            key,
            entry: Some(entry),
        });

        // Phase 5 T2 defer-release. Adopts into the currently-open
        // paint batch (creating an Idle one if none). When that
        // batch retires (its fence signals), the BatchResource's
        // release runs — `try_return` attempts to pool; on `Err`
        // (ineligible / full bucket) destroys.
        self.scheduler
            .defer_resource_release(vk_arc, pool_handle, pooled_return);

        Ok(())
    }

    fn open_font(
        &mut self,
        _origin: Option<OriginContext>,
        name: &str,
    ) -> io::Result<(FontHandle, FontMetrics)> {
        let (face, metrics, char_cache) = self.font_loader.open_font(name)?;
        let host_xid = self.next_host_xid();
        let handle = FontHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create font handle"))?;
        self.fonts.insert(
            host_xid,
            FontState {
                handle: host_xid,
                face: RefCell::new(FreetypeFace(face)),
                metrics: metrics.clone(),
                char_info_cache: char_cache,
            },
        );
        Ok((handle, metrics))
    }

    fn close_font(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.fonts.remove(&host_xid);
        Ok(())
    }

    fn create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        source_pixmap: PixmapHandle,
        mask_pixmap: Option<PixmapHandle>,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorHandle> {
        // Rasterize the (source, mask, fore, back) tuple into BGRA cursor
        // pixels. Both pixmaps are depth-1, mirrored as `R8_UNORM` with
        // 0xFF/0x00 per pixel. X11 semantics: a pixel is visible iff the
        // mask bit is set (or always, if no mask is given); visible
        // pixels carry `fore` where the source bit is set, `back`
        // otherwise. Invisible pixels stay (0, 0, 0, 0) — Vulkan
        // composite samples premultiplied alpha so transparent.
        let host_xid = self.next_host_xid();
        let cursor_handle = CursorHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create cursor handle"))?;

        let src_xid = source_pixmap.as_raw();
        let Some(src_rb) = self.read_mirror_pixels(src_xid) else {
            log::warn!("create_cursor: source pixmap {src_xid} has no mirror; cursor invisible");
            self.cursors.insert(
                host_xid,
                CursorState {
                    extent: ash::vk::Extent2D::default(),
                    hot_x,
                    hot_y,
                    vk_mirror: None,
                },
            );
            return Ok(cursor_handle);
        };
        let w = src_rb.width;
        let h = src_rb.height;

        // Read mask only if its dims match; otherwise treat as no-mask
        // (defensive — protocol says a size mismatch is BadMatch, but
        // the core handler doesn't validate yet).
        let mask_rb = match mask_pixmap {
            Some(m) => {
                let mxid = m.as_raw();
                let rb = self.read_mirror_pixels(mxid);
                match rb {
                    Some(mrb) if mrb.width == w && mrb.height == h => Some(mrb),
                    Some(mrb) => {
                        log::warn!(
                            "create_cursor: mask {mxid} dims {mw}x{mh} \
                             differ from source {src_xid} dims {w}x{h}; ignoring mask",
                            mw = mrb.width,
                            mh = mrb.height,
                        );
                        None
                    }
                    None => {
                        log::warn!("create_cursor: mask pixmap {mxid} has no mirror");
                        None
                    }
                }
            }
            None => None,
        };

        let fr = (fore.0 >> 8) as u8;
        let fg = (fore.1 >> 8) as u8;
        let fb = (fore.2 >> 8) as u8;
        let br = (back.0 >> 8) as u8;
        let bg = (back.1 >> 8) as u8;
        let bb = (back.2 >> 8) as u8;

        let pixel_count = (w as usize) * (h as usize);
        let mut argb = vec![0u8; pixel_count * 4];
        for i in 0..pixel_count {
            let src_set = src_rb.bytes.get(i).copied().unwrap_or(0) != 0;
            let visible = match &mask_rb {
                Some(mb) => mb.bytes.get(i).copied().unwrap_or(0) != 0,
                None => true,
            };
            if !visible {
                continue;
            }
            let off = i * 4;
            if src_set {
                argb[off] = fb;
                argb[off + 1] = fg;
                argb[off + 2] = fr;
            } else {
                argb[off] = bb;
                argb[off + 1] = bg;
                argb[off + 2] = br;
            }
            argb[off + 3] = 0xFF;
        }

        let extent = ash::vk::Extent2D {
            width: w,
            height: h,
        };
        let vk_mirror = if let Some(mut mirror) = self.allocate_cursor_mirror(w, h) {
            if let Err(e) = self.upload_bgra_to_mirror(&mut mirror, &argb) {
                log::warn!("create_cursor: upload_bgra_to_mirror failed: {e:?}");
            }
            Some(mirror)
        } else {
            None
        };

        self.cursors.insert(
            host_xid,
            CursorState {
                extent,
                hot_x,
                hot_y,
                vk_mirror,
            },
        );
        Ok(cursor_handle)
    }

    fn create_glyph_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        source_font: FontHandle,
        mask_font: Option<FontHandle>,
        source_char: u16,
        mask_char: u16,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
    ) -> io::Result<CursorHandle> {
        // Rasterize the source glyph (and optional mask glyph) into a
        // BGRA cursor image. Glyphs are aligned at their FreeType
        // origins; the cursor pixmap extent is the union of both
        // glyphs' bounding boxes, and the hotspot is the source glyph
        // origin in pixmap coords (matches Xorg's AllocGlyphCursor in
        // dix/cursor.c).
        //
        // X11 pixel rule:
        //   mask given     → visible iff mask bit set; visible pixels
        //                    carry `fore` if source bit set else `back`.
        //   no mask (None) → source doubles as mask: visible iff source
        //                    bit set; visible pixels always carry `fore`.
        let host_xid = self.next_host_xid();
        let cursor_handle = CursorHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create cursor handle"))?;

        // Render one glyph: returns (pixels[w*h], w, h, bitmap_left,
        // bitmap_top). FreeType invalidates the previous glyph's bitmap
        // on the next load_char, so we copy into an owned Vec eagerly.
        // Empty glyphs (e.g. SPACE) return a 1x1 zero buffer so the
        // bbox math still has something to work with.
        let rasterize =
            |this: &Self, font_xid: u32, ch: u16| -> Option<(Vec<u8>, i32, i32, i32, i32)> {
                let fs = this.fonts.get(&font_xid)?;
                let face = fs.face.borrow();
                let _ = face
                    .0
                    .load_char(ch as usize, freetype::face::LoadFlag::RENDER);
                let glyph = face.0.glyph();
                let bitmap = glyph.bitmap();
                let w = bitmap.width();
                let h = bitmap.rows();
                if w <= 0 || h <= 0 {
                    return Some((vec![0u8], 1, 1, glyph.bitmap_left(), glyph.bitmap_top()));
                }
                let stride = bitmap.pitch();
                let buf = bitmap.buffer();
                let wu = w as usize;
                let hu = h as usize;
                let mut pixels = vec![0u8; wu * hu];
                for row in 0..hu {
                    let src_off = if stride >= 0 {
                        row * stride as usize
                    } else {
                        (hu - 1 - row) * (stride as isize).unsigned_abs()
                    };
                    pixels[row * wu..row * wu + wu].copy_from_slice(&buf[src_off..src_off + wu]);
                }
                Some((pixels, w, h, glyph.bitmap_left(), glyph.bitmap_top()))
            };

        let src_xid = source_font.as_raw();
        let Some((src_pix, src_w, src_h, src_lsb, src_top)) = rasterize(self, src_xid, source_char)
        else {
            log::warn!("create_glyph_cursor: source font 0x{src_xid:x} unknown; cursor invisible");
            self.cursors.insert(
                host_xid,
                CursorState {
                    extent: ash::vk::Extent2D::default(),
                    hot_x: 0,
                    hot_y: 0,
                    vk_mirror: None,
                },
            );
            return Ok(cursor_handle);
        };

        let mask_data = mask_font.and_then(|mf| rasterize(self, mf.as_raw(), mask_char));

        // Union bbox in glyph-origin coords (positive y up). `top` and
        // `bottom` are non-negative extents above/below baseline; `left`
        // is signed (can be negative for italic-style glyphs).
        let (left, right, top, bottom) = match &mask_data {
            Some((_, mw, mh, ml, mt)) => (
                src_lsb.min(*ml),
                (src_lsb + src_w).max(ml + mw),
                src_top.max(*mt),
                (src_h - src_top).max(mh - mt),
            ),
            None => (src_lsb, src_lsb + src_w, src_top, src_h - src_top),
        };
        let pixmap_w = (right - left).max(1) as u32;
        let pixmap_h = (top + bottom).max(1) as u32;

        // Hotspot = source glyph origin point in pixmap coords (top-left
        // origin, y down).
        let hot_x = (-left).clamp(0, i32::from(u16::MAX)) as u16;
        let hot_y = top.clamp(0, i32::from(u16::MAX)) as u16;

        let fr = (fore.0 >> 8) as u8;
        let fg = (fore.1 >> 8) as u8;
        let fb = (fore.2 >> 8) as u8;
        let br = (back.0 >> 8) as u8;
        let bg = (back.1 >> 8) as u8;
        let bb = (back.2 >> 8) as u8;

        let read_bit = |pixels: &[u8], w: i32, h: i32, x: i32, y: i32| -> bool {
            if x < 0 || y < 0 || x >= w || y >= h {
                return false;
            }
            pixels[(y * w + x) as usize] > 0
        };

        // Glyph G top-left in pixmap coords: (G.lsb - left, top - G.top).
        let src_off_x = src_lsb - left;
        let src_off_y = top - src_top;
        let mask_off = mask_data
            .as_ref()
            .map(|(_, _, _, ml, mt)| (*ml - left, top - *mt));

        let pixel_count = (pixmap_w as usize) * (pixmap_h as usize);
        let mut argb = vec![0u8; pixel_count * 4];
        for y in 0..pixmap_h as i32 {
            for x in 0..pixmap_w as i32 {
                let src_set = read_bit(&src_pix, src_w, src_h, x - src_off_x, y - src_off_y);
                let visible = match (&mask_data, mask_off) {
                    (Some((mp, mw, mh, _, _)), Some((moff_x, moff_y))) => {
                        read_bit(mp, *mw, *mh, x - moff_x, y - moff_y)
                    }
                    _ => src_set,
                };
                if !visible {
                    continue;
                }
                let off = ((y as u32 * pixmap_w + x as u32) * 4) as usize;
                if src_set {
                    argb[off] = fb;
                    argb[off + 1] = fg;
                    argb[off + 2] = fr;
                } else {
                    argb[off] = bb;
                    argb[off + 1] = bg;
                    argb[off + 2] = br;
                }
                argb[off + 3] = 0xFF;
            }
        }

        let extent = ash::vk::Extent2D {
            width: pixmap_w,
            height: pixmap_h,
        };
        let vk_mirror = if let Some(mut mirror) = self.allocate_cursor_mirror(pixmap_w, pixmap_h) {
            if let Err(e) = self.upload_bgra_to_mirror(&mut mirror, &argb) {
                log::warn!("create_glyph_cursor: upload_bgra_to_mirror failed: {e:?}");
            }
            Some(mirror)
        } else {
            None
        };

        self.cursors.insert(
            host_xid,
            CursorState {
                extent,
                hot_x,
                hot_y,
                vk_mirror,
            },
        );
        Ok(cursor_handle)
    }

    fn define_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_window_xid: u32,
        cursor_host_xid: u32,
    ) -> io::Result<()> {
        // Per X11: each window has its own cursor attribute. The cursor
        // visible on screen is the one belonging to the deepest window
        // under the pointer that has a non-None cursor (walking up the
        // parent chain). cursor_host_xid == 0 means "inherit from
        // parent" (X11 `None`).
        let known = self.windows.contains_key(&host_window_xid);
        let cursor_known = self.cursors.contains_key(&cursor_host_xid);
        log::debug!(
            "define_cursor: window 0x{host_window_xid:x}{} ← cursor 0x{cursor_host_xid:x}{}",
            if known { "" } else { " (UNKNOWN)" },
            if cursor_known || cursor_host_xid == 0 {
                ""
            } else {
                " (UNKNOWN)"
            },
        );
        if let Some(w) = self.windows.get_mut(&host_window_xid) {
            w.cursor = cursor_host_xid;
        }
        // `active_cursor` is the sticky fallback used by effective_cursor()
        // when the walk-up hits no explicit cursor (the root container
        // isn't tracked in self.windows, so the chain always runs out
        // there). It's seeded at startup with the built-in X-shaped
        // default; a DefineCursor on the root container overrides it.
        if cursor_host_xid != 0 && host_window_xid == self.window_id {
            self.active_cursor = Some(cursor_host_xid);
        }
        // The window the pointer is over may have just had its
        // cursor changed under it — push the new image to the HW
        // plane. No-op when the effective cursor didn't actually
        // change (e.g. setting a different window's cursor that
        // the pointer isn't over).
        self.hw_cursor_refresh();
        Ok(())
    }

    fn set_container_background_pixel(
        &mut self,
        _origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()> {
        self.bg_pixel = Some(pixel);
        Ok(())
    }

    fn set_container_background_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()> {
        self.bg_pixmap = PixmapHandle::from_raw(host_pixmap_xid);
        Ok(())
    }

    fn clear_clip_rectangles(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        self.current_clip = ClipState::None;
        Ok(())
    }

    fn set_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        clip: Option<ClipRectangles>,
    ) -> io::Result<()> {
        self.current_clip = match clip {
            Some(c) => ClipState::Rectangles {
                origin: (c.x_origin, c.y_origin),
                rects: c,
            },
            None => ClipState::None,
        };
        Ok(())
    }

    fn set_clip_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap: u32,
        clip_x_origin: i16,
        clip_y_origin: i16,
    ) -> io::Result<()> {
        // Pixmap clip-masks aren't enforced yet — the rasteriser only knows
        // how to intersect rect lists. Store the state so future work can pick
        // it up; right now this means a pixmap-mask GC clip is silently
        // ignored (matches pre-fix behaviour for that specific shape).
        let Some(handle) = PixmapHandle::from_raw(host_pixmap) else {
            self.current_clip = ClipState::None;
            return Ok(());
        };
        self.current_clip = ClipState::Pixmap {
            origin: (clip_x_origin, clip_y_origin),
            pixmap: handle,
        };
        Ok(())
    }

    fn set_gc_fill_solid(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        self.current_fill = FillState::Solid;
        Ok(())
    }

    fn set_gc_fill_tiled(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pixmap: u32,
        _tile_x_origin: i16,
        _tile_y_origin: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn apply_clip_state(
        &mut self,
        _origin: Option<OriginContext>,
        clip: &ClipState,
    ) -> io::Result<()> {
        self.current_clip = clip.clone();
        Ok(())
    }

    fn apply_fill_state(
        &mut self,
        _origin: Option<OriginContext>,
        fill: &FillState,
    ) -> io::Result<()> {
        self.current_fill = fill.clone();
        Ok(())
    }

    fn apply_draw_state(
        &mut self,
        _origin: Option<OriginContext>,
        state: &DrawState,
    ) -> io::Result<()> {
        if let Some(font) = state.font {
            self.current_font = Some(font.as_raw());
        }
        self.current_function = state.function;
        self.current_foreground = state.foreground;
        self.current_background = state.background;
        Ok(())
    }

    fn copy_area(
        &mut self,
        _origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        // Phase 4.1.5: Vulkan-only. `try_vk_copy_area` handles
        // distinct src/dst, same-target non-overlapping, and
        // same-target overlapping (via the BGRA `copy_scratch`).
        // Format mismatch (R8↔BGRA) silently skips — those would
        // need a sample-and-store shader; deferred. Mirror missing
        // also skips.
        self.try_vk_copy_area(
            src_host_xid,
            dst_host_xid,
            src_x,
            src_y,
            dst_x,
            dst_y,
            width,
            height,
        );
        Ok(())
    }

    fn copy_plane(
        &mut self,
        _origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
        plane: u32,
    ) -> io::Result<()> {
        let Some(src_depth) = self.drawable_depth(src_host_xid) else {
            return Ok(());
        };
        let Some(src_readback) = self.read_mirror_pixels(src_host_xid) else {
            return Ok(());
        };

        let mut foreground_rects = Vec::new();
        let mut background_rects = Vec::new();
        for row in 0..height {
            let sy = i32::from(src_y) + i32::from(row);
            let dy = dst_y.saturating_add(row as i16);
            if sy < 0 {
                continue;
            }
            for col in 0..width {
                let sx = i32::from(src_x) + i32::from(col);
                let dx = dst_x.saturating_add(col as i16);
                if sx < 0 {
                    continue;
                }
                let pixel =
                    read_mirror_pixel_for_plane(&src_readback, src_depth, sx as usize, sy as usize);
                let rect = Rectangle16 {
                    x: dx,
                    y: dy,
                    width: 1,
                    height: 1,
                };
                if pixel & plane != 0 {
                    foreground_rects.push(rect);
                } else {
                    background_rects.push(rect);
                }
            }
        }

        let function = self.current_function;
        let foreground = self.current_foreground;
        let background = self.current_background;
        let foreground_rects = self.intersect_with_current_clip(&foreground_rects);
        let background_rects = self.intersect_with_current_clip(&background_rects);
        // Bg first, then fg (matches the previous pixman ordering so
        // overlap behaviour is preserved). The Vk path now honours
        // every GC function via `try_vk_fill_with_function` (Copy
        // fast path or per-`VkLogicOp` pipeline).
        self.try_vk_fill_with_function(dst_host_xid, function, background, &background_rects);
        self.try_vk_fill_with_function(dst_host_xid, function, foreground, &foreground_rects);
        Ok(())
    }

    fn put_image(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()> {
        // Phase 4.1.5: Vulkan-only. `try_vk_put_image` covers
        // depth-1 / depth-8 / depth-24 / depth-32. Depth-15 / depth-16
        // pixmaps don't have a matching mirror format and silently
        // skip — these depths aren't used by any current X11 client
        // we run.
        self.try_vk_put_image(host_xid, depth, width, height, dst_x, dst_y, data);
        Ok(())
    }

    fn get_image(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        _format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        _plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        let depth = self.drawable_depth(host_xid).unwrap_or(24);

        // X11 GetImage reply header (32 bytes). bpp + row stride match
        // the legacy pixman path so the Vulkan and pixman branches
        // emit identically-shaped replies.
        let bpp = match depth {
            1 => 1u32,
            4 => 4,
            8 => 8,
            15 | 16 => 16,
            24 | 32 => 32,
            _ => 32,
        };
        let row_bytes = (((width as u32) * bpp).div_ceil(32) * 4) as usize;
        let pixel_bytes = row_bytes * (height as usize);
        let mut result = Vec::with_capacity(32 + pixel_bytes);
        let reply_length_units = (pixel_bytes / 4) as u32;
        result.push(1); // 0: Reply
        result.push(depth); // 1: actual drawable depth
        result.extend_from_slice(&[0u8; 2]); // 2..4: sequence (patched by nested.rs)
        result.extend_from_slice(&reply_length_units.to_le_bytes()); // 4..8: length in u32 units
        result.extend_from_slice(&[0u8; 4]); // 8..12: visual (patched by nested.rs)
        result.extend_from_slice(&[0u8; 20]); // 12..32: padding
        debug_assert_eq!(result.len(), 32);

        // Phase 4.1.5: Vulkan-only readback. Returns true with
        // pixel bytes pushed into `result`. On failure the request
        // returns a zero-filled payload so the X11 reply stream
        // stays aligned — clients see all-zero pixels for unknown
        // formats (was previously the case for depths 1/4/15/16
        // anyway, even on the pixman path).
        if !self.try_vk_get_image_pixels(host_xid, x, y, width, height, depth, &mut result) {
            result.resize(32 + pixel_bytes, 0);
        }
        Ok(Some(result))
    }

    fn read_depth1_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<Option<(u32, u32, Vec<u8>)>> {
        // Depth-1 pixmaps mirror as `R8_UNORM` — 1 byte per pixel,
        // 0xFF or 0x00. `read_mirror_pixels` returns those bytes
        // directly; just verify the depth and bytes-per-pixel match
        // before handing them over.
        if self.drawable_depth(host_xid) != Some(1) {
            return Ok(None);
        }
        let Some(rb) = self.read_mirror_pixels(host_xid) else {
            return Ok(None);
        };
        if rb.bytes_per_pixel != 1 {
            return Ok(None);
        }
        Ok(Some((rb.width, rb.height, rb.bytes)))
    }

    fn poly_line(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        // X11 PolyLine: connect consecutive points with line segments.
        // coordinate_mode 0 = Origin (absolute), 1 = Previous (each point is
        // a delta from the previous).  Rasterise each segment with Bresenham.
        let mut rects: Vec<Rectangle16> = Vec::new();
        let mut prev: Option<(i32, i32)> = None;
        let mut offset = 0;
        while offset + 4 <= points.len() {
            let Some((x, y)) = read_i16_pair(points, offset) else {
                break;
            };
            offset += 4;
            let (xi, yi) = if coordinate_mode == 1 {
                if let Some((px, py)) = prev {
                    (px + x as i32, py + y as i32)
                } else {
                    (x as i32, y as i32)
                }
            } else {
                (x as i32, y as i32)
            };
            if let Some((px, py)) = prev {
                bresenham_segment(px, py, xi, yi, &mut rects);
            }
            prev = Some((xi, yi));
        }
        let rects = self.intersect_with_current_clip(&rects);
        // Phase 4.1.4.4: route through the shared solid-fill helper
        // so `Copy` strokes hit `try_vk_solid_fill`. `Xor` etc. fall
        // through to the pixman path inside the helper.
        self.fill_rects_solid_with_gc_function(host_xid, foreground, &rects);
        Ok(())
    }

    fn poly_segment(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()> {
        // Each segment is (x1:i16, y1:i16, x2:i16, y2:i16). Bresenham
        // rasterises diagonals correctly (axis-aligned bbox would only work
        // for horizontal / vertical segments).
        let mut rects: Vec<Rectangle16> = Vec::new();
        let mut offset = 0;
        while offset + 8 <= segments.len() {
            let Some((x1, y1)) = read_i16_pair(segments, offset) else {
                break;
            };
            let Some((x2, y2)) = read_i16_pair(segments, offset + 4) else {
                break;
            };
            offset += 8;
            bresenham_segment(x1 as i32, y1 as i32, x2 as i32, y2 as i32, &mut rects);
        }
        let rects = self.intersect_with_current_clip(&rects);
        // Phase 4.1.4.4: see `poly_line`.
        self.fill_rects_solid_with_gc_function(host_xid, foreground, &rects);
        Ok(())
    }

    fn poly_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        // Draw rectangle outlines (4 thin rectangles per rect)
        let mut rects = Vec::new();
        let mut offset = 0;
        while offset + 8 <= rectangles.len() {
            let Some(r) = read_rect(rectangles, offset) else {
                break;
            };
            offset += 8;
            if r.width == 0 || r.height == 0 {
                continue;
            }
            // top edge
            rects.push(Rectangle16 {
                x: r.x,
                y: r.y,
                width: r.width,
                height: 1,
            });
            // bottom edge
            rects.push(Rectangle16 {
                x: r.x,
                y: r.y.wrapping_add(r.height as i16).wrapping_sub(1),
                width: r.width,
                height: 1,
            });
            // left edge
            rects.push(Rectangle16 {
                x: r.x,
                y: r.y,
                width: 1,
                height: r.height,
            });
            // right edge
            rects.push(Rectangle16 {
                x: r.x.wrapping_add(r.width as i16).wrapping_sub(1),
                y: r.y,
                width: 1,
                height: r.height,
            });
        }
        let rects = self.intersect_with_current_clip(&rects);
        // Phase 4.1.4.4: see `poly_line`.
        self.fill_rects_solid_with_gc_function(host_xid, foreground, &rects);
        Ok(())
    }

    fn poly_arc(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        // Draw arc outlines.  Each arc is 12 bytes:
        //   x(i16) y(i16) w(u16) h(u16) angle1(i16) angle2(i16)
        // Like poly_fill_arc we treat partial-angle arcs as full ellipses
        // for now (the angle-mask refinement is a follow-up).
        //
        // Algorithm: for each scanline `py` of the bounding box, compute the
        // ellipse's inside x-range [x0, x1] and emit:
        //   - the full horizontal span at the first/last interior scanline
        //     (the top/bottom caps),
        //   - segments connecting the prev row's left/right edges to this
        //     row's left/right edges otherwise (the side outlines).
        // This produces a closed 1-pixel outline.
        let mut rects: Vec<Rectangle16> = Vec::new();
        for chunk in arcs.chunks_exact(12) {
            let ax = i16::from_le_bytes([chunk[0], chunk[1]]) as i32;
            let ay = i16::from_le_bytes([chunk[2], chunk[3]]) as i32;
            let aw = u16::from_le_bytes([chunk[4], chunk[5]]) as i32;
            let ah = u16::from_le_bytes([chunk[6], chunk[7]]) as i32;
            if aw <= 0 || ah <= 0 {
                continue;
            }
            let cx = ax as f64 + (aw as f64) * 0.5;
            let cy = ay as f64 + (ah as f64) * 0.5;
            let rx = (aw as f64) * 0.5;
            let ry = (ah as f64) * 0.5;

            let row_at = |py: i32| -> Option<(i32, i32)> {
                let dy = (py as f64 + 0.5 - cy) / ry;
                if dy.abs() > 1.0 {
                    return None;
                }
                let dx = (1.0 - dy * dy).sqrt() * rx;
                let x0 = (cx - dx).floor() as i32;
                let x1 = (cx + dx).ceil() as i32;
                Some((x0, x1))
            };

            let mut prev: Option<(i32, i32)> = None;
            for py in ay..ay + ah {
                let Some((x0, x1)) = row_at(py) else {
                    prev = None;
                    continue;
                };
                let next = row_at(py + 1);
                let cap = prev.is_none() || next.is_none();
                if cap {
                    // Full horizontal span (top or bottom of curve).
                    rects.push(Rectangle16 {
                        x: x0 as i16,
                        y: py as i16,
                        width: (x1 - x0 + 1) as u16,
                        height: 1,
                    });
                } else {
                    // Side connectors: left edge and right edge runs
                    // bridging this row's edge to the previous row's.
                    let (px0, px1) = prev.unwrap();
                    let l_lo = px0.min(x0);
                    let l_hi = px0.max(x0);
                    rects.push(Rectangle16 {
                        x: l_lo as i16,
                        y: py as i16,
                        width: (l_hi - l_lo + 1) as u16,
                        height: 1,
                    });
                    let r_lo = px1.min(x1);
                    let r_hi = px1.max(x1);
                    rects.push(Rectangle16 {
                        x: r_lo as i16,
                        y: py as i16,
                        width: (r_hi - r_lo + 1) as u16,
                        height: 1,
                    });
                }
                prev = Some((x0, x1));
            }
        }
        let rects = self.intersect_with_current_clip(&rects);
        // Phase 4.1.4.4: see `poly_line`.
        self.fill_rects_solid_with_gc_function(host_xid, foreground, &rects);
        Ok(())
    }

    fn poly_point(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        _coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        let mut rects = Vec::new();
        let mut offset = 0;
        while offset + 4 <= points.len() {
            let Some((x, y)) = read_i16_pair(points, offset) else {
                break;
            };
            offset += 4;
            rects.push(Rectangle16 {
                x,
                y,
                width: 1,
                height: 1,
            });
        }
        let rects = self.intersect_with_current_clip(&rects);
        // Phase 4.1.4.4: see `poly_line`.
        self.fill_rects_solid_with_gc_function(host_xid, foreground, &rects);
        Ok(())
    }

    fn poly_fill_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        let mut rects = Vec::new();
        let mut offset = 0;
        while offset + 8 <= rectangles.len() {
            let Some(r) = read_rect(rectangles, offset) else {
                break;
            };
            offset += 8;
            rects.push(r);
        }
        self.fill_rects_honoring_fill_state(host_xid, foreground, &rects);
        Ok(())
    }

    fn poly_fill_arc(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        // Each arc is 12 bytes: x(i16) y(i16) w(u16) h(u16) angle1(i16) angle2(i16).
        // angles are in 64ths of a degree (X11 convention).
        // We treat any arc with |angle2| >= 360*64 as a full ellipse and fill it
        // with a scanline approach. Partial arcs fall back to filling the full
        // ellipse for now; xeyes uses full circles so this is sufficient.
        let dst_dims = self.drawable_dims(host_xid).unwrap_or((0, 0));
        let img_w = dst_dims.0 as i32;
        let img_h = dst_dims.1 as i32;
        let mut rects: Vec<Rectangle16> = Vec::new();
        for chunk in arcs.chunks_exact(12) {
            let ax = i16::from_le_bytes([chunk[0], chunk[1]]) as i32;
            let ay = i16::from_le_bytes([chunk[2], chunk[3]]) as i32;
            let aw = u16::from_le_bytes([chunk[4], chunk[5]]) as i32;
            let ah = u16::from_le_bytes([chunk[6], chunk[7]]) as i32;
            if aw <= 0 || ah <= 0 {
                continue;
            }
            let cx = ax as f64 + (aw as f64) * 0.5;
            let cy = ay as f64 + (ah as f64) * 0.5;
            let rx = (aw as f64) * 0.5;
            let ry = (ah as f64) * 0.5;
            let y_start = ay.max(0);
            let y_end = (ay + ah).min(img_h);
            for py in y_start..y_end {
                let dy = (py as f64 + 0.5 - cy) / ry;
                if dy.abs() > 1.0 {
                    continue;
                }
                let dx = (1.0 - dy * dy).sqrt() * rx;
                let x0 = (cx - dx).floor().max(0.0) as i32;
                let x1 = (cx + dx).ceil().min(img_w as f64) as i32;
                if x1 <= x0 {
                    continue;
                }
                rects.push(Rectangle16 {
                    x: x0 as i16,
                    y: py as i16,
                    width: (x1 - x0) as u16,
                    height: 1,
                });
            }
        }
        if !rects.is_empty() {
            self.fill_rects_honoring_fill_state(host_xid, foreground, &rects);
        }
        Ok(())
    }

    fn fill_poly(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        // Parse i16 vertex pairs.  coord_mode 0 = Origin (absolute), 1 =
        // Previous (deltas from prior vertex).
        let mut verts: Vec<(i32, i32)> = Vec::with_capacity(points.len() / 4);
        let mut offset = 0;
        let mut last = (0i32, 0i32);
        while offset + 4 <= points.len() {
            let Some((x, y)) = read_i16_pair(points, offset) else {
                break;
            };
            offset += 4;
            let (xi, yi) = if coord_mode == 1 && !verts.is_empty() {
                (last.0 + x as i32, last.1 + y as i32)
            } else {
                (x as i32, y as i32)
            };
            verts.push((xi, yi));
            last = (xi, yi);
        }
        let mut rects: Vec<Rectangle16> = Vec::new();
        scanline_fill_polygon(&verts, &mut rects);
        let dst_dims = self.drawable_dims(host_xid).unwrap_or((0, 0));
        let clipped = clip_rects_to_image(&rects, dst_dims.0 as i32, dst_dims.1 as i32);
        self.fill_rects_honoring_fill_state(host_xid, foreground, &clipped);
        Ok(())
    }

    fn fill_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let rect = Rectangle16 {
            x,
            y,
            width,
            height,
        };
        let function = self.current_function;
        self.try_vk_fill_with_function(host_xid, function, foreground, &[rect]);
        Ok(())
    }

    fn poly_text8(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + LISTofTEXTITEM8.
        // Each TEXTITEM8 is `len(u8) delta(i8) chars(len)` for len in 0..=254,
        // or `255 font_id(u32 BE)` for a font change. No inter-item padding.
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut items = &body[12..];
        let mut cursor_x = x;

        while items.len() >= 2 {
            let len = items[0];
            if len == 255 {
                if items.len() < 5 {
                    break;
                }
                let font_xid = u32::from_be_bytes([items[1], items[2], items[3], items[4]]);
                self.current_font = Some(font_xid);
                items = &items[5..];
                continue;
            }
            let delta = items[1] as i8;
            let len = len as usize;
            if items.len() < 2 + len {
                break;
            }
            let text = &items[2..2 + len];
            cursor_x = cursor_x.saturating_add(i32::from(delta));
            if !text.is_empty() {
                self.render_text_string(host_xid, foreground, cursor_x, y, text)?;
                if let Some(font_state) = self.current_font.and_then(|f| self.fonts.get(&f)) {
                    let advance: i32 = text
                        .iter()
                        .map(|&b| {
                            font_state
                                .char_info_cache
                                .get(&(b as char))
                                .map(|ci| ci.character_width as i32)
                                .unwrap_or(6)
                        })
                        .sum();
                    cursor_x = cursor_x.saturating_add(advance);
                }
            }
            items = &items[2 + len..];
        }
        Ok(())
    }

    fn poly_text16(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + LISTofTEXTITEM16.
        // Each TEXTITEM16 is `len(u8) delta(i8) chars(2*len)` (chars are
        // CHAR2B, big-endian) for len in 0..=254, or `255 font_id(u32 BE)`
        // for a font change. No inter-item padding (only trailing request
        // padding).
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut cursor_x = x;
        let mut items = &body[12..];

        while items.len() >= 2 {
            let len = items[0];
            if len == 255 {
                if items.len() < 5 {
                    break;
                }
                let font_xid = u32::from_be_bytes([items[1], items[2], items[3], items[4]]);
                self.current_font = Some(font_xid);
                items = &items[5..];
                continue;
            }
            let delta = items[1] as i8;
            let len = len as usize;
            let needed = 2 + 2 * len;
            if items.len() < needed {
                break;
            }
            cursor_x = cursor_x.saturating_add(i32::from(delta));
            let mut chars = Vec::with_capacity(len);
            for i in 0..len {
                let codepoint = u16::from_be_bytes([items[2 + 2 * i], items[2 + 2 * i + 1]]) as u32;
                chars.push(char::from_u32(codepoint).unwrap_or('\u{fffd}'));
            }
            if !chars.is_empty() {
                self.render_text_chars(host_xid, foreground, cursor_x, y, &chars)?;
                if let Some(font_state) = self.current_font.and_then(|f| self.fonts.get(&f)) {
                    cursor_x = cursor_x.saturating_add(
                        chars
                            .iter()
                            .map(|ch| {
                                font_state
                                    .char_info_cache
                                    .get(ch)
                                    .map(|ci| ci.character_width as i32)
                                    .unwrap_or(6)
                            })
                            .sum::<i32>(),
                    );
                }
            }
            items = &items[needed..];
        }
        Ok(())
    }

    fn image_text8(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + string(text_len)
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;

        // Draw background rectangle first
        if let Some(font_state) = self.current_font.and_then(|f| self.fonts.get(&f)) {
            let total_width: i32 = body[12..]
                .iter()
                .take(text_len as usize)
                .map(|&b| {
                    font_state
                        .char_info_cache
                        .get(&(b as char))
                        .map(|ci| ci.character_width as i32)
                        .unwrap_or(6)
                })
                .sum();
            let ascent = font_state.metrics.font_ascent as i32;
            let descent = font_state.metrics.font_descent as i32;
            // Clamp to i16/u16 ranges so a buggy font (huge ascent) can't
            // produce a rect that overflows pixman's internal arithmetic.
            let rect = Rectangle16 {
                x: x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                y: (y - ascent).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                width: total_width.clamp(0, u16::MAX as i32) as u16,
                height: (ascent + descent).clamp(0, u16::MAX as i32) as u16,
            };
            let function = self.current_function;
            let bg_rects = self.intersect_with_current_clip(&[rect]);
            if !bg_rects.is_empty() {
                self.try_vk_fill_with_function(host_xid, function, background, &bg_rects);
            }
        }

        // Render the string (clamp to available body bytes)
        let end = (12usize + text_len as usize).min(body.len());
        let text = &body[12..end];
        self.render_text_string(host_xid, foreground, x, y, text)
    }

    fn image_text16(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + CHAR2B[text_len].
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut chars = Vec::with_capacity(text_len as usize);
        let mut pos = 12usize;
        for _ in 0..text_len {
            if pos + 2 > body.len() {
                break;
            }
            let codepoint = u16::from_be_bytes([body[pos], body[pos + 1]]) as u32;
            pos += 2;
            chars.push(char::from_u32(codepoint).unwrap_or('\u{fffd}'));
        }

        if let Some(font_state) = self.current_font.and_then(|f| self.fonts.get(&f)) {
            let total_width: i32 = chars
                .iter()
                .map(|ch| {
                    font_state
                        .char_info_cache
                        .get(ch)
                        .map(|ci| ci.character_width as i32)
                        .unwrap_or(6)
                })
                .sum();
            let ascent = font_state.metrics.font_ascent as i32;
            let descent = font_state.metrics.font_descent as i32;
            let rect = Rectangle16 {
                x: x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                y: (y - ascent).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                width: total_width.clamp(0, u16::MAX as i32) as u16,
                height: (ascent + descent).clamp(0, u16::MAX as i32) as u16,
            };
            let function = self.current_function;
            let bg_rects = self.intersect_with_current_clip(&[rect]);
            if !bg_rects.is_empty() {
                self.try_vk_fill_with_function(host_xid, function, background, &bg_rects);
            }
        }

        self.render_text_chars(host_xid, foreground, x, y, &chars)
    }

    fn render_create_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_drawable: AnyHandle,
        _ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        let drawable_xid = host_drawable.as_raw();
        let picture_xid = self.next_host_xid();
        self.pictures
            .insert(picture_xid, default_drawable_picture(drawable_xid));
        if value_mask != 0 {
            let mut body = Vec::with_capacity(8 + values.len());
            body.extend_from_slice(&picture_xid.to_le_bytes());
            body.extend_from_slice(&value_mask.to_le_bytes());
            body.extend_from_slice(values);
            self.render_change_picture(None, picture_xid, &body)?;
        }
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_change_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        if body.len() < 8 {
            return Ok(());
        }
        let value_mask = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        let values = &body[8..];
        let mut off = 0usize;
        let next_u32 = |off: &mut usize| -> Option<u32> {
            let bytes = values.get(*off..*off + 4)?;
            *off += 4;
            Some(u32::from_le_bytes(bytes.try_into().ok()?))
        };
        for bit in 0..13 {
            let mask_bit = 1u32 << bit;
            if value_mask & mask_bit == 0 {
                continue;
            }
            let Some(v) = next_u32(&mut off) else {
                break;
            };
            match mask_bit {
                // CPRepeat
                0x0001 => {
                    let repeat = match v {
                        1 => Repeat::Normal,
                        2 => Repeat::Pad,
                        3 => Repeat::Reflect,
                        _ => Repeat::None,
                    };
                    match self.pictures.get_mut(&host_pic) {
                        Some(PictureState::Drawable { repeat: r, .. }) => *r = repeat,
                        Some(PictureState::SolidFill { repeat: r, .. }) => *r = repeat,
                        Some(PictureState::Gradient { repeat: r, .. }) => *r = repeat,
                        None => {}
                    }
                }
                // CPAlphaMap
                0x0002 => {
                    if let Some(PictureState::Drawable { alpha_map, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *alpha_map = if v == 0 { None } else { Some(v) };
                    }
                }
                // CPAlphaXOrigin
                0x0004 => {
                    if let Some(PictureState::Drawable { alpha_x, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *alpha_x = v as i16;
                    }
                }
                // CPAlphaYOrigin
                0x0008 => {
                    if let Some(PictureState::Drawable { alpha_y, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *alpha_y = v as i16;
                    }
                }
                // CPClipXOrigin
                0x0010 => {
                    if let Some(PictureState::Drawable { clip_x, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *clip_x = v as i16;
                    }
                }
                // CPClipYOrigin
                0x0020 => {
                    if let Some(PictureState::Drawable { clip_y, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *clip_y = v as i16;
                    }
                }
                // CPClipMask
                0x0040 => {
                    let new_clip = if v == 0 {
                        None
                    } else {
                        self.pixmaps.get(&v).map(|px| {
                            vec![Rectangle16 {
                                x: 0,
                                y: 0,
                                width: px.width,
                                height: px.height,
                            }]
                        })
                    };
                    if let Some(PictureState::Drawable { clip, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *clip = new_clip;
                    }
                }
                // CPGraphicsExposure
                0x0080 => {
                    if let Some(PictureState::Drawable {
                        graphics_exposure, ..
                    }) = self.pictures.get_mut(&host_pic)
                    {
                        *graphics_exposure = v != 0;
                    }
                }
                // CPSubwindowMode
                0x0100 => {
                    if let Some(PictureState::Drawable { subwindow_mode, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *subwindow_mode = v as u8;
                    }
                }
                // CPPolyEdge
                0x0200 => {
                    if let Some(PictureState::Drawable { poly_edge, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *poly_edge = v as u8;
                    }
                }
                // CPPolyMode
                0x0400 => {
                    if let Some(PictureState::Drawable { poly_mode, .. }) =
                        self.pictures.get_mut(&host_pic)
                    {
                        *poly_mode = v as u8;
                    }
                }
                // CPDither: consumed but intentionally not stored.
                0x0800 => {}
                // CPComponentAlpha
                0x1000 => match self.pictures.get_mut(&host_pic) {
                    Some(PictureState::Drawable {
                        component_alpha, ..
                    })
                    | Some(PictureState::SolidFill {
                        component_alpha, ..
                    }) => *component_alpha = v != 0,
                    Some(PictureState::Gradient { .. }) | None => {}
                },
                _ => {}
            }
        }
        Ok(())
    }

    fn render_free_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
    ) -> io::Result<()> {
        // Phase-3B drawable-destruction barrier: a batched paint or
        // in-flight composite may still reference this picture's
        // rescued vk_mirror (if any). Flush before dropping.
        //
        // On strict-flush Err: DO NOT drop the picture or its
        // rescued image. Leave them in place so any GPU references
        // outlive; backend teardown drains.
        crate::vk_count!(pb_picture_destroy);
        if let Err(e) = self
            .flush_if_needed(crate::kms::scheduler::paint_batch::BatchFlushReason::ProtocolBarrier)
        {
            log::error!(
                "render_free_picture: pre-destruction flush failed ({e:?}); leaving picture + rescued image in place to avoid UAF"
            );
            return Err(std::io::Error::other(format!(
                "render_free_picture pre-flush failed: {e:?}"
            )));
        }
        self.pictures.remove(&host_pic);
        self.picture_rescued_images.remove(&host_pic);
        Ok(())
    }

    fn render_create_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>> {
        let format = match ynest_format {
            RENDER_FMT_A8 => GlyphSetFormat::A8,
            RENDER_FMT_A1 => GlyphSetFormat::A1,
            RENDER_FMT_ARGB32 => GlyphSetFormat::Argb32,
            _ => GlyphSetFormat::Other,
        };
        let id = self.next_host_xid();
        log::debug!(
            "render_create_glyphset: client_format=0x{ynest_format:x} -> {format:?} host_gs=0x{id:x}"
        );
        self.glyphsets.insert(
            id,
            GlyphSetState {
                format,
                glyphs: HashMap::new(),
            },
        );
        Ok(GlyphSetHandle::from_raw(id))
    }

    fn render_free_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
    ) -> io::Result<()> {
        self.glyphsets.remove(&host_gs);
        Ok(())
    }

    fn render_add_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
        body_tail: &[u8],
    ) -> io::Result<()> {
        if let Some(gs) = self.glyphsets.get_mut(&host_gs) {
            parse_add_glyphs(gs, body_tail);
        }
        Ok(())
    }

    fn render_free_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
        glyph_ids: &[u8],
    ) -> io::Result<()> {
        let Some(gs) = self.glyphsets.get_mut(&host_gs) else {
            return Ok(());
        };
        for chunk in glyph_ids.chunks_exact(4) {
            let id = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            gs.glyphs.remove(&id);
        }
        Ok(())
    }

    fn render_composite(
        &mut self,
        _origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_mask: u32,
        host_dst: u32,
        src_x: i16,
        src_y: i16,
        mask_x: i16,
        mask_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        // Extract picture-level repeat + transform attributes. The
        // shader handles all repeat modes + the affine portion of
        // any transform; SolidFill / Drawable / Gradient sources are
        // resolved by `resolve_render_pic_with_gradient_xid`.
        let (src_repeat, src_transform, src_filter) = match self.pictures.get(&host_src) {
            Some(PictureState::SolidFill { repeat, .. }) => (*repeat, None, PictureFilter::Nearest),
            Some(PictureState::Gradient {
                repeat, transform, ..
            }) => (*repeat, *transform, PictureFilter::Nearest),
            Some(PictureState::Drawable {
                repeat,
                transform,
                filter,
                ..
            }) => (*repeat, *transform, filter.clone()),
            None => {
                log::debug!("render_composite: host_src 0x{host_src:x} not found");
                return Ok(());
            }
        };
        let (mask_repeat, mask_transform, mask_component_alpha, mask_filter) = if host_mask == 0 {
            (Repeat::None, None, false, PictureFilter::Nearest)
        } else {
            match self.pictures.get(&host_mask) {
                Some(PictureState::SolidFill {
                    repeat,
                    component_alpha,
                    ..
                }) => (*repeat, None, *component_alpha, PictureFilter::Nearest),
                Some(PictureState::Gradient {
                    repeat, transform, ..
                }) => (*repeat, *transform, false, PictureFilter::Nearest),
                Some(PictureState::Drawable {
                    repeat,
                    transform,
                    component_alpha,
                    filter,
                    ..
                }) => (*repeat, *transform, *component_alpha, filter.clone()),
                None => {
                    log::debug!("render_composite: host_mask 0x{host_mask:x} not found");
                    return Ok(());
                }
            }
        };

        // Convolution-filter diagnostic: log when either side carries
        // a convolution kernel so we can see which clients use it on
        // src vs mask (xfwm4 / picom / mutter shadow blur). Phase 2
        // only honors src-side; mask-side falls through silently
        // (kernel ignored).
        if log::log_enabled!(log::Level::Debug) {
            let summarize = |f: &PictureFilter| -> &'static str {
                match f {
                    PictureFilter::Convolution { .. } => "convolution",
                    PictureFilter::Bilinear => "bilinear",
                    PictureFilter::Nearest => "nearest",
                }
            };
            if matches!(src_filter, PictureFilter::Convolution { .. })
                || matches!(mask_filter, PictureFilter::Convolution { .. })
            {
                log::debug!(
                    "render_composite: convolution-bearing composite op={op} \
                     src=0x{host_src:x}/{} mask=0x{host_mask:x}/{} dst=0x{host_dst:x}",
                    summarize(&src_filter),
                    summarize(&mask_filter),
                );
            }
        }

        let (dst_xid, clip) = match self.pictures.get(&host_dst) {
            Some(PictureState::Drawable { host_xid, clip, .. }) => (*host_xid, clip.clone()),
            _ => {
                log::debug!("render_composite: host_dst 0x{host_dst:x} is not a Drawable picture");
                return Ok(());
            }
        };

        // Phase 4.1.5: Vulkan-only Composite path. The shader covers
        // affine transforms + all four repeat modes; SolidFill /
        // Drawable / Gradient sources resolve through
        // `resolve_render_pic_with_gradient_xid`. Unsupported cases
        // (component_alpha, alpha_map, depth-1 src, etc.) silently
        // skip — `feedback_no_gating_during_family_port.md` accepts
        // visual regressions for those during the family port.
        if let Some(src_pic) = resolve_render_pic_with_gradient_xid(&self.pictures, host_src)
            && let Some(mask_pic) = if host_mask == 0 {
                Some(RenderPic::None)
            } else {
                resolve_render_pic_with_gradient_xid(&self.pictures, host_mask)
            }
            && let Some((rects, scissor)) = self.build_render_composite_inputs(
                &clip, src_x, src_y, mask_x, mask_y, dst_x, dst_y, width, height,
            )
        {
            self.try_vk_render_composite(
                op,
                src_pic,
                mask_pic,
                dst_xid,
                &rects,
                scissor,
                src_repeat,
                mask_repeat,
                src_transform,
                mask_transform,
                mask_component_alpha,
                &src_filter,
            );
        }

        Ok(())
    }

    fn render_composite_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _mask_fmt: u32,
        host_gs: u32,
        src_x: i16,
        src_y: i16,
        items: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        // Phase 4.1.5: Vulkan-only. Atlas miss / non-`Over` op /
        // non-SolidFill src silently skip the run.
        self.try_vk_render_composite_glyphs(
            minor, op, host_src, host_dst, host_gs, src_x, src_y, items, x_off, y_off,
        );
        Ok(())
    }

    fn render_fill_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        // Resolve the destination picture to its backing drawable + clip.
        // Solid / gradient pictures aren't valid RENDER fill destinations,
        // so bail quietly in those cases.
        let (dst_drawable_xid, picture_clip) = match self.pictures.get(&host_dst) {
            Some(PictureState::Drawable { host_xid, clip, .. }) => (*host_xid, clip.clone()),
            _ => return Ok(()),
        };

        let mut decoded: Vec<Rectangle16> = Vec::with_capacity(rects.len() / 8);
        for chunk in rects.chunks_exact(8) {
            let rx = i16::from_le_bytes([chunk[0], chunk[1]]).saturating_add(x_off);
            let ry = i16::from_le_bytes([chunk[2], chunk[3]]).saturating_add(y_off);
            let rw = u16::from_le_bytes([chunk[4], chunk[5]]);
            let rh = u16::from_le_bytes([chunk[6], chunk[7]]);
            if rw > 0 && rh > 0 {
                decoded.push(Rectangle16 {
                    x: rx,
                    y: ry,
                    width: rw,
                    height: rh,
                });
            }
        }
        if decoded.is_empty() {
            return Ok(());
        }

        // Phase 4.1.5: Vulkan-only. Solid src + no mask + per-op
        // blend covers every standard PictOp; out-of-format dst
        // silently skips.
        // X RENDER XRenderColor is already premultiplied on the wire
        // (see rendercheck main.c:337-345). Pass through unchanged.
        let color_premul = {
            let r = u16::from_le_bytes([color[0], color[1]]) as f32 / 65535.0;
            let g = u16::from_le_bytes([color[2], color[3]]) as f32 / 65535.0;
            let b = u16::from_le_bytes([color[4], color[5]]) as f32 / 65535.0;
            let a = u16::from_le_bytes([color[6], color[7]]) as f32 / 65535.0;
            [r, g, b, a]
        };
        let Some((_, scissor)) =
            self.build_render_composite_inputs(&picture_clip, 0, 0, 0, 0, 0, 0, 1, 1)
        else {
            log::debug!(
                "render_fill_rectangles bail: build_inputs returned None \
                 (dst=0x{host_dst:x} drawable=0x{dst_drawable_xid:x})"
            );
            return Ok(());
        };
        let composite_rects: Vec<crate::kms::vk::ops::render::CompositeRect> = decoded
            .iter()
            .map(|r| crate::kms::vk::ops::render::CompositeRect {
                src_x: 0,
                src_y: 0,
                mask_x: 0,
                mask_y: 0,
                dst_x: i32::from(r.x),
                dst_y: i32::from(r.y),
                width: u32::from(r.width),
                height: u32::from(r.height),
            })
            .collect();
        let took = self.try_vk_render_composite(
            op,
            RenderPic::Solid(color_premul),
            RenderPic::None,
            dst_drawable_xid,
            &composite_rects,
            scissor,
            Repeat::None,
            Repeat::None,
            None,
            None,
            false,
            &PictureFilter::Nearest,
        );
        if !took {
            log::debug!(
                "render_fill_rectangles bail: try_vk_render_composite returned false \
                 (op={op} dst_drawable=0x{dst_drawable_xid:x} nrects={} color={:?})",
                composite_rects.len(),
                color_premul,
            );
        }
        Ok(())
    }

    fn render_trapezoids(
        &mut self,
        _origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        if !traps.len().is_multiple_of(40) || traps.is_empty() {
            return Ok(());
        }
        let _ = (src_x, src_y);
        // Phase 4.1.5: Vulkan-only. CPU rasterises traps into the
        // R8 mask scratch; the existing Composite pipeline blends the
        // source through the mask. Supports SolidFill, Drawable, and
        // Gradient sources (pixman fallback no longer exists, so a
        // false return drops the request silently — log to surface it).
        if !self.try_vk_render_trapezoids_path(op, host_src, host_dst, traps, x_off, y_off) {
            log::debug!(
                "render_trapezoids: vk path declined op={op} src={host_src:#x} dst={host_dst:#x}"
            );
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_triangles_op(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        primitives: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        let _ = (src_x, src_y);
        // Phase 4.1.5: Vulkan-only. CPU triangle rasteriser fills
        // an R8 coverage mask, then Composite blends src through it.
        // Supports SolidFill, Drawable, and Gradient sources.
        if !self
            .try_vk_render_triangles_path(minor, op, host_src, host_dst, primitives, x_off, y_off)
        {
            log::debug!(
                "render_triangles_op: vk path declined minor={minor} op={op} src={host_src:#x} dst={host_dst:#x}"
            );
        }
        Ok(())
    }

    fn render_create_solid_fill(
        &mut self,
        _origin: Option<OriginContext>,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>> {
        // X RENDER CreateSolidFill color: 16-bit per channel, little-endian,
        // already premultiplied on the wire (see rendercheck main.c:337-345).
        // Byte layout: red[0..2], green[2..4], blue[4..6], alpha[6..8].
        let r16 = u16::from_le_bytes([color[0], color[1]]);
        let g16 = u16::from_le_bytes([color[2], color[3]]);
        let b16 = u16::from_le_bytes([color[4], color[5]]);
        let a16 = u16::from_le_bytes([color[6], color[7]]);
        let r = f32::from(r16) / 65535.0;
        let g = f32::from(g16) / 65535.0;
        let b = f32::from(b16) / 65535.0;
        let a = f32::from(a16) / 65535.0;
        let premul = [r, g, b, a];

        let picture_xid = self.next_host_xid();
        self.pictures.insert(
            picture_xid,
            PictureState::SolidFill {
                premul,
                repeat: Repeat::Normal,
                component_alpha: false,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_linear_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        use crate::kms::vk::gradient::{GradientPicture, Stop};

        if body.len() < 24 {
            return Ok(None);
        }
        let p1x = i32::from_le_bytes(body[4..8].try_into().unwrap());
        let p1y = i32::from_le_bytes(body[8..12].try_into().unwrap());
        let p2x = i32::from_le_bytes(body[12..16].try_into().unwrap());
        let p2y = i32::from_le_bytes(body[16..20].try_into().unwrap());
        let n_stops = u32::from_le_bytes(body[20..24].try_into().unwrap()) as usize;
        let pos_base = 24usize;
        let color_base = pos_base + n_stops * 4;
        if body.len() < color_base + n_stops * 8 {
            return Ok(None);
        }
        let mut stops: Vec<Stop> = Vec::with_capacity(n_stops);
        for i in 0..n_stops {
            let pos = i32::from_le_bytes(
                body[pos_base + i * 4..pos_base + i * 4 + 4]
                    .try_into()
                    .unwrap(),
            );
            let cb = color_base + i * 8;
            let r = u16::from_le_bytes(body[cb..cb + 2].try_into().unwrap());
            let g = u16::from_le_bytes(body[cb + 2..cb + 4].try_into().unwrap());
            let b = u16::from_le_bytes(body[cb + 4..cb + 6].try_into().unwrap());
            let a = u16::from_le_bytes(body[cb + 6..cb + 8].try_into().unwrap());
            stops.push(Stop { pos, r, g, b, a });
        }
        let Some(vkctx) = self.vk.as_ref().cloned() else {
            log::debug!("render_create_linear_gradient: vulkan unavailable; cannot create");
            return Ok(None);
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return Ok(None);
        };
        // No pre-build flush: the new gradient has a fresh XID, so
        // GradientPicture::new_linear's one-shot upload CB can't race
        // anything in the open paint batch. Telemetry on bee/fuji
        // showed gradient create rates of 50-90/sec — strict-flushing
        // each one cost a queue submit + wait per gradient.
        let gradient =
            match GradientPicture::new_linear(vkctx, pool_handle, (p1x, p1y), (p2x, p2y), &stops) {
                Ok(g) => g,
                Err(e) => {
                    log::warn!("render_create_linear_gradient: vk init failed: {e:?}");
                    return Ok(None);
                }
            };
        let picture_xid = self.next_host_xid();
        self.pictures.insert(
            picture_xid,
            PictureState::Gradient {
                gradient,
                repeat: Repeat::None,
                transform: None,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_radial_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        if body.len() < 32 {
            return Ok(None);
        }
        use crate::kms::vk::gradient::{GradientPicture, Stop};

        let icx = i32::from_le_bytes(body[4..8].try_into().unwrap());
        let icy = i32::from_le_bytes(body[8..12].try_into().unwrap());
        let ocx = i32::from_le_bytes(body[12..16].try_into().unwrap());
        let ocy = i32::from_le_bytes(body[16..20].try_into().unwrap());
        let ir = i32::from_le_bytes(body[20..24].try_into().unwrap());
        let or_ = i32::from_le_bytes(body[24..28].try_into().unwrap());
        let n_stops = u32::from_le_bytes(body[28..32].try_into().unwrap()) as usize;
        let pos_base = 32usize;
        let color_base = pos_base + n_stops * 4;
        if body.len() < color_base + n_stops * 8 {
            return Ok(None);
        }
        let mut stops: Vec<Stop> = Vec::with_capacity(n_stops);
        for i in 0..n_stops {
            let pos = i32::from_le_bytes(
                body[pos_base + i * 4..pos_base + i * 4 + 4]
                    .try_into()
                    .unwrap(),
            );
            let cb = color_base + i * 8;
            let r = u16::from_le_bytes(body[cb..cb + 2].try_into().unwrap());
            let g = u16::from_le_bytes(body[cb + 2..cb + 4].try_into().unwrap());
            let b = u16::from_le_bytes(body[cb + 4..cb + 6].try_into().unwrap());
            let a = u16::from_le_bytes(body[cb + 6..cb + 8].try_into().unwrap());
            stops.push(Stop { pos, r, g, b, a });
        }
        let Some(vkctx) = self.vk.as_ref().cloned() else {
            return Ok(None);
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return Ok(None);
        };
        // No pre-build flush — see render_create_linear_gradient for
        // the rationale; same fresh-XID argument applies.
        let gradient = match GradientPicture::new_radial(
            vkctx,
            pool_handle,
            (icx, icy, ir),
            (ocx, ocy, or_),
            &stops,
        ) {
            Ok(g) => g,
            Err(e) => {
                log::warn!("render_create_radial_gradient: vk init failed: {e:?}");
                return Ok(None);
            }
        };
        let picture_xid = self.next_host_xid();
        self.pictures.insert(
            picture_xid,
            PictureState::Gradient {
                gradient,
                repeat: Repeat::None,
                transform: None,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_src_pic: PictureHandle,
        x: u16,
        y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        let pic_xid = host_src_pic.as_raw();
        let host_xid = match self.pictures.get(&pic_xid) {
            Some(PictureState::Drawable { host_xid, .. }) => *host_xid,
            other => {
                log::debug!(
                    "render_create_cursor: pic {pic_xid} not found or not Drawable (got {:?})",
                    other.map(|_| "non-Drawable")
                );
                return Ok(None);
            }
        };

        // fvwm pattern: CreatePixmap → PutImage → CreatePicture →
        // FreePixmap → CreateCursor. By the time CreateCursor lands,
        // the pixmap may already be freed; in that case `free_pixmap`
        // moved its `vk_mirror` into `picture_rescued_images`.
        //
        // Two source-shape cases, deliberately handled separately:
        //   1. Live pixmap: we need a `&mut DrawableImage` pointing
        //      into `self.pixmaps`. Disjoint borrows let us do the
        //      copy without taking ownership.
        //   2. Rescued mirror: owned locally; consumed by the copy
        //      and dropped (with its VkImage) afterwards.
        //
        // Both cases produce the cursor mirror via vkCmdCopyImage —
        // no pixman path either way.
        let id = self.next_host_xid();

        if let Some(rescued) = self.picture_rescued_images.remove(&pic_xid) {
            log::debug!("render_create_cursor: using rescued mirror for pic {pic_xid}");
            let mut src = rescued;
            let cw = src.extent.width;
            let ch = src.extent.height;
            let vk_mirror = self.copy_drawable_to_new_cursor_mirror(&mut src);
            // Phase-3B drawable-destruction barrier:
            // `copy_drawable_to_new_cursor_mirror` (T3-migrated)
            // records into the batch and returns; `src` is about to
            // drop, destroying the rescued mirror's VkImage. Flush
            // the batch before the drop so the GPU has consumed the
            // copy.
            //
            // On strict-flush Err: the GPU may still reference src
            // (path-2 leak) AND the new cursor mirror (also pending
            // in the abandoned batch). Leak both via `mem::forget`
            // and bail out — `picture_rescued_images.remove()`
            // already consumed src so we can't put it back, but
            // forgetting it prevents the Drop from freeing the
            // VkImage the GPU may still hold.
            crate::vk_count!(pb_cursor_picture);
            if let Err(e) = self.flush_if_needed(
                crate::kms::scheduler::paint_batch::BatchFlushReason::ProtocolBarrier,
            ) {
                log::error!(
                    "render_create_cursor: post-copy flush failed ({e:?}); leaking rescued src + new cursor mirror to avoid UAF"
                );
                std::mem::forget(src);
                if let Some(m) = vk_mirror {
                    std::mem::forget(m);
                }
                return Ok(None);
            }
            // `src` drops here, releasing the rescued mirror's VkImage.
            self.cursors.insert(
                id,
                CursorState {
                    extent: ash::vk::Extent2D {
                        width: cw,
                        height: ch,
                    },
                    hot_x: x,
                    hot_y: y,
                    vk_mirror,
                },
            );
        } else if let Some(pm) = self.pixmaps.get(&host_xid) {
            let Some((cw, ch)) = pm
                .vk_mirror
                .as_ref()
                .map(|m| (m.extent.width, m.extent.height))
            else {
                log::debug!(
                    "render_create_cursor: pixmap host_xid={host_xid} has no mirror for pic {pic_xid}"
                );
                return Ok(None);
            };
            // Allocate the cursor mirror first (immutable self.vk
            // borrow), then re-borrow self.pixmaps mutably for the
            // src side of the copy — disjoint fields, two `&mut`s OK.
            let Some(mut cursor_mirror) = self.allocate_cursor_mirror(cw, ch) else {
                return Ok(None);
            };
            if let Err(e) = self.copy_pixmap_mirror_to_cursor(host_xid, &mut cursor_mirror, cw, ch)
            {
                log::warn!("render_create_cursor: mirror copy failed: {e:?}");
            }
            self.cursors.insert(
                id,
                CursorState {
                    extent: ash::vk::Extent2D {
                        width: cw,
                        height: ch,
                    },
                    hot_x: x,
                    hot_y: y,
                    vk_mirror: Some(cursor_mirror),
                },
            );
        } else {
            log::debug!(
                "render_create_cursor: pixmap host_xid={host_xid} not found for pic {pic_xid}"
            );
            return Ok(None);
        }

        CursorHandle::from_raw(id)
            .map(Some)
            .ok_or_else(|| io::Error::other("cursor handle overflow"))
    }

    fn render_set_picture_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Wire body (passed through from nested.rs): picture(4) +
        // clip_x_origin(INT16) + clip_y_origin(INT16) + N × [x y w h].
        // Pre-shift each rectangle by the clip-origin so the stored list
        // is already in dst-coords; the clip-region path doesn't track
        // origin separately.
        if body.len() < 8 {
            return Ok(());
        }
        let x_origin = i16::from_le_bytes([body[4], body[5]]) as i32;
        let y_origin = i16::from_le_bytes([body[6], body[7]]) as i32;
        let rects_data = &body[8..];
        let mut rects = Vec::with_capacity(rects_data.len() / 8);
        for chunk in rects_data.chunks_exact(8) {
            let x = (i16::from_le_bytes([chunk[0], chunk[1]]) as i32 + x_origin)
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            let y = (i16::from_le_bytes([chunk[2], chunk[3]]) as i32 + y_origin)
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            let w = u16::from_le_bytes([chunk[4], chunk[5]]);
            let h = u16::from_le_bytes([chunk[6], chunk[7]]);
            rects.push(Rectangle16 {
                x,
                y,
                width: w,
                height: h,
            });
        }
        if let Some(PictureState::Drawable { clip, .. }) = self.pictures.get_mut(&host_pic) {
            *clip = if rects.is_empty() { None } else { Some(rects) };
        }
        // SolidFill pictures: clip is a no-op.
        Ok(())
    }

    fn render_set_picture_filter(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Body layout (after the 4-byte picture xid already consumed
        // by the dispatcher): name_length:u16, 2 pad bytes, name
        // (STRING8 padded to multiple of 4), values (LISTofFIXED).
        // The dispatcher (`process_request.rs` opcode 30 handler)
        // passed `body` including the leading pid; skip those 4 bytes.
        if body.len() < 4 {
            return Ok(());
        }
        let filter_body = &body[4..];
        let Some(filter) = PictureFilter::parse(filter_body) else {
            log::debug!(
                "render_set_picture_filter: ignoring unknown / malformed filter for picture 0x{host_pic:x}"
            );
            return Ok(());
        };
        let filter_summary = match &filter {
            PictureFilter::Nearest => "nearest".to_string(),
            PictureFilter::Bilinear => "bilinear".to_string(),
            PictureFilter::Convolution { width, height, .. } => {
                format!("convolution {width}x{height}")
            }
        };
        let picture_kind = match self.pictures.get(&host_pic) {
            Some(PictureState::Drawable { .. }) => "Drawable",
            Some(PictureState::SolidFill { .. }) => "SolidFill",
            Some(PictureState::Gradient { .. }) => "Gradient",
            None => "<absent>",
        };
        log::debug!(
            "render_set_picture_filter: picture=0x{host_pic:x} kind={picture_kind} \
             filter={filter_summary}"
        );
        if let Some(PictureState::Drawable { filter: slot, .. }) = self.pictures.get_mut(&host_pic)
        {
            *slot = filter;
        }
        // SolidFill / Gradient pictures don't carry a filter (their
        // sampling is fully shader-side); ignore for those types.
        Ok(())
    }

    fn render_set_picture_transform(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        if body.len() < 40 {
            return Ok(());
        }
        let mut matrix = [[0i32; 3]; 3];
        for (idx, slot) in matrix.iter_mut().flatten().enumerate() {
            let off = 4 + idx * 4;
            *slot = i32::from_le_bytes(body[off..off + 4].try_into().unwrap());
        }
        let transform = if matrix == [[0x10000, 0, 0], [0, 0x10000, 0], [0, 0, 0x10000]] {
            None
        } else {
            Some(PictTransform { matrix })
        };
        match self.pictures.get_mut(&host_pic) {
            Some(PictureState::Drawable { transform: t, .. })
            | Some(PictureState::Gradient { transform: t, .. }) => *t = transform,
            _ => {}
        }
        Ok(())
    }

    fn render_query_version(&mut self, _origin: Option<OriginContext>) -> io::Result<(u32, u32)> {
        // RENDER protocol versions are major=0, minor=N. Current
        // upstream is 0.11; rendercheck rejects anything with major≠0.
        Ok((0, 11))
    }

    fn xkb_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        _body: &[u8],
    ) -> io::Result<Option<Vec<u8>>> {
        // XKB minor opcodes per `xkbproto` / `xcb/xkb.xml`. Reply
        // minors get a body; **void** minors must produce no reply
        // at all — clients that use `_checked` variants call
        // `xcb_request_check`, which asserts `!reply` and aborts
        // the client (`xcb_in.c:757` — what we just saw kill
        // wezterm on `XkbSelectEvents`).
        //
        // Reply minors: 0 UseExtension, 4 GetState, 6 GetControls,
        // 8 GetMap, 10 GetCompatMap, 12 GetIndicatorState,
        // 13 GetIndicatorMap, 15 GetNamedIndicator, 17 GetNames,
        // 19 GetGeometry, 21 PerClientFlags, 22 ListComponents,
        // 23 GetKbdByName, 24 GetDeviceInfo, 101 SetDebuggingFlags.
        // Void minors: 1 SelectEvents, 3 Bell, 5 LatchLockState,
        // 7 SetControls, 9 SetMap, 11 SetCompatMap,
        // 14 SetIndicatorMap, 16 SetNamedIndicator, 18 SetNames,
        // 20 SetGeometry, 25 SetDeviceInfo.
        use crate::kms::xkb as xkb_replies;
        let reply = match minor {
            // Reply minors with a real-data path.
            0 => Some(xkb_replies::reply_use_extension()),
            6 => Some(xkb_replies::reply_get_controls(&self.xkb_keymap.0)),
            8 => Some(xkb_replies::reply_get_map(&self.xkb_keymap.0)),
            10 => Some(xkb_replies::reply_get_compat_map()),
            17 => Some(xkb_replies::reply_get_names(&self.xkb_keymap.0)),
            24 => Some(xkb_replies::reply_get_device_info()),
            // Reply minors we don't model — answer with a minimal
            // 32-byte zero reply so xcb completes the cookie.
            4 | 12 | 13 | 15 | 19 | 21 | 22 | 23 | 101 => Some(xkb_replies::reply_minimal(minor)),
            // Void minors — return None so no reply hits the wire.
            1 | 3 | 5 | 7 | 9 | 11 | 14 | 16 | 18 | 20 | 25 => None,
            // Unknown minor — be defensive and stay silent.
            _ => {
                log::debug!("xkb: unknown minor {minor}, no reply sent");
                None
            }
        };
        Ok(reply)
    }

    fn xfixes_change_cursor_by_name(
        &mut self,
        _origin: Option<OriginContext>,
        _host_cursor_xid: u32,
        _name_bytes: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn set_shape_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        kind: u8,
        rects: &[xfixes::RegionRect],
    ) -> io::Result<()> {
        let map = match kind {
            0 => &mut self.shape_bounding,
            1 => &mut self.shape_clip,
            2 => &mut self.shape_input,
            _ => return Ok(()),
        };
        // Store always: server sends the full window rect when shape is cleared
        // (shape_rects_for fallback), and the actual rects otherwise.
        // Empty vec = window clips to nothing (explicitly shaped to empty region).
        map.insert(host_xid, rects.to_vec());
        Ok(())
    }

    fn warp_pointer(
        &mut self,
        _origin: Option<OriginContext>,
        dst_host_xid: u32,
        dst_x: i16,
        dst_y: i16,
    ) -> io::Result<()> {
        let (base_x, base_y) = if dst_host_xid == 0 {
            (self.cursor_x, self.cursor_y)
        } else if let Some(w) = self.windows.get(&dst_host_xid) {
            (w.x as f32, w.y as f32)
        } else {
            return Ok(());
        };

        self.cursor_x = (base_x + dst_x as f32).clamp(0.0, self.fb_w as f32 - 1.0);
        self.cursor_y = (base_y + dst_y as f32).clamp(0.0, self.fb_h as f32 - 1.0);
        // XWarpPointer doesn't have ServerState in the Backend trait
        // scope, so we can't compute the spec-correct crossing chain
        // here today. Emit a motion event only; warp-driven crossings
        // are filed as a followup (the chain would mirror what
        // `update_pointer_window` does for libinput motion).
        let host_xid = self.window_under_cursor().unwrap_or(self.window_id);
        let mask = self.serialize_modifiers() | self.button_mask;
        self.emit_motion_only(host_xid, mask);
        Ok(())
    }

    fn query_pointer(&mut self, _origin: Option<OriginContext>) -> io::Result<PointerPosition> {
        Ok(PointerPosition {
            same_screen: true,
            win_x: self.cursor_x as i16,
            win_y: self.cursor_y as i16,
            mask: self.serialize_modifiers(),
        })
    }

    fn list_fonts_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<u8>> {
        let cap = usize::from(max_names);
        let names: Vec<&str> = self
            .font_loader
            .catalog
            .iter()
            .map(String::as_str)
            .filter(|n| xlfd_pattern_matches(pattern, n))
            .take(cap)
            .collect();

        // Layout: 32-byte header + string items, each: 1-byte length + name bytes.
        let mut name_data: Vec<u8> = Vec::new();
        for name in &names {
            name_data.push(u8::try_from(name.len()).unwrap_or(u8::MAX));
            name_data.extend_from_slice(name.as_bytes());
        }
        let pad = (4 - (name_data.len() % 4)) % 4;
        name_data.resize(name_data.len() + pad, 0);

        let extra_words = u32::try_from(name_data.len() / 4).unwrap_or(0);
        let mut reply = vec![0u8; 32 + name_data.len()];
        reply[0] = 1;
        // bytes [2..4] sequence: rewritten by caller
        reply[4..8].copy_from_slice(&extra_words.to_le_bytes());
        reply[8..10].copy_from_slice(&u16::try_from(names.len()).unwrap_or(u16::MAX).to_le_bytes());
        reply[32..].copy_from_slice(&name_data);
        Ok(reply)
    }

    fn list_fonts_with_info_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<Vec<u8>>> {
        // For each catalog entry that matches, open the real font and emit
        // a reply with FreeType-derived metrics. libXt's XCreateFontSet
        // validates min_bounds/max_bounds/font_ascent against the
        // candidate before letting it into a fontset; stub bounds get
        // rejected. This path mirrors what QueryFont returns so the LFWI
        // metrics agree with later QueryFont metrics on the same XLFD.
        let cap = usize::from(max_names);
        let matched: Vec<String> = self
            .font_loader
            .catalog
            .iter()
            .filter(|n| xlfd_pattern_matches(pattern, n))
            .take(cap)
            .cloned()
            .collect();

        let mut entries: Vec<(String, FontMetrics)> = Vec::with_capacity(matched.len());
        for name in matched {
            match self.font_loader.open_font(&name) {
                Ok((_face, metrics, _cache)) => entries.push((name, metrics)),
                Err(err) => {
                    log::debug!("ListFontsWithInfo: skipping {name:?} — open_font: {err}");
                }
            }
        }

        let total = entries.len();
        let mut replies: Vec<Vec<u8>> = Vec::with_capacity(total + 1);
        for (idx, (name, metrics)) in entries.iter().enumerate() {
            // replies-hint excludes both this reply and the trailing
            // terminator, per the X11 spec.
            let remaining = u32::try_from(total - idx - 1).unwrap_or(0);
            let mut buf = Vec::new();
            yserver_protocol::x11::write_list_fonts_with_info_reply(
                &mut buf,
                yserver_protocol::x11::ClientByteOrder::LittleEndian,
                yserver_protocol::x11::SequenceNumber(0),
                metrics,
                name,
                remaining,
            )?;
            replies.push(buf);
        }
        let mut term = Vec::new();
        yserver_protocol::x11::write_list_fonts_with_info_terminator(
            &mut term,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            yserver_protocol::x11::SequenceNumber(0),
        )?;
        replies.push(term);
        Ok(replies)
    }

    fn get_atom_name(
        &mut self,
        _origin: Option<OriginContext>,
        _atom: u32,
    ) -> io::Result<Option<String>> {
        Ok(None)
    }

    fn get_keyboard_mapping(
        &mut self,
        _origin: Option<OriginContext>,
        first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)> {
        // X11 GetKeyboardMapping: per keycode, return a flat row of
        // keysyms across shift levels in the order
        // unshifted/shifted/mode-switch-unshifted/mode-switch-shifted.
        // Apps combine the keycode they received with the modifier
        // bits in the event's `state` field to pick the right slot.
        // Returning only level 0 means apps can never produce
        // shifted characters — typing Shift+a yields 'a' instead of
        // 'A' because that's the only keysym we ever expose.
        const LEVELS: usize = 4;
        let max_kc = u16::from(first_keycode) + u16::from(count);
        let mut flat = Vec::with_capacity(usize::from(count) * LEVELS);
        for kc in u16::from(first_keycode)..max_kc {
            let xkb_kc = xkbcommon::xkb::Keycode::new(u32::from(kc));
            for level in 0..LEVELS as u32 {
                let syms = self.xkb_keymap.0.key_get_syms_by_level(xkb_kc, 0, level);
                flat.push(syms.first().map_or(0, |s| s.raw()));
            }
        }
        Ok((LEVELS as u8, flat))
    }

    fn get_modifier_mapping(
        &mut self,
        _origin: Option<OriginContext>,
    ) -> io::Result<(u8, Vec<u8>)> {
        // Conventional defaults: 8 rows, up to 4 keycodes each
        // Shift(0x32,0x3E), Lock(0x42), Control(0x25,0x69),
        // Mod1(0x40,0x6C), Mod2(0x4D), Mod3(0x73), Mod4(0x85,0x86), Mod5(empty)
        // Encoded as count + flat vec of 8*4 = 32 bytes
        let data: Vec<u8> = vec![
            0x32, 0x3E, 0, 0, // Shift
            0x42, 0, 0, 0, // Lock
            0x25, 0x69, 0, 0, // Control
            0x40, 0x6C, 0, 0, // Mod1
            0x4D, 0, 0, 0, // Mod2
            0x73, 0, 0, 0, // Mod3
            0x85, 0x86, 0, 0, // Mod4
            0, 0, 0, 0, // Mod5
        ];
        Ok((4, data))
    }
}

/// Parse an AddGlyphs `body_tail` and insert glyphs into `gs`.
/// `body_tail` is everything after the 4-byte glyphset XID.
pub(super) fn parse_add_glyphs(gs: &mut GlyphSetState, body_tail: &[u8]) {
    if !matches!(
        gs.format,
        GlyphSetFormat::A8 | GlyphSetFormat::A1 | GlyphSetFormat::Argb32
    ) {
        log::debug!(
            "parse_add_glyphs bail: format={:?} (only A8/A1/ARGB32 supported) — {} glyphs lost",
            gs.format,
            body_tail
                .get(..4)
                .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        );
        return;
    }
    if body_tail.len() < 4 {
        return;
    }
    let n = u32::from_le_bytes([body_tail[0], body_tail[1], body_tail[2], body_tail[3]]) as usize;
    let ids_end = 4 + n * 4;
    let infos_end = ids_end + n * 12;
    if body_tail.len() < infos_end {
        return;
    }

    let id_chunks = body_tail[4..ids_end].chunks_exact(4);
    let info_chunks = body_tail[ids_end..infos_end].chunks_exact(12);
    let mut data_off = infos_end;

    for (id_b, info_b) in id_chunks.zip(info_chunks) {
        let id = u32::from_le_bytes([id_b[0], id_b[1], id_b[2], id_b[3]]);
        let width = u16::from_le_bytes([info_b[0], info_b[1]]);
        let height = u16::from_le_bytes([info_b[2], info_b[3]]);
        let x = i16::from_le_bytes([info_b[4], info_b[5]]);
        let y = i16::from_le_bytes([info_b[6], info_b[7]]);
        let x_off = i16::from_le_bytes([info_b[8], info_b[9]]);
        let y_off = i16::from_le_bytes([info_b[10], info_b[11]]);

        let w = width as usize;
        let h = height as usize;
        let stride = match gs.format {
            GlyphSetFormat::A8 => (w + 3) & !3,
            GlyphSetFormat::A1 => w.div_ceil(32) * 4,
            // CARD32 per pixel; row size always 4-aligned, no per-row pad.
            GlyphSetFormat::Argb32 => w * 4,
            GlyphSetFormat::Other => return,
        };
        let nbytes = stride * h;
        if data_off + nbytes > body_tail.len() {
            break;
        }
        let wire = &body_tail[data_off..data_off + nbytes];
        // For ARGB32 we extract the alpha byte from each pixel into a
        // densely-packed A8 buffer and record the stored glyph as A8.
        // The downstream atlas + text pipeline path then handles it
        // identically to a real A8 upload.
        let (pixels, stored_format) = match gs.format {
            GlyphSetFormat::A8 => {
                let mut pixels = vec![0u8; w * h];
                for row in 0..h {
                    pixels[row * w..row * w + w]
                        .copy_from_slice(&wire[row * stride..row * stride + w]);
                }
                (pixels, GlyphSetFormat::A8)
            }
            GlyphSetFormat::A1 => (wire.to_vec(), GlyphSetFormat::A1),
            GlyphSetFormat::Argb32 => {
                // Pixel bytes per X RENDER ARGB32 = little-endian
                // CARD32 with alpha-shift=24 → memory order [B, G, R, A].
                let mut pixels = vec![0u8; w * h];
                for row in 0..h {
                    let row_off = row * stride;
                    for col in 0..w {
                        pixels[row * w + col] = wire[row_off + col * 4 + 3];
                    }
                }
                (pixels, GlyphSetFormat::A8)
            }
            GlyphSetFormat::Other => return,
        };
        data_off += nbytes;
        gs.glyphs.insert(
            id,
            StoredGlyph {
                width,
                height,
                x,
                y,
                x_off,
                y_off,
                pixels,
                format: stored_format,
            },
        );
    }
}

// `composite_glyphs_onto` (pixman CompositeGlyphs path) deleted in
// 4.1.5. The Vk path in `try_vk_render_composite_glyphs` (using the
// 4.1.4.5 atlas + text pipeline) is the sole CompositeGlyphs path.

#[cfg(test)]
mod tests {
    use super::{Rectangle16, Repeat};
    use yserver_core::backend::Backend;
    use yserver_protocol::x11::ResourceId;

    use super::{AliasEntry, AliasRegistry, KmsBackend, OutputLayout, WindowState};
    use yserver_core::backend::PixmapHandle;

    #[test]
    fn kms_backend_advertises_composite_opcode_144() {
        let backend = KmsBackend::for_tests();
        assert_eq!(backend.composite_opcode(), Some(144));
    }

    #[test]
    fn allocate_redirected_backing_seeds_refcount_and_map() {
        // L2 plan B.6a — backend allocates a backing for a host
        // window XID, inserts into alias_registry at refcount=1,
        // and registers in host_window_to_backing for later
        // NameWindowPixmap lookups.
        let mut backend = KmsBackend::for_tests();
        let host_window = yserver_core::backend::WindowHandle::from_raw_panicking(0x100_0010);
        let backing = backend
            .allocate_redirected_backing(None, host_window, 200, 150, 24)
            .expect("allocate_redirected_backing");
        let entry = backend
            .alias_registry
            .get(backing)
            .copied()
            .expect("alias entry present");
        assert_eq!(entry.refcount, 1);
        assert_eq!(entry.width, 200);
        assert_eq!(entry.height, 150);
        assert_eq!(entry.depth, 24);
        assert_eq!(
            backend
                .host_window_to_backing
                .get(&host_window.as_raw())
                .copied(),
            Some(backing)
        );
    }

    #[test]
    fn name_window_pixmap_aliases_existing_backing_with_incref() {
        // L2 plan B.5 — NameWindowPixmap on a redirected window
        // returns the same backing handle and bumps the refcount.
        let mut backend = KmsBackend::for_tests();
        let host_window = yserver_core::backend::WindowHandle::from_raw_panicking(0x100_0011);
        let backing = backend
            .allocate_redirected_backing(None, host_window, 100, 50, 24)
            .expect("allocate");
        let pre = backend.alias_registry.get(backing).map(|e| e.refcount);
        let aliased = backend
            .name_window_pixmap(None, host_window)
            .expect("alias");
        assert_eq!(aliased, backing);
        let post = backend.alias_registry.get(backing).map(|e| e.refcount);
        assert_eq!(post, pre.map(|r| r + 1));
    }

    #[test]
    fn unredirect_drops_backing_when_no_alias_holds_it() {
        let mut backend = KmsBackend::for_tests();
        let host_window = yserver_core::backend::WindowHandle::from_raw_panicking(0x100_0020);
        let backing = backend
            .allocate_redirected_backing(None, host_window, 32, 32, 24)
            .expect("allocate");
        backend
            .release_redirected_backing(None, backing)
            .expect("release");
        assert!(backend.alias_registry.get(backing).is_none());
        assert!(backend.host_window_to_backing.is_empty());
    }

    #[test]
    fn unredirect_keeps_backing_alive_while_alias_holds_it() {
        let mut backend = KmsBackend::for_tests();
        let host_window = yserver_core::backend::WindowHandle::from_raw_panicking(0x100_0021);
        let backing = backend
            .allocate_redirected_backing(None, host_window, 32, 32, 24)
            .expect("allocate");
        // Alias bumps refcount to 2.
        let _aliased = backend
            .name_window_pixmap(None, host_window)
            .expect("alias");
        // Release drops reason-1 hold → refcount goes 2 → 1, entry stays.
        backend
            .release_redirected_backing(None, backing)
            .expect("release");
        assert_eq!(
            backend.alias_registry.get(backing).map(|e| e.refcount),
            Some(1)
        );
    }

    #[test]
    fn name_window_pixmap_errors_when_window_not_redirected() {
        let mut backend = KmsBackend::for_tests();
        let host_window = yserver_core::backend::WindowHandle::from_raw_panicking(0x100_0012);
        let err = backend.name_window_pixmap(None, host_window).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn alias_registry_refcount_lifecycle() {
        let mut reg = AliasRegistry::default();
        let h = PixmapHandle::from_raw_panicking(0x77);
        reg.insert(
            h,
            AliasEntry {
                refcount: 1,
                width: 100,
                height: 50,
                depth: 24,
            },
        );
        reg.incref(h);
        assert_eq!(reg.get(h).map(|e| e.refcount), Some(2));
        // first decref: 2 → 1 (still referenced).
        assert!(!reg.decref(h));
        // second decref: 1 → 0 → removed.
        assert!(reg.decref(h));
        assert!(reg.get(h).is_none());
        assert!(reg.is_empty());
    }

    // ---------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------

    fn make_test_backend() -> KmsBackend {
        KmsBackend::for_tests()
    }

    fn make_test_window(x: i16, y: i16, width: u16, height: u16, mapped: bool) -> WindowState {
        WindowState {
            _nested_id: ResourceId(0x0000_0100),
            x,
            y,
            width,
            height,
            border_width: 0,
            mapped,
            _override_redirect: false,
            _parent: Some(1),
            children: Vec::new(),
            bg_pixel: None,
            bg_pixmap: None,
            depth: 24,
            visual: 0,
            cursor: 0,
            vk_mirror: None,
        }
    }

    // pixman test helpers and gated drawing-op tests removed in
    // 4.1.5: their target paths (`copy_plane`, `poly_text16`,
    // `image_text16`, `fill_rects_with_gc_function`,
    // `composite_glyphs_onto`) are all Vk-only now and exercised
    // by rendercheck under the lavapipe smoke harness.

    #[test]
    fn change_picture_cprepeat_updates_drawable_repeat() {
        let mut b = make_test_backend();
        let pixmap_xid = 0x0040_3000;
        let pic_xid = 0x0040_3001;
        b.pixmaps.insert(
            pixmap_xid,
            super::PixmapState {
                handle: pixmap_xid,
                width: 4,
                height: 4,
                depth: 32,
                vk_mirror: None,
            },
        );
        b.pictures
            .insert(pic_xid, super::default_drawable_picture(pixmap_xid));

        let mut body = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        body.extend_from_slice(&0x0001u32.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes());
        b.render_change_picture(None, pic_xid, &body).unwrap();

        match b.pictures.get(&pic_xid).unwrap() {
            super::PictureState::Drawable { repeat, .. } => {
                assert!(matches!(repeat, Repeat::Normal));
            }
            _ => panic!("expected drawable picture"),
        }
    }

    #[test]
    fn change_picture_cpclipmask_zero_clears_clip() {
        let mut b = make_test_backend();
        let pixmap_xid = 0x0040_3010;
        let pic_xid = 0x0040_3011;
        b.pixmaps.insert(
            pixmap_xid,
            super::PixmapState {
                handle: pixmap_xid,
                width: 4,
                height: 4,
                depth: 32,
                vk_mirror: None,
            },
        );
        b.pictures
            .insert(pic_xid, super::default_drawable_picture(pixmap_xid));
        if let Some(super::PictureState::Drawable { clip, .. }) = b.pictures.get_mut(&pic_xid) {
            *clip = Some(vec![Rectangle16 {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }]);
        }

        let mut body = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        body.extend_from_slice(&0x0040u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        b.render_change_picture(None, pic_xid, &body).unwrap();

        match b.pictures.get(&pic_xid).unwrap() {
            super::PictureState::Drawable { clip, .. } => assert!(clip.is_none()),
            _ => panic!("expected drawable picture"),
        }
    }

    // `linear_gradient_composite_produces_nonzero_pixels` removed:
    // gradient pictures now require a Vulkan context for creation
    // (`GradientPicture::new_linear`), which the unit-test backend
    // doesn't provide. The Vk gradient path is exercised by the
    // rendercheck `gradients` suite under `just yserver-venus`.

    #[test]
    fn set_picture_transform_stores_non_identity_matrix() {
        let mut b = make_test_backend();
        let pixmap_xid = 0x0040_5000;
        let pic_xid = 0x0040_5001;
        b.pixmaps.insert(
            pixmap_xid,
            super::PixmapState {
                handle: pixmap_xid,
                width: 4,
                height: 4,
                depth: 32,
                vk_mirror: None,
            },
        );
        b.pictures
            .insert(pic_xid, super::default_drawable_picture(pixmap_xid));

        let mut body = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        for value in [0x20000i32, 0, 0, 0, 0x10000, 0, 0, 0, 0x10000] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        b.render_set_picture_transform(None, pic_xid, &body)
            .unwrap();

        match b.pictures.get(&pic_xid).unwrap() {
            super::PictureState::Drawable { transform, .. } => assert!(transform.is_some()),
            _ => panic!("expected drawable picture"),
        }
    }

    #[test]
    fn warp_pointer_updates_cursor_position() {
        let mut b = make_test_backend();
        let xid = b.next_host_xid;
        b.next_host_xid += 1;
        b.windows
            .insert(xid, make_test_window(100, 200, 300, 200, true));
        b.top_level_order.push(xid);

        b.warp_pointer(None, xid, 10, 20).unwrap();

        assert_eq!(b.cursor_x as i32, 110);
        assert_eq!(b.cursor_y as i32, 220);
    }

    // ---------------------------------------------------------------------------
    // Step 4 — multi-monitor: per-output bbox pre-filter
    // ---------------------------------------------------------------------------

    #[test]
    fn window_intersects_bbox_filters_off_screen_top_levels() {
        let mut b = make_test_backend();
        // Top-level placed off the default test layout (0,0,800,600).
        let xid = b.next_host_xid;
        b.next_host_xid += 1;
        b.windows
            .insert(xid, make_test_window(2000, 100, 100, 100, true));
        b.top_level_order.push(xid);

        // Whole virtual screen including (0..1024, 0..768) — does not reach x=2000.
        assert!(!b.window_intersects(
            xid,
            super::Rect {
                x: 0,
                y: 0,
                w: 1024,
                h: 768,
            }
        ));
        // Output positioned at virtual-screen x=1900 with width 1024 — overlaps window.
        assert!(b.window_intersects(
            xid,
            super::Rect {
                x: 1900,
                y: 0,
                w: 1024,
                h: 768,
            }
        ));
        // Top-left corner of virtual screen — far away from the window.
        assert!(!b.window_intersects(
            xid,
            super::Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            }
        ));
    }

    // `paint_output_offsets_window_into_scanout` removed in 4.1.5:
    // the legacy pixman scanout path it tested no longer exists.
    // Layout-offset behaviour is now exercised by `compositor.rs`'s
    // composite-scene assembly under the lavapipe smoke harness.

    // RENDER trapezoid tests removed in 4.1.5 — they exercised
    // `pixman_composite_trapezoids` directly. The Vk path is
    // `try_vk_render_trapezoids` (CPU-rasterised mask + Vk composite),
    // exercised by rendercheck under the lavapipe smoke harness.

    #[test]
    fn add_glyphs_stores_pixel_data_correctly() {
        // Build a minimal AddGlyphs body_tail for 2 glyphs:
        //   Glyph 0 (id=1): 2×2 A8 bitmap, x=-1, y=-2, x_off=4, y_off=0
        //   Glyph 1 (id=2): 4×1 A8 bitmap, x=0,  y=-1, x_off=5, y_off=0
        //
        // Wire layout:
        //   num_glyphs(4) = 2
        //   ids: [1u32 LE, 2u32 LE]
        //   infos:
        //     glyph0: width=2 height=2 x=-1 y=-2 x_off=4 y_off=0  (12 bytes)
        //     glyph1: width=4 height=1 x=0  y=-1 x_off=5 y_off=0  (12 bytes)
        //   pixel data:
        //     glyph0: 2×2 A8, row-stride=4 (padded): [0x11,0x22, 0,0, 0x33,0x44, 0,0]
        //     glyph1: 4×1 A8, row-stride=4 (no pad):  [0x55,0x66,0x77,0x88]

        let mut body = Vec::new();
        body.extend_from_slice(&2u32.to_le_bytes()); // num_glyphs
        body.extend_from_slice(&1u32.to_le_bytes()); // id[0]
        body.extend_from_slice(&2u32.to_le_bytes()); // id[1]
        // glyph0 info
        body.extend_from_slice(&2u16.to_le_bytes()); // width
        body.extend_from_slice(&2u16.to_le_bytes()); // height
        body.extend_from_slice(&(-1i16).to_le_bytes()); // x
        body.extend_from_slice(&(-2i16).to_le_bytes()); // y
        body.extend_from_slice(&4i16.to_le_bytes()); // x_off
        body.extend_from_slice(&0i16.to_le_bytes()); // y_off
        // glyph1 info
        body.extend_from_slice(&4u16.to_le_bytes()); // width
        body.extend_from_slice(&1u16.to_le_bytes()); // height
        body.extend_from_slice(&0i16.to_le_bytes()); // x
        body.extend_from_slice(&(-1i16).to_le_bytes()); // y
        body.extend_from_slice(&5i16.to_le_bytes()); // x_off
        body.extend_from_slice(&0i16.to_le_bytes()); // y_off
        // glyph0 pixels: 2×2, padded row stride 4
        body.extend_from_slice(&[0x11, 0x22, 0x00, 0x00]); // row 0
        body.extend_from_slice(&[0x33, 0x44, 0x00, 0x00]); // row 1
        // glyph1 pixels: 4×1, padded row stride 4
        body.extend_from_slice(&[0x55, 0x66, 0x77, 0x88]);

        let mut gs = super::GlyphSetState {
            format: super::GlyphSetFormat::A8,
            glyphs: std::collections::HashMap::new(),
        };
        super::parse_add_glyphs(&mut gs, &body);

        let g0 = gs.glyphs.get(&1).expect("glyph id=1 missing");
        assert_eq!(g0.width, 2);
        assert_eq!(g0.height, 2);
        assert_eq!(g0.x, -1);
        assert_eq!(g0.y, -2);
        assert_eq!(g0.x_off, 4);
        assert_eq!(g0.pixels, vec![0x11, 0x22, 0x33, 0x44]); // densely packed

        let g1 = gs.glyphs.get(&2).expect("glyph id=2 missing");
        assert_eq!(g1.width, 4);
        assert_eq!(g1.pixels, vec![0x55, 0x66, 0x77, 0x88]);
    }

    // composite_glyphs_* tests removed in 4.1.5 — pixman path gone.
    // The Vk path is exercised by rendercheck under the lavapipe smoke
    // harness, which has Vulkan available.

    #[test]
    fn parse_xlfd_extracts_family_style_pixelsize() {
        // XLFD weight strings are lowercase; we pass them straight to fontconfig
        // which matches case-insensitively.
        let (fam, style, px) = super::FontLoader::parse_xlfd(
            "-adobe-helvetica-bold-i-normal--12-120-75-75-p-67-iso8859-1",
        );
        assert_eq!(fam.as_deref(), Some("helvetica"));
        assert_eq!(style.as_deref(), Some("bold Italic"));
        assert_eq!(px, Some(12));
    }

    #[test]
    fn parse_xlfd_treats_wildcards_as_unspecified() {
        // Wildcards in family/weight/slant ⇒ None; pixelsize "*" ⇒ no size.
        let (fam, style, px) = super::FontLoader::parse_xlfd("-*-*-*-*-*-*-*-*-*-*-*-*-*-*");
        assert!(fam.is_none());
        assert!(style.is_none());
        assert!(px.is_none());
    }

    #[test]
    fn parse_xlfd_roman_slant_no_italic() {
        // Slant "r" (roman) shouldn't pull in "Italic"; weight "medium" carries through.
        let (_, style, _) = super::FontLoader::parse_xlfd(
            "-adobe-courier-medium-r-normal--14-140-75-75-m-90-iso8859-1",
        );
        assert_eq!(style.as_deref(), Some("medium"));
    }

    #[test]
    fn open_font_accepts_x11_alias_via_fontconfig() {
        // "fixed" is a classic X11 alias. fontconfig knows it, or falls back
        // to monospace — either way we must get a usable face.
        let loader = super::FontLoader::new().expect("fontconfig+freetype init");
        let (_face, metrics, _cache) = loader.open_font("fixed").expect("resolve fixed");
        assert!(metrics.font_ascent + metrics.font_descent > 0);
    }

    #[test]
    fn xlfd_pattern_matches_charset_filter() {
        // libXt asks per-charset patterns when assembling a fontset.
        // A name with the wrong charset must not match.
        let pat = "-*-*-*-R-*-*-*-120-*-*-*-*-iso8859-1";
        assert!(super::xlfd_pattern_matches(
            pat,
            "-adobe-helvetica-medium-r-normal--12-120-75-75-p-67-iso8859-1",
        ));
        assert!(!super::xlfd_pattern_matches(
            pat,
            "-misc-fixed-medium-r-normal--13-120-75-75-c-70-iso10646-1",
        ));
    }

    #[test]
    fn xlfd_pattern_matches_is_case_insensitive() {
        // xclock sends `-R-` (uppercase) for roman slant; our names use `-r-`.
        assert!(super::xlfd_pattern_matches(
            "-*-HELVETICA-*-R-*-*-*-120-*-*-*-*-ISO8859-1",
            "-adobe-helvetica-medium-r-normal--12-120-75-75-p-67-iso8859-1",
        ));
    }

    #[test]
    fn xlfd_pattern_matches_question_mark_is_single_char() {
        assert!(super::xlfd_pattern_matches("a?c", "abc"));
        assert!(!super::xlfd_pattern_matches("a?c", "abbc"));
        assert!(!super::xlfd_pattern_matches("a?c", "ac"));
    }

    #[test]
    fn xlfd_pattern_matches_star_spans_dashes() {
        // '*' is shell-style glob, not an XLFD-field anchor.
        assert!(super::xlfd_pattern_matches(
            "-*-iso8859-1",
            "-foo-bar-iso8859-1"
        ));
    }

    #[test]
    fn xlfd_weight_buckets() {
        // Spot-check the FC_WEIGHT_* → XLFD weight bucket boundaries.
        assert_eq!(super::xlfd_weight(0), "thin");
        assert_eq!(super::xlfd_weight(50), "light");
        assert_eq!(super::xlfd_weight(80), "book"); // FC_WEIGHT_REGULAR
        assert_eq!(super::xlfd_weight(100), "medium"); // FC_WEIGHT_MEDIUM
        assert_eq!(super::xlfd_weight(180), "demibold"); // FC_WEIGHT_DEMIBOLD
        assert_eq!(super::xlfd_weight(200), "bold"); // FC_WEIGHT_BOLD
        assert_eq!(super::xlfd_weight(210), "black"); // FC_WEIGHT_BLACK
    }

    #[test]
    fn xlfd_slant_and_spacing_codes() {
        assert_eq!(super::xlfd_slant(0), "r");
        assert_eq!(super::xlfd_slant(100), "i");
        assert_eq!(super::xlfd_slant(110), "o");
        assert_eq!(super::xlfd_spacing(0), "p");
        assert_eq!(super::xlfd_spacing(100), "m");
        assert_eq!(super::xlfd_spacing(110), "c");
    }

    #[test]
    fn sanitize_xlfd_field_replaces_dashes_and_lowercases() {
        // Dashes inside an XLFD field would corrupt field separation.
        assert_eq!(super::sanitize_xlfd_field("DejaVu Sans"), "dejavu sans");
        assert_eq!(
            super::sanitize_xlfd_field("Liberation-Mono"),
            "liberation mono"
        );
    }

    #[test]
    fn font_catalog_includes_iso8859_1_and_iso10646_1() {
        // build_font_catalog enumerates real fontconfig faces and emits
        // XLFDs for both Latin-1 and Unicode charsets; libXt's fontset
        // assembly needs both.
        let fc = fontconfig::Fontconfig::new().expect("fontconfig init");
        let catalog = super::build_font_catalog(&fc);
        assert!(
            catalog.iter().any(|x| x.ends_with("-iso8859-1")),
            "catalog has no iso8859-1 entries"
        );
        assert!(
            catalog.iter().any(|x| x.ends_with("-iso10646-1")),
            "catalog has no iso10646-1 entries"
        );
        // Aliases the loader handles directly.
        assert!(catalog.iter().any(|x| x == "fixed"));
    }

    #[test]
    fn dri3_open_errs_when_render_node_unavailable() {
        // make_test_backend uses for_tests() drm device + render_node_fd: None,
        // so dri3_open must Err out (the SCM_RIGHTS dispatch path then maps
        // it to BadAlloc).
        use yserver_core::backend::Backend as _;
        let mut backend = make_test_backend();
        let res = backend.dri3_open(0x1234);
        assert!(res.is_err(), "expected Err when render_node_fd is None");
    }

    // ---------------------------------------------------------------------------
    // Per-output dirty generations: composite_and_flip skips outputs whose
    // last_presented_gen == dirty_gen, and clears nothing globally. The
    // previous global dirty bool collapsed "anything anywhere changed"
    // with "this output needs a composite this tick"; per-output state
    // separates them so a skipped output catches up cleanly.
    // ---------------------------------------------------------------------------

    #[test]
    fn fresh_backend_has_every_output_dirty() {
        let backend = make_test_backend();
        for layout in &backend.outputs {
            assert!(
                layout.damage.needs_composite(),
                "fresh output must paint on first frame"
            );
        }
    }

    #[test]
    fn composite_and_flip_does_not_set_flip_pending_on_no_vk_path() {
        // The for_tests backend has no VK context, so
        // try_vulkan_composite_flip returns None and record_submit
        // is never called. This test guards against the regression
        // where record_submit is accidentally called outside the
        // successful-submit branch.
        let mut backend = make_test_backend();
        backend.composite_and_flip().unwrap();
        for layout in &backend.outputs {
            assert!(
                !layout.damage.flip_pending(),
                "no-VK path must not set flip_pending"
            );
        }
    }

    #[test]
    fn mark_dirty_bumps_every_output() {
        use yserver_core::backend::Backend as _;
        let mut backend = make_test_backend();
        let before: Vec<u64> = backend
            .outputs
            .iter()
            .map(|l| l.damage.dirty_gen())
            .collect();
        backend.mark_dirty();
        for (i, layout) in backend.outputs.iter().enumerate() {
            assert!(
                layout.damage.dirty_gen() > before[i],
                "mark_dirty (no-arg) bumps every output"
            );
        }
    }

    // ---------------------------------------------------------------------------
    // PolyText{8,16} wire-format parsing — TEXTITEM is `len(u8) delta(i8)
    // chars(len*itemSize)` for len in 0..=254, or `255 font_id(u32 BE)` for a
    // font change. Pre-fix code read the first byte as `delta`, dropped the
    // delta byte, included it as a leading char, and read the font id at the
    // wrong offset / endianness — visible as bold/colored xterm text doubling
    // ("create" → "ccrate" with each glyph offset by ~6 px from the SOH char
    // shifting the cursor on the bold-overdraw pass).
    // ---------------------------------------------------------------------------
    fn poly_text_header(x: i16, y: i16) -> Vec<u8> {
        let mut h = Vec::with_capacity(12);
        h.extend_from_slice(&0u32.to_le_bytes()); // drawable
        h.extend_from_slice(&0u32.to_le_bytes()); // gc
        h.extend_from_slice(&x.to_le_bytes());
        h.extend_from_slice(&y.to_le_bytes());
        h
    }

    #[test]
    fn poly_text8_string_then_font_change_consumes_correct_byte_count() {
        // Pre-fix this fails because the buggy parser would read the leading
        // `len=6` byte as `delta`, then misread the trailing 'e' as a new
        // item, never reaching the font-change marker.
        let mut b = make_test_backend();
        let mut body = poly_text_header(0, 0);
        // String item: len=6, delta=+1 (xterm bold-overdraw pattern), "create"
        body.push(6);
        body.push(1);
        body.extend_from_slice(b"create");
        // Font change: 0xff, font id 0x12345678 big-endian
        body.push(0xff);
        body.extend_from_slice(&[0x12, 0x34, 0x56, 0x78]);

        b.poly_text8(None, 0, 0, &body).unwrap();
        assert_eq!(b.current_font, Some(0x1234_5678));
    }

    #[test]
    fn poly_text8_font_change_uses_big_endian_font_id() {
        let mut b = make_test_backend();
        let mut body = poly_text_header(0, 0);
        body.push(0xff);
        body.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

        b.poly_text8(None, 0, 0, &body).unwrap();
        assert_eq!(b.current_font, Some(0xAABB_CCDD));
    }

    #[test]
    fn poly_text8_zero_length_item_consumes_two_bytes_and_continues() {
        // X11: `len=0` is a delta-only adjustment (still 2 bytes: len + delta).
        // Pre-fix it terminated the loop, so any subsequent items were dropped.
        let mut b = make_test_backend();
        let mut body = poly_text_header(0, 0);
        body.push(0); // len=0 (delta-only)
        body.push(7); // delta=+7
        body.push(0xff);
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        b.poly_text8(None, 0, 0, &body).unwrap();
        assert_eq!(b.current_font, Some(0xDEAD_BEEF));
    }

    #[test]
    fn poly_text16_string_then_font_change_consumes_correct_byte_count() {
        // PolyText16 chars are 2 bytes each, big-endian (CHAR2B).
        let mut b = make_test_backend();
        let mut body = poly_text_header(0, 0);
        body.push(3); // len=3 (chars)
        body.push(0); // delta=0
        body.extend_from_slice(&[0x00, b'A', 0x00, b'B', 0x00, b'C']);
        body.push(0xff);
        body.extend_from_slice(&[0xCA, 0xFE, 0xF0, 0x0D]);

        b.poly_text16(None, 0, 0, &body).unwrap();
        assert_eq!(b.current_font, Some(0xCAFE_F00D));
    }

    // ---------------------------------------------------------------------------
    // mark_window_dirty_with_old_rect — helper and multi-output fixture
    // ---------------------------------------------------------------------------

    /// Push a synthetic second (or third) output into `backend.outputs`.
    /// Constructs a new `OutputLayout` using the same fake DRM handles as
    /// `for_tests()`; tests must never pass these through the DRM ioctl path.
    /// `x` is the left edge in virtual-screen coords; `width` is in pixels.
    fn push_extra_output(backend: &mut KmsBackend, x: i32, width: u16) {
        backend.outputs.push(OutputLayout {
            output: crate::drm::modeset::Output {
                connector: ::drm::control::from_u32(2).unwrap(),
                connector_name: format!("test-extra-{x}"),
                crtc: ::drm::control::from_u32(2).unwrap(),
                plane: ::drm::control::from_u32(2).unwrap(),
                // SAFETY: never passed to DRM in tests.
                mode: unsafe { std::mem::zeroed() },
                picked: crate::drm::modeset::Mode {
                    name: format!("{width}x600"),
                    width,
                    height: 600,
                    vrefresh: 60,
                    preferred: false,
                },
                plane_fb_id_prop: ::drm::control::from_u32(2).unwrap(),
                plane_crtc_id_prop: ::drm::control::from_u32(2).unwrap(),
            },
            swapchain: crate::drm::Swapchain::empty_for_tests(),
            x,
            y: 0,
            width,
            height: 600,
            damage: crate::kms::scheduler::damage::OutputDamageState::new(),
            composite_pools: None,
        });
        // Keep first_pageflip_logged in sync with the outputs count.
        backend.first_pageflip_logged.push(false);
    }

    #[test]
    fn mark_window_dirty_with_old_rect_single_output_sanity() {
        // Single output: any non-empty rect touching it bumps the gen.
        let mut backend = make_test_backend();
        let gen_before = backend.outputs[0].damage.dirty_gen();
        let rect = super::Rect {
            x: 10,
            y: 10,
            w: 50,
            h: 50,
        };
        backend.mark_window_dirty_with_old_rect(rect, rect);
        assert!(
            backend.outputs[0].damage.dirty_gen() > gen_before,
            "single output must be bumped when rect is on it"
        );
    }

    #[test]
    fn mark_window_dirty_with_old_rect_bumps_old_and_new_outputs() {
        let mut backend = make_test_backend();
        // Output A: x=0..800 (from for_tests). Output B: x=1920..3840.
        push_extra_output(&mut backend, 1920, 1920);
        // Extend the output B width to cover x=1920..3840; the base output
        // covers 0..800. Use a new_rect on B.
        let gen_a_before = backend.outputs[0].damage.dirty_gen();
        let gen_b_before = backend.outputs[1].damage.dirty_gen();
        let old_rect = super::Rect {
            x: 50,
            y: 50,
            w: 100,
            h: 100,
        }; // on A (0..800)
        let new_rect = super::Rect {
            x: 2000,
            y: 50,
            w: 100,
            h: 100,
        }; // on B (1920..3840)
        backend.mark_window_dirty_with_old_rect(old_rect, new_rect);
        assert!(
            backend.outputs[0].damage.dirty_gen() > gen_a_before,
            "output A must be bumped (old_rect is on A)"
        );
        assert!(
            backend.outputs[1].damage.dirty_gen() > gen_b_before,
            "output B must be bumped (new_rect is on B)"
        );
    }

    #[test]
    fn mark_window_dirty_with_old_rect_does_not_bump_uninvolved_outputs() {
        let mut backend = make_test_backend();
        // Output A: 0..800. Output B: 1920..3840. Output C: 3840..5760.
        push_extra_output(&mut backend, 1920, 1920);
        push_extra_output(&mut backend, 3840, 1920);
        let gens_before: Vec<u64> = backend
            .outputs
            .iter()
            .map(|l| l.damage.dirty_gen())
            .collect();
        // Move within A — only A bumps.
        let old_rect = super::Rect {
            x: 50,
            y: 50,
            w: 100,
            h: 100,
        };
        let new_rect = super::Rect {
            x: 200,
            y: 50,
            w: 100,
            h: 100,
        };
        backend.mark_window_dirty_with_old_rect(old_rect, new_rect);
        assert!(
            backend.outputs[0].damage.dirty_gen() > gens_before[0],
            "output A must be bumped (both rects on A)"
        );
        assert_eq!(
            backend.outputs[1].damage.dirty_gen(),
            gens_before[1],
            "output B must NOT be bumped"
        );
        assert_eq!(
            backend.outputs[2].damage.dirty_gen(),
            gens_before[2],
            "output C must NOT be bumped"
        );
    }

    #[test]
    fn mark_window_dirty_with_old_rect_handles_empty_rect_as_no_bump() {
        // Empty rect (e.g. map: old = empty, new = current) overlaps nothing.
        let mut backend = make_test_backend();
        let gen_before = backend.outputs[0].damage.dirty_gen();
        let empty = super::Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        let new_rect = super::Rect {
            x: 50,
            y: 50,
            w: 100,
            h: 100,
        };
        // map case: old=empty, new=current — only new bumps output A.
        backend.mark_window_dirty_with_old_rect(empty, new_rect);
        assert!(
            backend.outputs[0].damage.dirty_gen() > gen_before,
            "output must be bumped by new_rect even when old is empty"
        );
        // unmap case: old=current, new=empty — only old bumps output A.
        let gen_mid = backend.outputs[0].damage.dirty_gen();
        backend.mark_window_dirty_with_old_rect(new_rect, empty);
        assert!(
            backend.outputs[0].damage.dirty_gen() > gen_mid,
            "output must be bumped by old_rect even when new is empty"
        );
    }

    // ---------------------------------------------------------------------------
    // renderer_failed gate tests (Task 1, step 5)
    // ---------------------------------------------------------------------------

    #[test]
    fn renderer_failed_makes_record_paint_op_return_device_lost() {
        let mut backend = make_test_backend();
        backend.renderer_failed = true;
        let result = backend.record_paint_op(|_, _| Ok(()));
        assert_eq!(result, Err(ash::vk::Result::ERROR_DEVICE_LOST));
    }

    #[test]
    fn renderer_failed_makes_visible_composite_flush_a_noop() {
        use crate::kms::scheduler::paint_batch::BatchFlushReason;
        let mut backend = make_test_backend();
        backend.renderer_failed = true;
        // VisibleComposite is best-effort; gate returns Ok.
        assert!(
            backend
                .flush_if_needed(BatchFlushReason::VisibleComposite)
                .is_ok()
        );
    }

    #[test]
    fn renderer_failed_makes_readback_flush_surface_device_lost() {
        use crate::kms::scheduler::paint_batch::BatchFlushReason;
        let mut backend = make_test_backend();
        backend.renderer_failed = true;
        // Readback is strict; gate returns Err.
        assert_eq!(
            backend.flush_if_needed(BatchFlushReason::Readback),
            Err(ash::vk::Result::ERROR_DEVICE_LOST)
        );
    }

    #[test]
    fn renderer_failed_makes_composite_and_flip_a_noop() {
        let mut backend = make_test_backend();
        backend.renderer_failed = true;
        // Even with dirty outputs, composite returns Ok early.
        assert!(backend.composite_and_flip().is_ok());
    }
}
