//! BC1 (DXT1) encoding.

use intel_tex_2::{bc1, RgbaSurface};

use crate::raster::Rgba;

/// Encode an RGBA8 image to BC1. Images smaller than a block or not a multiple
/// of 4 are edge-padded first (as for the 2×2 and 1×1 mips).
pub fn encode(img: &Rgba) -> Vec<u8> {
    let img = img.pad_to_block();
    let surface = RgbaSurface {
        width: img.width,
        height: img.height,
        stride: img.width * 4,
        data: &img.data,
    };
    bc1::compress_blocks(&surface)
}

/// Decode BC1 back to RGBA8 (tests and diagnostics only).
pub fn decode(data: &[u8], width: u32, height: u32) -> Rgba {
    let bw = width.div_ceil(4);
    let bh = height.div_ceil(4);
    let mut out = Rgba::new(bw * 4, bh * 4);
    for by in 0..bh {
        for bx in 0..bw {
            let b = &data[((by * bw + bx) * 8) as usize..][..8];
            let c0 = u16::from_le_bytes([b[0], b[1]]);
            let c1 = u16::from_le_bytes([b[2], b[3]]);
            let e = |c: u16| {
                let r = ((c >> 11) & 31) as u32;
                let g = ((c >> 5) & 63) as u32;
                let bl = (c & 31) as u32;
                [(r * 527 + 23) >> 6, (g * 259 + 33) >> 6, (bl * 527 + 23) >> 6]
            };
            let (p0, p1) = (e(c0), e(c1));
            let mix = |a: u32, b: u32, wa: u32, wb: u32| ((a * wa + b * wb) / (wa + wb)) as u8;
            let palette: [[u8; 4]; 4] = if c0 > c1 {
                [
                    [p0[0] as u8, p0[1] as u8, p0[2] as u8, 255],
                    [p1[0] as u8, p1[1] as u8, p1[2] as u8, 255],
                    [mix(p0[0], p1[0], 2, 1), mix(p0[1], p1[1], 2, 1), mix(p0[2], p1[2], 2, 1), 255],
                    [mix(p0[0], p1[0], 1, 2), mix(p0[1], p1[1], 1, 2), mix(p0[2], p1[2], 1, 2), 255],
                ]
            } else {
                [
                    [p0[0] as u8, p0[1] as u8, p0[2] as u8, 255],
                    [p1[0] as u8, p1[1] as u8, p1[2] as u8, 255],
                    [mix(p0[0], p1[0], 1, 1), mix(p0[1], p1[1], 1, 1), mix(p0[2], p1[2], 1, 1), 255],
                    [0, 0, 0, 0],
                ]
            };
            let idx = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
            for py in 0..4 {
                for px in 0..4 {
                    let sel = (idx >> (2 * (py * 4 + px))) & 3;
                    let x = bx * 4 + px;
                    let y = by * 4 + py;
                    let i = ((y * out.width + x) * 4) as usize;
                    out.data[i..i + 4].copy_from_slice(&palette[sel as usize]);
                }
            }
        }
    }
    out
}

/// Peak signal-to-noise ratio between two RGB(A) images, ignoring alpha.
pub fn psnr(a: &Rgba, b: &Rgba) -> f64 {
    assert_eq!((a.width, a.height), (b.width, b.height));
    let mut se = 0f64;
    let mut n = 0f64;
    for (pa, pb) in a.data.chunks(4).zip(b.data.chunks(4)) {
        for c in 0..3 {
            let d = pa[c] as f64 - pb[c] as f64;
            se += d * d;
            n += 1.0;
        }
    }
    if se == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0f64 * 255.0 / (se / n)).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_gradient() {
        let mut img = Rgba::new(64, 64);
        for y in 0..64 {
            for x in 0..64 {
                let i = ((y * 64 + x) * 4) as usize;
                img.data[i..i + 4].copy_from_slice(&[(x * 4) as u8, (y * 4) as u8, 128, 255]);
            }
        }
        let enc = encode(&img);
        assert_eq!(enc.len(), 16 * 16 * 8);
        let dec = decode(&enc, 64, 64);
        assert!(psnr(&img, &dec) > 35.0, "{}", psnr(&img, &dec));
    }

    #[test]
    fn tiny_mips_are_one_block() {
        assert_eq!(encode(&Rgba::filled(1, 1, [1, 2, 3, 255])).len(), 8);
        assert_eq!(encode(&Rgba::filled(2, 2, [1, 2, 3, 255])).len(), 8);
    }
}
