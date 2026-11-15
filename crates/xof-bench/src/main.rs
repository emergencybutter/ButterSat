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
