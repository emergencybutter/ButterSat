//! xof-bench: automated in-sim A/B benchmark for ButterSat.
//!
//! `xof-bench run bench/nyc.toml` launches X-Plane 12 once per variant,
//! slews between the scenario's spots with the camera pointing straight down,
//! takes timed screenshots, and writes `run.json` + `report.html`.
//! `xof-bench report <run-dir>` re-scores and rebuilds the report.

mod daemon;
mod metrics;
mod packs;
mod record;
mod report;
mod run;
mod scenario;
mod xp;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xof-bench", about = "Automated in-sim A/B benchmark (X-Plane 12.4+)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a scenario. Takes over X-Plane: it is killed and relaunched per variant.
    Run {
        scenario: PathBuf,
        /// Only these variants (comma-separated), in this order.
        #[arg(long, value_delimiter = ',')]
        variants: Vec<String>,
        /// Output root; each run gets its own folder.
        #[arg(long, default_value = "bench-runs")]
        out: PathBuf,
        /// Run the variant list this many times (persistent caches warm up).
        #[arg(long, default_value_t = 1)]
        repeat: u32,
        /// X-Plane folder (auto-detected otherwise).
        #[arg(long)]
        xplane: Option<PathBuf>,
        #[arg(long, default_value_t = 8086)]
        port: u16,
        /// xof-daemon binary (default: next to xof-bench).
        #[arg(long)]
        daemon: Option<PathBuf>,
        /// Keep the original PNG screenshots (≈7 MB each).
        #[arg(long)]
        keep_png: bool,
    },
    /// Re-score a run folder and rebuild its report.
    Report { run_dir: PathBuf },
    /// Fold the variant runs of other run folders into `into` (same scenario),
    /// replacing failed runs of the same variant, then rebuild the report.
    Merge { into: PathBuf, from: Vec<PathBuf> },
    /// Compare open regional providers against Esri directly on tiles (no X-Plane).
    Tiles,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new()?;
    match cli.cmd {
        Cmd::Run { scenario, variants, out, repeat, xplane, port, daemon, keep_png } => {
            let sc = scenario::Scenario::load(&scenario)?;
            let selected: Vec<scenario::Variant> = if variants.is_empty() {
                sc.variants.clone()
            } else {
                variants
                    .iter()
                    .map(|n| {
                        sc.variants
                            .iter()
                            .find(|v| &v.name == n)
                            .cloned()
                            .ok_or_else(|| anyhow::anyhow!("no variant '{n}' in {}", scenario.display()))
                    })
                    .collect::<anyhow::Result<_>>()?
            };
            let install = match xplane {
                Some(p) => p,
                None => xof_proto::xplane::resolve(None)
                    .ok_or_else(|| anyhow::anyhow!("X-Plane 12 not found; pass --xplane"))?,
            };
            anyhow::ensure!(
                xof_proto::xplane::is_valid_install(&install),
                "{} is not an X-Plane 12 folder",
                install.display()
            );
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
            let run_dir = out.join(format!("{}-{stamp}", sc.name));
            println!("xof-bench: {} variants x {} spots -> {}", selected.len(), sc.spots.len(), run_dir.display());
            println!(
                "X-Plane at {} will be killed and relaunched; scenery_packs.ini is restored afterwards.",
                install.display()
            );
            let opts = run::RunOpts { install, port, out: out.clone(), daemon_exe: daemon, repeat: repeat.max(1) };
            let mut rec = rt.block_on(async {
                tokio::select! {
                    r = run::run(&sc, &selected, &opts, &run_dir) => r,
                    _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("interrupted")),
                }
            });
            // On Ctrl-C the run future is dropped: make sure X-Plane is gone (packs restore on drop).
            if rec.is_err() {
                rt.block_on(async {
                    let _ = xp::ensure_stopped().await;
                });
            }
            let rec = rec.as_mut().map_err(|e| anyhow::anyhow!("{e:#}"))?;
            finish(rec, &run_dir, keep_png)?;
        }
        Cmd::Report { run_dir } => {
            let mut rec = load(&run_dir)?;
            finish(&mut rec, &run_dir, true)?;
        }
        Cmd::Merge { into, from } => {
            let mut rec = load(&into)?;
            for dir in &from {
                merge(&mut rec, &into, &load(dir)?, dir)?;
            }
            finish(&mut rec, &into, true)?;
        }
        Cmd::Tiles => rt.block_on(bench_tiles())?,
    }
    Ok(())
}

