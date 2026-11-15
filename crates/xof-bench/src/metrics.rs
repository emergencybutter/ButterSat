//! Image metrics on the centre crop of each screenshot (edges can carry
//! X-Plane notifications): sharpness (mean squared Laplacian: blurry
//! fallback textures lose high frequencies) and SSIM against a reference,
//! after aligning the two images: two runs never stop at exactly the same
//! point, and a few pixels of offset would dominate a block-wise SSIM.
//! Also converts PNGs to JPEG + thumbnail to keep runs small.

use std::path::Path;

use image::{imageops::FilterType, DynamicImage, GrayImage};

use crate::record::RunRecord;

/// Fraction of each dimension kept around the centre.
const CROP: f64 = 0.7;
/// Largest offset searched when aligning to the reference (full-res px).
const MAX_SHIFT: i32 = 16;

pub struct Luma {
    pub w: usize,
    pub h: usize,
    pub px: Vec<f32>,
}

pub fn full_luma(img: &DynamicImage) -> Luma {
    let g: GrayImage = img.to_luma8();
    Luma { w: g.width() as usize, h: g.height() as usize, px: g.into_raw().into_iter().map(|v| v as f32).collect() }
}

pub fn contrast(l: &Luma) -> f64 {
    if l.px.is_empty() {
        return 0.0;
    }
    let n = l.px.len() as f64;
    let mean = l.px.iter().map(|&v| v as f64).sum::<f64>() / n;
    let var = l.px.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / n;
    var.sqrt()
}

pub fn analysis_image(img: &DynamicImage) -> Luma {
    let (w, h) = (img.width(), img.height());
    let (cw, ch) = ((w as f64 * CROP) as u32, (h as f64 * CROP) as u32);
    // Native resolution: alignment and sharpness need every pixel.
    let g: GrayImage = img.crop_imm((w - cw) / 2, (h - ch) / 2, cw, ch).to_luma8();
    Luma { w: g.width() as usize, h: g.height() as usize, px: g.into_raw().into_iter().map(|v| v as f32).collect() }
}

pub fn sharpness(l: &Luma) -> f64 {
    let (w, h) = (l.w, l.h);
    if w < 3 || h < 3 {
        return 0.0;
    }
    let mut acc = 0.0f64;
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let c = l.px[y * w + x];
            let lap =
                l.px[y * w + x - 1] + l.px[y * w + x + 1] + l.px[(y - 1) * w + x] + l.px[(y + 1) * w + x] - 4.0 * c;
            acc += (lap as f64) * (lap as f64);
        }
    }
    acc / ((w - 2) * (h - 2)) as f64
}

/// Mean SSIM over non-overlapping 8x8 windows.
#[cfg(test)]
pub fn ssim(a: &Luma, b: &Luma) -> f64 {
    ssim_at(a, b, 0, 0)
}

/// SSIM of `a` against `b` displaced by (dx, dy): pixel (x, y) of `a` is
/// compared with (x + dx, y + dy) of `b`, over the overlap.
pub fn ssim_at(a: &Luma, b: &Luma, dx: i32, dy: i32) -> f64 {
    if a.w != b.w || a.h != b.h {
        return f64::NAN;
    }
    const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
    const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
    let w = a.w;
    let (x0, x1) = (0.max(-dx) as usize, (a.w as i32 - 0.max(dx)) as usize);
    let (y0, y1) = (0.max(-dy) as usize, (a.h as i32 - 0.max(dy)) as usize);
    let bi = |x: usize, y: usize| ((y as i32 + dy) as usize) * w + (x as i32 + dx) as usize;
    let (mut sum, mut n) = (0.0, 0usize);
    for by in (y0..y1.saturating_sub(7)).step_by(8) {
        for bx in (x0..x1.saturating_sub(7)).step_by(8) {
            let (mut ma, mut mb) = (0.0f64, 0.0f64);
            for y in by..by + 8 {
                for x in bx..bx + 8 {
                    ma += a.px[y * w + x] as f64;
                    mb += b.px[bi(x, y)] as f64;
                }
            }
            ma /= 64.0;
            mb /= 64.0;
            let (mut va, mut vb, mut cov) = (0.0f64, 0.0f64, 0.0f64);
            for y in by..by + 8 {
                for x in bx..bx + 8 {
                    let da = a.px[y * w + x] as f64 - ma;
                    let db = b.px[bi(x, y)] as f64 - mb;
                    va += da * da;
                    vb += db * db;
                    cov += da * db;
                }
            }
            va /= 63.0;
            vb /= 63.0;
            cov /= 63.0;
            sum += ((2.0 * ma * mb + C1) * (2.0 * cov + C2)) / ((ma * ma + mb * mb + C1) * (va + vb + C2));
            n += 1;
        }
    }
    if n == 0 {
        f64::NAN
    } else {
        sum / n as f64
    }
}

