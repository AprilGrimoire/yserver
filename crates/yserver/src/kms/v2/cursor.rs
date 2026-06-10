// Cursor rasterisation is full of intentional i32 → u16/u32 saturating
// casts matched per the codebase's per-call discipline in
// `kms/v2/backend.rs`. Hoisted to module scope here to avoid clutter
// inside the algorithm body.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

//! Stage 5 Phase A — cursor records + sprite rasterisation.
//!
//! Each X11 cursor (created via `CreateCursor`, `CreateGlyphCursor`,
//! `RenderCreateCursor`) lowers to an immutable [`CursorRecord`]:
//! a versioned, refcounted snapshot of the cursor sprite.
//!
//! Two pixel storages per record:
//! - `bgra_bytes`: tightly-packed little-endian BGRA8 matching DRM
//!   `ARGB8888`. Used by the HW cursor path to memcpy into the
//!   dumb-buffer (no GPU readback). Premultiplied-alpha convention
//!   is `straight` here — fully-visible pixels have α=0xFF, fully
//!   transparent pixels have α=0x00. Phase B's `cursor_plane_upload_image`
//!   blits these straight bytes.
//! - The matching v2 [`DrawableStore`] Pixmap (held under
//!   `cursor_pixmaps[xid]` on the backend) which the SW scene
//!   path samples through. Uploaded once via `engine.put_image`.
//!
//! Records are wrapped in `Arc` so anything that captured a
//! reference (a pending cursor swap mid-frame, a Phase D deferred
//! upload) observes the bytes it captured even after a later
//! `DefineCursor` allocates a fresh record with a fresh version.
//! Versions are monotonically increasing server-wide; comparison is
//! by value, never by `Arc` pointer identity.

use std::sync::Arc;

/// Per-cursor versioned snapshot.
///
/// Immutable after construction — theme reload / `XFixes` replacement
/// / `RenderCreateCursor` of a new image allocates a *fresh* record
/// with a fresh version, never mutates an existing one. This is what
/// lets pointer-grab paths capture an "effective cursor" reference
/// (`Arc<CursorRecord>`) and observe stable bytes even if a newer
/// record has superseded the canonical xid mapping.
#[derive(Debug)]
pub(crate) struct CursorRecord {
    /// Sprite width in pixels. Clamped to ≤ `HW_CURSOR_W` (64) at
    /// rasterisation time; cursors larger than that take the SW
    /// fallback in Phase C's `CursorAssignment` decision.
    pub(crate) width: u16,
    /// Sprite height in pixels.
    pub(crate) height: u16,
    /// Hotspot X (X11 cursor-origin coords; the click point).
    pub(crate) hot_x: u16,
    /// Hotspot Y.
    pub(crate) hot_y: u16,
    /// Tightly-packed `width × height × 4` BGRA8. Little-endian
    /// byte order matching DRM `ARGB8888`. Straight alpha (NOT
    /// premultiplied) so the HW dumb-buffer and the SW pixmap
    /// agree on sample values byte-for-byte.
    pub(crate) bgra_bytes: Vec<u8>,
    /// Monotonically-increasing version (compared by value, never
    /// by Arc identity). Consumed by Phase B/C's upload-dedup path.
    pub(crate) version: u64,
}

impl CursorRecord {
    pub(crate) fn new(
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
        bgra_bytes: Vec<u8>,
        version: u64,
    ) -> Arc<Self> {
        debug_assert_eq!(
            bgra_bytes.len(),
            usize::from(width) * usize::from(height) * 4,
            "CursorRecord bytes must be width*height*4",
        );
        Arc::new(Self {
            width,
            height,
            hot_x,
            hot_y,
            bgra_bytes,
            version,
        })
    }
}

/// One frame of a RENDER animated cursor (spec
/// `2026-06-10-animated-cursors-design.md`). Snapshotted at
/// `create_anim_cursor` time so constituent-cursor lifetime is a
/// non-issue (Xorg refcounts; we snapshot).
pub(crate) struct AnimFrame {
    pub(crate) record: Arc<CursorRecord>,
    /// Sprite pixmap the SW scene path samples. `None` when the
    /// sub-cursor's sprite alloc was skipped (Vk-less test
    /// fixtures) — mirrors `insert_cursor_record`'s best-effort
    /// `cursor_pixmaps` insert.
    pub(crate) pixmap: Option<crate::kms::v2::store::DrawableId>,
    pub(crate) delay: std::time::Duration,
}

/// Frame list for one animated cursor, keyed by the anim cursor's
/// host handle in `KmsBackendV2::anim_cursor_records`.
pub(crate) struct AnimCursorRecord {
    pub(crate) frames: Vec<AnimFrame>,
}