async fn bench_tiles() -> anyhow::Result<()> {
    use xof_core::provider::{self, Fetched, Fetcher, Priority};
    use xof_core::tile::TILE_PX;
    use xof_core::{bc1, TileCoord};

    struct Spot {
        name: &'static str,
        open_id: &'static str,
        lat: f64,
        lon: f64,
    }
    let spots = [
        Spot { name: "NYC-JFK", open_id: "usgs", lat: 40.6413, lon: -73.7781 },
        Spot { name: "NYC-CentralPark", open_id: "usgs", lat: 40.7812, lon: -73.9665 },
        Spot { name: "NYC-LGA", open_id: "usgs", lat: 40.7769, lon: -73.8740 },
        Spot { name: "Paris-CDG", open_id: "ign", lat: 49.0097, lon: 2.5479 },
        Spot { name: "Zurich-ZRH", open_id: "swisstopo", lat: 47.4647, lon: 8.5492 },
        Spot { name: "Amsterdam-AMS", open_id: "pdok", lat: 52.3105, lon: 4.7683 },
        Spot { name: "Madrid-MAD", open_id: "pnoa", lat: 40.4983, lon: -3.5676 },
        Spot { name: "Vienna-VIE", open_id: "basemap_at", lat: 48.1103, lon: 16.5697 },
    ];

    let mean_rgb = |rgba: &[u8]| -> [f64; 3] {
        let n = (rgba.len() / 4).max(1) as f64;
        let (mut r, mut g, mut b) = (0.0, 0.0, 0.0);
        for px in rgba.chunks_exact(4) {
            r += px[0] as f64;
            g += px[1] as f64;
            b += px[2] as f64;
        }
        [r / n, g / n, b / n]
    };

    let bc1_psnr = |tile: &xof_core::raster::Tile| -> f64 {
        let enc = bc1::encode(tile);
        let dec = bc1::decode(&enc, TILE_PX, TILE_PX);
        let mut mse = 0.0f64;
        let mut count = 0usize;
        for (a, b) in tile.data.chunks_exact(4).zip(dec.data.chunks_exact(4)) {
            for c in 0..3 {
                let d = a[c] as f64 - b[c] as f64;
                mse += d * d;
                count += 1;
            }
        }
        mse /= count.max(1) as f64;
        if mse <= 1e-9 {
            99.0
        } else {
            10.0 * (255.0 * 255.0 / mse).log10()
        }
    };

    println!(
        "{:<16} {:<4} {:<11} {:>8} {:>9} {:>9} {:>9} {:>17} {:>8}",
        "Spot", "ZL", "Provider", "Bytes", "Sharpness", "Contrast", "BC1-PSNR", "Mean RGB", "vs-Esri"
    );
    println!("{}", "-".repeat(100));

    let esri_fetcher = Fetcher::new(provider::builtin("esri").unwrap())?;

    for s in &spots {
        let open_p = provider::builtin(s.open_id).unwrap();
        let open_fetcher = Fetcher::new(open_p.clone())?;
        for &z in &[16u8, 17u8] {
            let t = TileCoord::from_latlon(s.lat, s.lon, z);
            let esri_fetched = esri_fetcher.fetch(t, Priority::Live).await.map_err(|e| anyhow::anyhow!(e))?;
            let (esri_bytes, esri_tile) = match esri_fetched {
                Fetched::Data(b) => (b.len(), provider::decode_tile(&b)?),
                Fetched::NoData => continue,
            };
            let esri_luma = metrics::full_luma(&image::DynamicImage::ImageRgba8(
                image::RgbaImage::from_raw(TILE_PX, TILE_PX, esri_tile.data.clone()).unwrap(),
            ));
            let esri_sharp = metrics::sharpness(&esri_luma);
            let esri_cont = metrics::contrast(&esri_luma);
            let esri_rgb = mean_rgb(&esri_tile.data);
            let esri_psnr = bc1_psnr(&esri_tile);
            println!(
                "{:<16} z{:<3} {:<11} {:>8} {:>9.1} {:>9.1} {:>8.2}dB {:>5.0},{:>4.0},{:>4.0} {:>8}",
                s.name,
                z,
                "esri",
                esri_bytes,
                esri_sharp,
                esri_cont,
                esri_psnr,
                esri_rgb[0],
                esri_rgb[1],
                esri_rgb[2],
                "ref"
            );

            // Open provider (at z17 for max_zoom=16 providers like USGS, upscale z16 ancestor 2x as engine does):
            let (open_label, open_bytes, open_tile) = if z > open_p.max_zoom() {
                let k = z - open_p.max_zoom();
                let a = t.ancestor(k).unwrap();
                match open_fetcher.fetch(a, Priority::Live).await.map_err(|e| anyhow::anyhow!(e))? {
                    Fetched::Data(b) => {
                        let parent = provider::decode_tile(&b)?;
                        let size = TILE_PX as f32 / (1u32 << k) as f32;
                        let sx = (t.x - (a.x << k)) as f32 * size;
                        let sy = (t.y - (a.y << k)) as f32 * size;
                        (
                            format!("{}↑z{}", s.open_id, open_p.max_zoom()),
                            b.len(),
                            parent.crop_scale(sx, sy, size, TILE_PX),
                        )
                    }
                    Fetched::NoData => continue,
                }
            } else {
                match open_fetcher.fetch(t, Priority::Live).await.map_err(|e| anyhow::anyhow!(e))? {
                    Fetched::Data(b) => (s.open_id.to_string(), b.len(), provider::decode_tile(&b)?),
                    Fetched::NoData => continue,
                }
            };
            let open_luma = metrics::full_luma(&image::DynamicImage::ImageRgba8(
                image::RgbaImage::from_raw(TILE_PX, TILE_PX, open_tile.data.clone()).unwrap(),
            ));
            let open_sharp = metrics::sharpness(&open_luma);
            let open_cont = metrics::contrast(&open_luma);
            let open_rgb = mean_rgb(&open_tile.data);
            let open_psnr = bc1_psnr(&open_tile);
            let ssim = metrics::ssim_at(&open_luma, &esri_luma, 0, 0);
            println!(
                "{:<16} z{:<3} {:<11} {:>8} {:>9.1} {:>9.1} {:>8.2}dB {:>5.0},{:>4.0},{:>4.0} {:>8.3}",
                s.name,
                z,
                open_label,
                open_bytes,
                open_sharp,
                open_cont,
                open_psnr,
                open_rgb[0],
                open_rgb[1],
                open_rgb[2],
                ssim
            );
        }
    }
    Ok(())
}

