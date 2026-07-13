//! Glyph pixel sources for the `CompositeGlyphs` path and their
//! conversion to the dense A8 bitmap the atlas uploader consumes.
//!
//! The backend's wire parse forwards a glyph's pixels *as stored in
//! the glyphset* — dense A8 for `PICT_a8` / ARGB32-preconverted
//! glyphs, or the raw A1 wire bitmap for `PICT_a1`. Expansion of A1
//! to A8 is **deferred to the engine's atlas-miss branch** via
//! [`GlyphPixels::to_a8`]: a glyph already resident in the GPU atlas
//! is never expanded again, mirroring Xorg/EXA's convert-on-upload
//! (`exa/exa_glyphs.c`). See
//! `docs/superpowers/findings/2026-07-08-xorg-render-optimization-gaps.md` #2.

use std::borrow::Cow;

/// Pixel source for one glyph, as stored in its glyphset. Cheap to
/// carry (two `&[u8]` variants); the actual A1→A8 expansion only
/// happens on an atlas miss.
#[derive(Copy, Clone, Debug)]
pub(crate) enum GlyphPixels<'a> {
    /// Dense A8 coverage bitmap, row-major `w × h`. Covers native
    /// `PICT_a8` glyphs and ARGB32 glyphs already alpha-extracted to
    /// A8 at ingest (`parse_add_glyphs`).
    A8(&'a [u8]),
    /// Raw A1 wire bitmap: rows padded to a 32-bit scanline unit,
    /// bit order = advertised `bitmap-bit-order` (LSBFirst on the
    /// common little-endian client). Expanded lazily by [`Self::to_a8`].
    A1Wire(&'a [u8]),
}

impl<'a> GlyphPixels<'a> {
    /// Produce the dense `w × h` A8 bitmap the atlas uploader copies
    /// into its staging buffer. `A8` borrows in place (truncated to
    /// `w × h`); `A1Wire` expands into an owned buffer. Returns
    /// `None` if the source is too short for the declared dimensions
    /// (caller drops the glyph), so a malformed request can never
    /// index out of bounds during upload.
    pub(crate) fn to_a8(self, w: u32, h: u32) -> Option<Cow<'a, [u8]>> {
        let cells = (w as usize).checked_mul(h as usize)?;
        match self {
            GlyphPixels::A8(src) => {
                if src.len() < cells {
                    return None;
                }
                Some(Cow::Borrowed(&src[..cells]))
            }
            GlyphPixels::A1Wire(src) => {
                let wire_stride = (w as usize).div_ceil(32) * 4;
                let need = wire_stride.checked_mul(h as usize)?;
                if src.len() < need {
                    return None;
                }
                Some(Cow::Owned(expand_a1_glyph_to_a8(src, w, h)))
            }
        }
    }
}

/// Expand a wire A1 glyph bitmap to dense A8 (`0x00` / `0xFF` per
/// pixel), row-major `w × h`.
///
/// Wire rows are padded to a 32-bit scanline unit; bit order is
/// LSBFirst (bit 0 of a byte is the leftmost of its 8-pixel group),
/// matching yserver's advertised `bitmap-bit-order` on x86. Reading
/// MSBFirst would mirror every 8-pixel run — the issue-#77 bitmap-font
/// regression the tests below guard.
pub(crate) fn expand_a1_glyph_to_a8(pixels: &[u8], gw: u32, gh: u32) -> Vec<u8> {
    let wire_stride = (gw as usize).div_ceil(32) * 4;
    let mut a8 = vec![0u8; (gw as usize) * (gh as usize)];
    for row in 0..(gh as usize) {
        let src_off = row * wire_stride;
        if src_off + wire_stride > pixels.len() {
            break;
        }
        for col in 0..(gw as usize) {
            let byte = pixels[src_off + col / 8];
            let bit = (byte >> (col & 7)) & 1;
            a8[row * (gw as usize) + col] = if bit != 0 { 0xFF } else { 0 };
        }
    }
    a8
}

#[cfg(test)]
mod tests {
    use super::{GlyphPixels, expand_a1_glyph_to_a8};
    use std::borrow::Cow;

    #[test]
    fn to_a8_borrows_dense_a8_truncated_to_cells() {
        // A8 source longer than w*h (trailing padding bytes) borrows
        // in place, truncated to exactly the glyph cells — no copy.
        let src = [10u8, 20, 30, 40, 0xEE, 0xEE];
        let out = GlyphPixels::A8(&src).to_a8(2, 2).expect("fits");
        assert!(matches!(out, Cow::Borrowed(_)), "A8 must not copy");
        assert_eq!(&*out, &[10, 20, 30, 40]);
    }

    #[test]
    fn to_a8_expands_a1_wire_lsb_first() {
        // 8x1 wire byte 0b0000_0001 → col 0 set (LSBFirst). Owned copy.
        let wire = [0b0000_0001u8, 0, 0, 0];
        let out = GlyphPixels::A1Wire(&wire).to_a8(8, 1).expect("fits");
        assert!(matches!(out, Cow::Owned(_)), "A1 must expand into owned");
        assert_eq!(&*out, &[0xFF, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn to_a8_none_when_a8_too_short() {
        let src = [1u8, 2, 3];
        assert!(GlyphPixels::A8(&src).to_a8(2, 2).is_none());
    }

    #[test]
    fn to_a8_none_when_a1_wire_too_short() {
        // 8x2 needs two 4-byte scanline units; give only one.
        let wire = [0xFFu8, 0, 0, 0];
        assert!(GlyphPixels::A1Wire(&wire).to_a8(8, 2).is_none());
    }

    // ── issue #77: bitmap fonts (terminus) rendered backwards ──
    // Guards the LSBFirst expansion order at its new home.

    #[test]
    fn a1_glyph_expands_lsb_first_not_mirrored() {
        // 8x1 glyph, one wire byte (padded to a 32-bit unit). Byte
        // 0b0000_0001 has only bit 0 set → LSBFirst means the LEFTMOST
        // pixel (col 0) is on and the rest are off.
        let wire = [0b0000_0001u8, 0, 0, 0];
        let a8 = expand_a1_glyph_to_a8(&wire, 8, 1);
        assert_eq!(
            a8,
            vec![0xFF, 0, 0, 0, 0, 0, 0, 0],
            "col 0 must be the set pixel (LSBFirst); an MSBFirst read \
             would light col 7 instead, mirroring the glyph"
        );

        // A left-to-right ramp: bits 0,1,2 set (cols 0,1,2), 0b0000_0111.
        let wire = [0b0000_0111u8, 0, 0, 0];
        let a8 = expand_a1_glyph_to_a8(&wire, 8, 1);
        assert_eq!(a8, vec![0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn a1_glyph_expands_wide_glyph_across_scanline_units() {
        // 12px-wide glyph: still one 32-bit scanline unit (4 bytes) per
        // row, spanning two bytes of pixels. Verify column indexing is
        // continuous and LSBFirst across the byte boundary.
        // Byte 0 = 0b1000_0001 → cols 0 and 7 set.
        // Byte 1 = 0b0000_1000 → col 8+3 = 11 set.
        let wire = [0b1000_0001u8, 0b0000_1000u8, 0, 0];
        let a8 = expand_a1_glyph_to_a8(&wire, 12, 1);
        let mut expect = vec![0u8; 12];
        expect[0] = 0xFF;
        expect[7] = 0xFF;
        expect[11] = 0xFF;
        assert_eq!(a8, expect);
    }
}