/// Live animation state — at most one, mirroring the single
/// effective cursor. Armed/cleared by `sync_cursor_animation`,
/// advanced by `tick_cursor_animation`.
pub(crate) struct ActiveCursorAnim {
    /// Animated cursor (host handle) whose frames are cycling.
    pub(crate) handle: u32,
    /// Current frame index into `AnimCursorRecord::frames`.
    pub(crate) frame: usize,
    /// Deadline for the next advance. Reported via `next_wakeup()`
    /// while outputs are active.
    pub(crate) next_frame: std::time::Instant,
}

/// 16×16 classic X-shaped default cursor (matches v1's
/// `install_default_cursor` at `kms/backend.rs:2286-2308`). Both
/// diagonals of the 16×16 box drawn in black with a 1-pixel white
/// halo for visibility against dark backgrounds. Hotspot at the
/// centre — the natural choice for a centred X.
///
/// Renamed from `default_arrow` (Stage 3f.8 placeholder) so the
/// boot cursor matches the v1 baseline that long-running test
/// sessions expect.
pub(crate) fn default_arrow_bgra() -> Vec<u8> {
    const W: i32 = 16;
    const H: i32 = 16;
    let mut bytes = vec![0u8; (W * H) as usize * 4];
    let last = W - 1;
    for y in 0..H {
        for x in 0..W {
            // Distance to either diagonal of the 16×16 box.
            let d1 = (x - y).abs();
            let d2 = (x + y - last).abs();
            let dist = d1.min(d2);
            let bgra: [u8; 4] = match dist {
                0 => [0x00, 0x00, 0x00, 0xFF], // black core, opaque
                1 => [0xFF, 0xFF, 0xFF, 0xFF], // white halo, opaque
                _ => [0x00, 0x00, 0x00, 0x00], // transparent
            };
            let off = ((y * W + x) as usize) * 4;
            bytes[off..off + 4].copy_from_slice(&bgra);
        }
    }
    bytes
}

pub(crate) const DEFAULT_ARROW_W: u16 = 16;
pub(crate) const DEFAULT_ARROW_H: u16 = 16;
/// Hotspot for the default X cursor — centred per v1's
/// `install_default_cursor` (`w/2, h/2`).
pub(crate) const DEFAULT_ARROW_HOT_X: u16 = 8;
pub(crate) const DEFAULT_ARROW_HOT_Y: u16 = 8;

/// Rasterise an X11 `CreateCursor` (`source`, `mask`, `fore`, `back`)
/// tuple into BGRA. Both sources are depth-1 R8-mirrored — a non-zero
/// byte means the bit is set. Output uses straight alpha (0xFF for
/// visible, 0x00 for transparent).
///
/// `src_bytes` and `mask_bytes` are arranged row-major at width
/// `src_w` (mask must match dims or be `None`). Pre-sized so we
/// don't have to clip per-pixel; bytes are read directly.
///
/// X11 pixel rule:
///   * mask supplied → pixel visible iff mask bit set; visible pixels
///     carry `fore` if source bit set else `back`.
///   * mask = None   → all pixels visible; same fore/back gating.
pub(crate) fn rasterise_create_cursor(
    src_bytes: &[u8],
    src_w: u16,
    src_h: u16,
    mask_bytes: Option<&[u8]>,
    fore: (u16, u16, u16),
    back: (u16, u16, u16),
) -> Vec<u8> {
    let w = usize::from(src_w);
    let h = usize::from(src_h);
    let pixel_count = w * h;
    let fr = (fore.0 >> 8) as u8;
    let fg = (fore.1 >> 8) as u8;
    let fb = (fore.2 >> 8) as u8;
    let br = (back.0 >> 8) as u8;
    let bg = (back.1 >> 8) as u8;
    let bb = (back.2 >> 8) as u8;
    let mut argb = vec![0u8; pixel_count * 4];
    for i in 0..pixel_count {
        let src_set = src_bytes.get(i).copied().unwrap_or(0) != 0;
        let visible = match mask_bytes {
            Some(mb) => mb.get(i).copied().unwrap_or(0) != 0,
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
    argb
}

/// Glyph cursor rasterisation result. Returned by
/// [`rasterise_glyph_cursor`]; carries the pixmap dimensions, hotspot
/// derived from the source glyph's origin, and the packed BGRA bytes.
pub(crate) struct GlyphCursorImage {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) hot_x: u16,
    pub(crate) hot_y: u16,
    pub(crate) bgra_bytes: Vec<u8>,
}

/// A single FreeType-rendered glyph used as input to glyph-cursor
/// rasterisation. `lsb` / `top` are the `FreeType` `bitmap_left` /
/// `bitmap_top` (signed; can be negative for italic-style glyphs).
pub(crate) struct GlyphBitmap<'a> {
    pub(crate) pixels: &'a [u8],
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) lsb: i32,
    pub(crate) top: i32,
}

