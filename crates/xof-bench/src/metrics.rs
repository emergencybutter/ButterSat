//! Image metrics on the centre crop of each screenshot (edges can carry
//! X-Plane notifications): sharpness (mean squared Laplacian: blurry
//! fallback textures lose high frequencies) and SSIM against a reference.
//! Also converts PNGs to JPEG + thumbnail to keep runs small.

use std::path::Path;

use image::{imageops::FilterType, DynamicImage, GrayImage};

use crate::record::RunRecord;

/// Fraction of each dimension kept around the centre.
const CROP: f64 = 0.7;
/// Analysis width (the 2560 px screenshot is resized first).
const WIDTH: u32 = 1280;

pub struct Luma {
    w: usize,
    h: usize,
    px: Vec<f32>,
}

pub fn analysis_image(img: &DynamicImage) -> Luma {
    let (w, h) = (img.width(), img.height());
    let (cw, ch) = ((w as f64 * CROP) as u32, (h as f64 * CROP) as u32);
    let crop = img.crop_imm((w - cw) / 2, (h - ch) / 2, cw, ch);
    let target_w = (WIDTH as f64 * CROP) as u32;
    let target_h = (ch as f64 * target_w as f64 / cw as f64).round() as u32;
    let g: GrayImage = crop.resize_exact(target_w, target_h, FilterType::Triangle).to_luma8();
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
pub fn ssim(a: &Luma, b: &Luma) -> f64 {
    if a.w != b.w || a.h != b.h {
        return f64::NAN;
    }
    const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
    const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
    let (w, h) = (a.w, a.h);
    let (mut sum, mut n) = (0.0, 0usize);
    for by in (0..h.saturating_sub(7)).step_by(8) {
        for bx in (0..w.saturating_sub(7)).step_by(8) {
            let (mut ma, mut mb) = (0.0f64, 0.0f64);
            for y in by..by + 8 {
                for x in bx..bx + 8 {
                    ma += a.px[y * w + x] as f64;
                    mb += b.px[y * w + x] as f64;
                }
            }
            ma /= 64.0;
            mb /= 64.0;
            let (mut va, mut vb, mut cov) = (0.0f64, 0.0f64, 0.0f64);
            for y in by..by + 8 {
                for x in bx..bx + 8 {
                    let da = a.px[y * w + x] as f64 - ma;
                    let db = b.px[y * w + x] as f64 - mb;
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
                shot.similarity = refs.get(&s.spot).map(|rl| ssim(&l, rl)).filter(|v| v.is_finite());
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
}