fn downsample(l: &Luma, f: usize) -> Luma {
    let (w, h) = (l.w / f, l.h / f);
    let mut px = vec![0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for yy in 0..f {
                for xx in 0..f {
                    acc += l.px[(y * f + yy) * l.w + x * f + xx];
                }
            }
            px[y * w + x] = acc / (f * f) as f32;
        }
    }
    Luma { w, h, px }
}

/// Mean absolute difference of `a` vs `b` displaced by (dx, dy), sampled every `step` px.
fn mad(a: &Luma, b: &Luma, dx: i32, dy: i32, step: usize) -> f64 {
    let (x0, x1) = (0.max(-dx) as usize, (a.w as i32 - 0.max(dx)) as usize);
    let (y0, y1) = (0.max(-dy) as usize, (a.h as i32 - 0.max(dy)) as usize);
    let (mut acc, mut n) = (0.0f64, 0usize);
    for y in (y0..y1).step_by(step) {
        for x in (x0..x1).step_by(step) {
            let bv = b.px[((y as i32 + dy) as usize) * b.w + (x as i32 + dx) as usize];
            acc += (a.px[y * a.w + x] - bv).abs() as f64;
            n += 1;
        }
    }
    if n == 0 {
        f64::MAX
    } else {
        acc / n as f64
    }
}

fn argmin(cands: impl Iterator<Item = (i32, i32)>, f: impl Fn(i32, i32) -> f64) -> (i32, i32) {
    cands.map(|(x, y)| (f(x, y), x, y)).min_by(|p, q| p.0.total_cmp(&q.0)).map(|(_, x, y)| (x, y)).unwrap_or((0, 0))
}

/// Integer offset (dx, dy) that best aligns `b` onto `a`: coarse search at
/// 1/4 resolution, then refinement at full resolution.
pub fn align(a: &Luma, b: &Luma) -> (i32, i32) {
    let (qa, qb) = (downsample(a, 4), downsample(b, 4));
    let r = MAX_SHIFT / 4;
    let (cx, cy) = argmin((-r..=r).flat_map(|y| (-r..=r).map(move |x| (x, y))), |x, y| mad(&qa, &qb, x, y, 1));
    let (cx, cy) = (cx * 4, cy * 4);
    argmin((-4..=4).flat_map(|y| (-4..=4).map(move |x| (cx + x, cy + y))), |x, y| mad(a, b, x, y, 1))
}

fn load(run_dir: &Path, rel: &str) -> anyhow::Result<DynamicImage> {
    Ok(image::open(run_dir.join(rel))?)
}

