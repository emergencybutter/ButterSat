//! RGBA8 raster helpers: composing tiles, linear-light downsampling,
//! upscaling ancestor tiles for degraded fills.

use std::sync::OnceLock;

use crate::tile::TILE_PX;

/// An RGBA8 image, row-major, tightly packed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Rgba {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, data: vec![0; (width * height * 4) as usize] }
    }

    pub fn filled(width: u32, height: u32, px: [u8; 4]) -> Self {
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            data.extend_from_slice(&px);
        }
        Self { width, height, data }
    }

    #[inline]
    pub fn px(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        self.data[i..i + 4].try_into().unwrap()
    }

    /// Copy `src` into `self` with its top-left at (`dx`, `dy`).
    pub fn blit(&mut self, src: &Rgba, dx: u32, dy: u32) {
        let row = (src.width * 4) as usize;
        for y in 0..src.height {
            let s = (y * src.width * 4) as usize;
            let d = (((dy + y) * self.width + dx) * 4) as usize;
            self.data[d..d + row].copy_from_slice(&src.data[s..s + row]);
        }
    }

    /// Bilinear-free crop + nearest/bilinear upscale: take the square
    /// `[sx, sx+size) × [sy, sy+size)` (in source pixels, may be fractional
    /// when size < 1) and scale it to `out × out`.
    pub fn crop_scale(&self, sx: f32, sy: f32, size: f32, out: u32) -> Rgba {
        let mut dst = Rgba::new(out, out);
        let scale = size / out as f32;
        let maxx = (self.width - 1) as f32;
        let maxy = (self.height - 1) as f32;
        for y in 0..out {
            let fy = (sy + (y as f32 + 0.5) * scale - 0.5).clamp(0.0, maxy);
            let y0 = fy.floor() as u32;
            let y1 = (y0 + 1).min(self.height - 1);
            let ty = fy - y0 as f32;
            for x in 0..out {
                let fx = (sx + (x as f32 + 0.5) * scale - 0.5).clamp(0.0, maxx);
                let x0 = fx.floor() as u32;
                let x1 = (x0 + 1).min(self.width - 1);
                let tx = fx - x0 as f32;
                let (a, b, c, d) = (self.px(x0, y0), self.px(x1, y0), self.px(x0, y1), self.px(x1, y1));
                let i = ((y * out + x) * 4) as usize;
                for ch in 0..4 {
                    let top = a[ch] as f32 * (1.0 - tx) + b[ch] as f32 * tx;
                    let bot = c[ch] as f32 * (1.0 - tx) + d[ch] as f32 * tx;
                    dst.data[i + ch] = (top * (1.0 - ty) + bot * ty + 0.5) as u8;
                }
            }
        }
        dst
    }

    /// Halve both dimensions with a 2×2 box filter in linear light.
    pub fn downsample_linear(&self) -> Rgba {
        let w = (self.width / 2).max(1);
        let h = (self.height / 2).max(1);
        let lut = srgb_to_linear_lut();
        let mut dst = Rgba::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let xs = [(2 * x).min(self.width - 1), (2 * x + 1).min(self.width - 1)];
                let ys = [(2 * y).min(self.height - 1), (2 * y + 1).min(self.height - 1)];
                let mut acc = [0f32; 4];
                for &yy in &ys {
                    for &xx in &xs {
                        let p = self.px(xx, yy);
                        acc[0] += lut[p[0] as usize];
                        acc[1] += lut[p[1] as usize];
                        acc[2] += lut[p[2] as usize];
                        acc[3] += p[3] as f32;
                    }
                }
                let i = ((y * w + x) * 4) as usize;
                dst.data[i] = linear_to_srgb(acc[0] / 4.0);
                dst.data[i + 1] = linear_to_srgb(acc[1] / 4.0);
                dst.data[i + 2] = linear_to_srgb(acc[2] / 4.0);
                dst.data[i + 3] = (acc[3] / 4.0 + 0.5) as u8;
            }
        }
        dst
    }

    /// Pad to at least 4×4 by edge replication (BC1 needs whole blocks).
    pub fn pad_to_block(&self) -> Rgba {
        if self.width >= 4 && self.height >= 4 && self.width % 4 == 0 && self.height % 4 == 0 {
            return self.clone();
        }
        let w = self.width.max(4).next_multiple_of(4);
        let h = self.height.max(4).next_multiple_of(4);
        let mut dst = Rgba::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let p = self.px(x.min(self.width - 1), y.min(self.height - 1));
                let i = ((y * w + x) * 4) as usize;
                dst.data[i..i + 4].copy_from_slice(&p);
            }
        }
        dst
    }
}

/// A decoded 256×256 imagery tile.
pub type Tile = Rgba;

/// Neutral fill used when no imagery at all is available.
pub fn placeholder_tile() -> Tile {
    Rgba::filled(TILE_PX, TILE_PX, [96, 104, 88, 255])
}

fn srgb_to_linear_lut() -> &'static [f32; 256] {
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0f32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let c = i as f32 / 255.0;
            *v = if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
        }
        t
    })
}

fn linear_to_srgb(l: f32) -> u8 {
    let l = l.clamp(0.0, 1.0);
    let c = if l <= 0.003_130_8 { l * 12.92 } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 };
    (c * 255.0 + 0.5) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsample_is_gamma_correct() {
        // Black/white checkerboard averages to sRGB ~188, not 128.
        let mut img = Rgba::new(2, 2);
        for (i, v) in [255u8, 0, 0, 255].iter().enumerate() {
            let p = &mut img.data[i * 4..i * 4 + 4];
            p.copy_from_slice(&[*v, *v, *v, 255]);
        }
        let d = img.downsample_linear();
        assert_eq!((d.width, d.height), (1, 1));
        assert!((186..=190).contains(&d.data[0]), "{}", d.data[0]);
        assert_eq!(d.data[3], 255);
    }

    #[test]
    fn crop_scale_identity_and_upscale() {
        let mut img = Rgba::new(4, 4);
        for (i, b) in img.data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        assert_eq!(img.crop_scale(0.0, 0.0, 4.0, 4), img);
        let up = Rgba::filled(2, 2, [10, 20, 30, 255]).crop_scale(0.0, 0.0, 1.0, 8);
        assert!(up.data.chunks(4).all(|p| p == [10, 20, 30, 255]));
    }

    #[test]
    fn blit_and_pad() {
        let mut dst = Rgba::new(4, 2);
        dst.blit(&Rgba::filled(2, 2, [1, 2, 3, 4]), 2, 0);
        assert_eq!(dst.px(3, 1), [1, 2, 3, 4]);
        assert_eq!(dst.px(0, 0), [0, 0, 0, 0]);
        let p = Rgba::filled(1, 1, [9, 9, 9, 9]).pad_to_block();
        assert_eq!((p.width, p.height), (4, 4));
        assert!(p.data.iter().all(|&b| b == 9));
    }
}
