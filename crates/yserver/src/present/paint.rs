//! Throwaway painter — fills a buffer with a dark-grey background, a
//! 60×60 magenta rectangle at `state.rect_*`, and a 4×4 white cursor at
//! `state.cursor_*`. Naive bounds-checked write loops; not optimised
//! because it's about to be deleted in C.

use crate::{
    drm::DumbBuffer,
    present::state::{CURSOR_SIZE, RECT_SIZE, State},
};

const BACKGROUND: u32 = 0x0020_2020;
const MAGENTA: u32 = 0x00FF_0080;
const WHITE: u32 = 0x00FF_FFFF;

pub fn paint(state: &State, buffer: &mut DumbBuffer) {
    let width = u32::from(buffer.width());
    let height = u32::from(buffer.height());
    let stride_words = (buffer.stride() / 4) as usize;
    let pixels = buffer.pixels_mut();

    for word in pixels.iter_mut() {
        *word = BACKGROUND;
    }

    fill_rect(
        pixels,
        stride_words,
        width,
        height,
        state.rect_x,
        state.rect_y,
        u32::from(RECT_SIZE),
        u32::from(RECT_SIZE),
        MAGENTA,
    );
    fill_rect(
        pixels,
        stride_words,
        width,
        height,
        state.cursor_x,
        state.cursor_y,
        u32::from(CURSOR_SIZE),
        u32::from(CURSOR_SIZE),
        WHITE,
    );
}

#[allow(clippy::too_many_arguments)]
fn fill_rect(
    pixels: &mut [u32],
    stride_words: usize,
    fb_w: u32,
    fb_h: u32,
    fx: f32,
    fy: f32,
    rw: u32,
    rh: u32,
    colour: u32,
) {
    let x0 = fx.max(0.0) as u32;
    let y0 = fy.max(0.0) as u32;
    if x0 >= fb_w || y0 >= fb_h {
        return;
    }
    let x1 = (x0 + rw).min(fb_w);
    let y1 = (y0 + rh).min(fb_h);
    for row in y0..y1 {
        let base = row as usize * stride_words;
        for col in x0..x1 {
            pixels[base + col as usize] = colour;
        }
    }
}