/// Rasterise an X11 `CreateGlyphCursor` into BGRA bytes + pixmap dims +
/// hotspot. Ported from v1's body in `kms/backend.rs:9937-10108`.
///
/// X11 pixel rule:
///   * `mask` supplied → visible iff mask bit set; visible pixels
///     carry `fore` if source bit set else `back`.
///   * `mask = None`   → source doubles as mask: visible iff source
///     bit set; visible pixels always carry `fore`.
///
/// Coordinates: the cursor pixmap is the union bbox of source + mask
/// glyphs in their `FreeType` origin frame (positive y up). The hotspot
/// is the source glyph's origin point expressed in pixmap coords
/// (top-left origin, y down).
pub(crate) fn rasterise_glyph_cursor(
    src: &GlyphBitmap<'_>,
    mask: Option<&GlyphBitmap<'_>>,
    fore: (u16, u16, u16),
    back: (u16, u16, u16),
) -> GlyphCursorImage {
    let (left, right, top, bottom) = match mask {
        Some(m) => (
            src.lsb.min(m.lsb),
            (src.lsb + src.width).max(m.lsb + m.width),
            src.top.max(m.top),
            (src.height - src.top).max(m.height - m.top),
        ),
        None => (src.lsb, src.lsb + src.width, src.top, src.height - src.top),
    };
    let pixmap_w = (right - left).max(1) as u32;
    let pixmap_h = (top + bottom).max(1) as u32;
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
        let off = (y * w + x) as usize;
        pixels.get(off).copied().unwrap_or(0) > 0
    };
    let src_off_x = src.lsb - left;
    let src_off_y = top - src.top;
    let mask_off = mask.as_ref().map(|m| (m.lsb - left, top - m.top));

    let pixel_count = (pixmap_w as usize) * (pixmap_h as usize);
    let mut argb = vec![0u8; pixel_count * 4];
    for y in 0..pixmap_h as i32 {
        for x in 0..pixmap_w as i32 {
            let src_set = read_bit(
                src.pixels,
                src.width,
                src.height,
                x - src_off_x,
                y - src_off_y,
            );
            let visible = match (mask, mask_off) {
                (Some(m), Some((mox, moy))) => {
                    read_bit(m.pixels, m.width, m.height, x - mox, y - moy)
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
    GlyphCursorImage {
        width: pixmap_w.min(u32::from(u16::MAX)) as u16,
        height: pixmap_h.min(u32::from(u16::MAX)) as u16,
        hot_x,
        hot_y,
        bgra_bytes: argb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_record_dims_round_trip() {
        let rec = CursorRecord::new(4, 4, 1, 2, vec![0xFFu8; 4 * 4 * 4], 42);
        assert_eq!(rec.width, 4);
        assert_eq!(rec.height, 4);
        assert_eq!(rec.hot_x, 1);
        assert_eq!(rec.hot_y, 2);
        assert_eq!(rec.version, 42);
        assert_eq!(rec.bgra_bytes.len(), 4 * 4 * 4);
    }

    /// Replacing a record never mutates the old Arc's bytes — load-
    /// bearing for any path that captured an `Arc<CursorRecord>`
    /// reference (pointer grab, Phase D deferred upload).
    #[test]
    fn replacement_does_not_mutate_old() {
        let old = CursorRecord::new(
            2,
            2,
            0,
            0,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            1,
        );
        let snapshot: Vec<u8> = old.bgra_bytes.clone();
        // Allocate a "replacement" — a fresh Arc with different bytes.
        // The old Arc is held in `old`; the test simulates the
        // protocol-handler swapping the canonical xid → record map.
        let _new = CursorRecord::new(2, 2, 0, 0, vec![0u8; 2 * 2 * 4], 2);
        assert_eq!(old.bgra_bytes, snapshot, "old bytes mutated under us");
    }

    /// Version comparison is by value, not pointer identity — two
    /// distinct Arc allocations holding the same bytes/version must
    /// compare equal.
    #[test]
    fn version_compared_by_value() {
        let a = CursorRecord::new(1, 1, 0, 0, vec![0, 0, 0, 0xFF], 7);
        let b = CursorRecord::new(1, 1, 0, 0, vec![0, 0, 0, 0xFF], 7);
        assert!(!Arc::ptr_eq(&a, &b), "test invariant: distinct Arcs");
        assert_eq!(a.version, b.version);
    }

    /// Straight-alpha invariant — visible pixels in
    /// `rasterise_create_cursor` carry `α=0xFF`, fully-transparent
    /// pixels carry `α=0x00`. No intermediate values (no premul).
    #[test]
    fn rasterise_create_cursor_uses_straight_alpha() {
        // 2×2 source: pixel 0,2 set; mask: pixel 0,1 set.
        // → pixel 0 visible (mask set) + src set → `fore` opaque
        // → pixel 1 visible (mask set) + src clear → `back` opaque
        // → pixel 2 invisible (mask clear) → α=0
        // → pixel 3 invisible (mask clear) → α=0
        let src = [0xFFu8, 0x00, 0xFFu8, 0x00];
        let mask = [0xFFu8, 0xFFu8, 0x00, 0x00];
        let bgra = rasterise_create_cursor(
            &src,
            2,
            2,
            Some(&mask),
            (0xFFFF, 0, 0), // red fore
            (0, 0xFFFF, 0), // green back
        );
        assert_eq!(bgra.len(), 16);
        // Pixel 0: visible, src set → red, α=FF.
        assert_eq!(&bgra[0..4], &[0x00, 0x00, 0xFF, 0xFF]);
        // Pixel 1: visible, src clear → green, α=FF.
        assert_eq!(&bgra[4..8], &[0x00, 0xFF, 0x00, 0xFF]);
        // Pixel 2,3: invisible → all zero (α=0).
        assert_eq!(&bgra[8..12], &[0, 0, 0, 0]);
        assert_eq!(&bgra[12..16], &[0, 0, 0, 0]);
    }

    /// Default X cursor rasterisation: 16×16 with both diagonals
    /// in opaque black, 1-pixel white halo, fully-transparent
    /// elsewhere. Matches v1's `install_default_cursor` at
    /// `kms/backend.rs:2286-2308`.
    #[test]
    fn default_arrow_is_x_with_halo() {
        let bytes = default_arrow_bgra();
        assert_eq!(bytes.len(), 16 * 16 * 4);
        let px = |x: usize, y: usize| {
            let off = (y * 16 + x) * 4;
            [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]
        };
        // Diagonal 1 (top-left → bottom-right): every (k, k) is
        // opaque black.
        for k in 0..16 {
            assert_eq!(px(k, k), [0x00, 0x00, 0x00, 0xFF], "diag1 (k,k)={k}");
        }
        // Diagonal 2 (top-right → bottom-left): every (k, 15-k).
        for k in 0..16 {
            assert_eq!(
                px(k, 15 - k),
                [0x00, 0x00, 0x00, 0xFF],
                "diag2 (k,15-k)={k}"
            );
        }
        // 1-pixel white halo: e.g. (1, 0) is `dist=1` to diag1,
        // visible-white.
        assert_eq!(px(1, 0), [0xFF, 0xFF, 0xFF, 0xFF]);
        // Far from both diagonals: (7, 0) sits at d1=7, d2=8 →
        // min=7 → transparent.
        assert_eq!(px(7, 0), [0, 0, 0, 0]);
        // Centre (8, 8) is on diag1 → opaque black (hotspot).
        assert_eq!(px(8, 8), [0x00, 0x00, 0x00, 0xFF]);
    }

    /// Glyph cursor with `mask = None` collapses to "source bit also
    /// acts as visibility" — every visible pixel carries `fore`,
    /// invisible pixels are α=0.
    #[test]
    fn glyph_cursor_no_mask_uses_source_as_mask() {
        // 2×2 glyph: top-left and bottom-right set.
        let pixels = [0xFFu8, 0x00, 0x00, 0xFFu8];
        let src = GlyphBitmap {
            pixels: &pixels,
            width: 2,
            height: 2,
            lsb: 0,
            top: 2, // glyph origin at (0, 2) in FreeType frame
        };
        let img = rasterise_glyph_cursor(
            &src,
            None,
            (0xFFFF, 0, 0), // red
            (0, 0xFFFF, 0), // green (unused because mask is None)
        );
        // Pixmap dims = src dims (no mask), hotspot = (0, 2).
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.hot_x, 0);
        assert_eq!(img.hot_y, 2);
        // Pixel (0,0) — src set → red, opaque.
        assert_eq!(&img.bgra_bytes[0..4], &[0x00, 0x00, 0xFF, 0xFF]);
        // Pixel (1,0) — src clear, no mask → invisible.
        assert_eq!(&img.bgra_bytes[4..8], &[0, 0, 0, 0]);
        // Pixel (1,1) — src set → red.
        assert_eq!(&img.bgra_bytes[12..16], &[0x00, 0x00, 0xFF, 0xFF]);
    }
}