/// Score every shot; convert PNGs to JPEG (+ thumbnail) unless `keep_png`.
pub fn score(rec: &mut RunRecord, run_dir: &Path, keep_png: bool) -> anyhow::Result<()> {
    // Reference images: the comparison variant's final shot per spot (first repeat that has one).
    let mut refs: std::collections::HashMap<String, Luma> = Default::default();
    if let Some(cmp) = rec.scenario.compare_to.clone() {
        for r in rec.runs.iter().filter(|r| r.variant == cmp) {
            for s in &r.spots {
                if refs.contains_key(&s.spot) {
                    continue;
                }
                if let Some(last) = s.shots.last() {
                    if let Ok(img) = load(run_dir, &last.file) {
                        refs.insert(s.spot.clone(), analysis_image(&img));
                    }
                }
            }
        }
    }
    for r in &mut rec.runs {
        for s in &mut r.spots {
            for shot in &mut s.shots {
                let img = match load(run_dir, &shot.file) {
                    Ok(i) => i,
                    Err(e) => {
                        eprintln!("skip {}: {e}", shot.file);
                        continue;
                    }
                };
                let l = analysis_image(&img);
                shot.sharpness = Some(sharpness(&l));
                match refs.get(&s.spot) {
                    Some(rl) => {
                        let (dx, dy) = align(&l, rl);
                        shot.offset_px = Some((dx, dy));
                        shot.similarity = Some(ssim_at(&l, rl, dx, dy)).filter(|v| v.is_finite());
                    }
                    None => {
                        shot.offset_px = None;
                        shot.similarity = None;
                    }
                }
                if shot.file.ends_with(".png") {
                    let jpg = shot.file.trim_end_matches(".png").to_string() + ".jpg";
                    let thumb = shot.file.trim_end_matches(".png").to_string() + ".thumb.jpg";
                    let rgb = img.to_rgb8();
                    save_jpeg(&DynamicImage::ImageRgb8(rgb.clone()), &run_dir.join(&jpg), 90)?;
                    let t = DynamicImage::ImageRgb8(rgb).resize(480, 480, FilterType::Triangle);
                    save_jpeg(&t, &run_dir.join(&thumb), 80)?;
                    if !keep_png {
                        let _ = std::fs::remove_file(run_dir.join(&shot.file));
                    }
                    shot.file = jpg;
                    shot.thumb = Some(thumb);
                }
            }
        }
    }
    Ok(())
}

fn save_jpeg(img: &DynamicImage, path: &Path, q: u8) -> anyhow::Result<()> {
    let f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(f, q);
    enc.encode_image(img)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker(blur: bool) -> DynamicImage {
        let mut g = GrayImage::new(512, 288);
        for (x, y, p) in g.enumerate_pixels_mut() {
            p.0 = [if ((x / 4) + (y / 4)) % 2 == 0 { 40 } else { 220 }];
        }
        let d = DynamicImage::ImageLuma8(g);
        if blur {
            d.blur(3.0)
        } else {
            d
        }
    }

    #[test]
    fn blur_lowers_sharpness_and_similarity() {
        let sharp = analysis_image(&checker(false));
        let blurred = analysis_image(&checker(true));
        assert!(sharpness(&sharp) > 3.0 * sharpness(&blurred));
        assert!((ssim(&sharp, &sharp) - 1.0).abs() < 1e-9);
        assert!(ssim(&sharp, &blurred) < 0.8);
    }

    fn noise(w: usize, h: usize, seed: u64) -> Luma {
        let mut s = seed;
        let px = (0..w * h)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((s >> 33) % 256) as f32
            })
            .collect();
        Luma { w, h, px }
    }

    /// 3x3 box blur, so the coarse 1/4-resolution search has signal.
    fn smooth(l: &Luma) -> Luma {
        let mut px = l.px.clone();
        for y in 1..l.h - 1 {
            for x in 1..l.w - 1 {
                let mut a = 0.0;
                for oy in -1i32..=1 {
                    for ox in -1i32..=1 {
                        a += l.px[((y as i32 + oy) as usize) * l.w + (x as i32 + ox) as usize];
                    }
                }
                px[y * l.w + x] = a / 9.0;
            }
        }
        Luma { w: l.w, h: l.h, px }
    }

    #[test]
    fn alignment_recovers_offsets() {
        let base = smooth(&smooth(&noise(400, 240, 7)));
        for (dx, dy) in [(0, 0), (5, -3), (-4, -3), (11, 7)] {
            // Build b with b(x + dx, y + dy) == a(x, y).
            let mut b = Luma { w: base.w, h: base.h, px: vec![0.0; base.px.len()] };
            for y in 0..base.h as i32 {
                for x in 0..base.w as i32 {
                    let (sx, sy) = (x - dx, y - dy);
                    if sx >= 0 && sy >= 0 && (sx as usize) < base.w && (sy as usize) < base.h {
                        b.px[y as usize * base.w + x as usize] = base.px[sy as usize * base.w + sx as usize];
                    }
                }
            }
            assert_eq!(align(&base, &b), (dx, dy));
            assert!(ssim_at(&base, &b, dx, dy) > 0.999);
            if (dx, dy) != (0, 0) {
                assert!(ssim(&base, &b) < 0.9);
            }
        }
    }
}