fn load(dir: &std::path::Path) -> anyhow::Result<record::RunRecord> {
    Ok(serde_json::from_slice(&std::fs::read(dir.join("run.json"))?)?)
}

fn merge(
    rec: &mut record::RunRecord,
    into: &std::path::Path,
    other: &record::RunRecord,
    other_dir: &std::path::Path,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        rec.scenario.name == other.scenario.name,
        "different scenarios: {} vs {}",
        rec.scenario.name,
        other.scenario.name
    );
    for r in other.runs.iter().filter(|r| r.error.is_none()) {
        rec.runs.retain(|x| !(x.variant == r.variant && x.error.is_some()));
        let mut r = r.clone();
        // A fresh shots folder name in the target run.
        let mut k = 0;
        let folder = loop {
            let f =
                if k == 0 { format!("{}-{}", r.variant, r.repeat) } else { format!("{}-{}-m{k}", r.variant, r.repeat) };
            if !into.join("shots").join(&f).exists() {
                break f;
            }
            k += 1;
        };
        std::fs::create_dir_all(into.join("shots").join(&folder))?;
        for s in &mut r.spots {
            for shot in &mut s.shots {
                for f in std::iter::once(&mut shot.file).chain(shot.thumb.as_mut()) {
                    let name = std::path::Path::new(f.as_str()).file_name().unwrap().to_string_lossy().to_string();
                    std::fs::copy(other_dir.join(f.as_str()), into.join("shots").join(&folder).join(&name))?;
                    *f = format!("shots/{folder}/{name}");
                }
            }
        }
        println!("merged {} from {}", r.variant, other_dir.display());
        rec.runs.push(r);
    }
    Ok(())
}

fn finish(rec: &mut record::RunRecord, run_dir: &std::path::Path, keep_png: bool) -> anyhow::Result<()> {
    println!("scoring screenshots…");
    metrics::score(rec, run_dir, keep_png)?;
    std::fs::write(run_dir.join("run.json"), serde_json::to_vec_pretty(rec)?)?;
    std::fs::write(run_dir.join("report.html"), report::html(rec))?;
    for s in report::summarize(rec) {
        println!(
            "{:<20} load {:>4.0}s  sim@0 {:>5}  sim final {:>5}  sharp after {:>5}s  slew {:>3.0} fps, {:.1} stalls/min{}",
            s.label,
            s.load_s,
            s.sim_first.map(|v| format!("{v:.3}")).unwrap_or("–".into()),
            s.sim_final.map(|v| format!("{v:.3}")).unwrap_or("–".into()),
            s.t_sharp.map(|v| format!("{v:.1}")).unwrap_or("–".into()),
            s.slew.fps_mean,
            s.slew.stalls_per_min,
            s.error.map(|e| format!("  FAILED: {e}")).unwrap_or_default()
        );
    }
    println!("report: {}", run_dir.join("report.html").display());
    Ok(())
}
