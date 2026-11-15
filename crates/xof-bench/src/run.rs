//! The run loop: per variant, set up packs (and the daemon), launch X-Plane,
//! start a flight at the first spot, freeze the aircraft with the camera
//! looking straight down, then slew spot to spot taking timed screenshots.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::json;

use crate::daemon::{self, Daemon};
use crate::packs::PackGuard;
use crate::record::*;
use crate::scenario::{CacheMode, Scenario, Spot, Variant};
use crate::xp::{self, Xp};

pub struct RunOpts {
    pub install: PathBuf,
    pub port: u16,
    pub out: PathBuf,
    pub daemon_exe: Option<PathBuf>,
    pub repeat: u32,
}

const FT: f64 = 0.3048;
const KT: f64 = 0.514_444;

fn log(msg: impl AsRef<str>) {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let (h, m, s) = ((t / 3600) % 24, (t / 60) % 60, t % 60);
    println!("[{h:02}:{m:02}:{s:02}Z] {}", msg.as_ref());
}

pub async fn run(sc: &Scenario, variants: &[Variant], opts: &RunOpts, run_dir: &Path) -> anyhow::Result<RunRecord> {
    anyhow::ensure!(
        !variants.iter().any(|v| v.xof) || !daemon::already_running().await,
        "an xporthofs daemon is already running: quit it first (the bench starts its own)"
    );
    std::fs::create_dir_all(run_dir)?;
    let xp = Arc::new(Xp::new(opts.port));
    let origin = match (&sc.origin, variants.iter().any(|v| v.xof)) {
        (Some(o), true) => Some(daemon::serve(o)?),
        _ => None,
    };
    let mut packs = PackGuard::new(&opts.install)?;
    let mut rec = RunRecord {
        scenario: sc.clone(),
        xplane_version: String::new(),
        started_unix: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(),
        runs: vec![],
    };
    let result: anyhow::Result<()> = async {
        for repeat in 0..opts.repeat {
            for v in variants {
                log(format!("=== variant {} (run {}) ===", v.name, repeat + 1));
                let mut vr = VariantRun::default();
                // A flight that never gets going (X-Plane wedged at start) is
                // retried once from scratch; failures after the first spot are kept.
                for attempt in 1..=2 {
                    vr = VariantRun { variant: v.name.clone(), repeat, ..Default::default() };
                    let r =
                        run_variant(sc, v, opts, run_dir, &xp, &mut packs, origin.as_ref(), &mut vr, &mut rec).await;
                    let Err(e) = r else { break };
                    log(format!("variant {} failed (attempt {attempt}): {e:#}", v.name));
                    vr.error = Some(format!("{e:#}"));
                    if !vr.spots.is_empty() {
                        break;
                    }
                }
                rec.runs.push(vr);
                std::fs::write(run_dir.join("run.json"), serde_json::to_vec_pretty(&rec)?)?;
            }
        }
        Ok(())
    }
    .await;
    let _ = xp::ensure_stopped().await;
    packs.restore();
    result?;
    Ok(rec)
}

#[allow(clippy::too_many_arguments)]
async fn run_variant(
    sc: &Scenario,
    v: &Variant,
    opts: &RunOpts,
    run_dir: &Path,
    xp: &Arc<Xp>,
    packs: &mut PackGuard,
    origin: Option<&daemon::Origin>,
    vr: &mut VariantRun,
    rec: &mut RunRecord,
) -> anyhow::Result<()> {
    xp::ensure_stopped().await?;
    xp.reset();
    packs.apply(&v.name, &v.packs, v.xof, &sc.disable_packs, sc.isolate_packs)?;

    let mut dmn = None;
    if v.xof {
        let home = match v.cache {
            CacheMode::Cold => run_dir.join("homes").join(format!("{}-{}", v.name, vr.repeat)),
            CacheMode::Persistent => opts.out.join("homes").join(&v.name),
        };
        if v.cache == CacheMode::Cold && home.exists() {
            std::fs::remove_dir_all(&home)?;
        }
        let cfg =
            daemon::config_toml(&opts.install, opts.port, origin.map(|o| o.url.as_str()), &sc.public_keys, &v.config)?;
        let exe = daemon::daemon_exe(opts.daemon_exe.as_deref())?;
        log(format!("starting daemon (home {})", home.display()));
        dmn = Some(Daemon::start(&exe, &home, &cfg).await?);
    }
    let r = fly(sc, v, opts, run_dir, xp, dmn.as_ref(), vr, rec).await;
    if r.is_err() {
        // What was on screen (a dialog?) before X-Plane is killed.
        let dir = run_dir.join("failures");
        let _ = std::fs::create_dir_all(&dir);
        let stamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let png = dir.join(format!("{}-{}-{stamp}.png", v.name, vr.repeat));
        if capture_desktop(&png) {
            log(format!("desktop captured: {}", png.display()));
        }
    }
    let _ = xp::ensure_stopped().await;
    if let Some(mut d) = dmn {
        vr.daemon_end = d.engine().await;
        d.stop().await;
    }
    r
}

#[allow(clippy::too_many_arguments)]
async fn fly(
    sc: &Scenario,
    v: &Variant,
    opts: &RunOpts,
    run_dir: &Path,
    xp: &Arc<Xp>,
    dmn: Option<&Daemon>,
    vr: &mut VariantRun,
    rec: &mut RunRecord,
) -> anyhow::Result<()> {
    let t_launch = Instant::now();
    xp::launch(&opts.install)?;
    xp::wait_until("X-Plane Web API", Duration::from_secs(300), || xp.alive()).await?;
    vr.launch_s = t_launch.elapsed().as_secs_f64();
    rec.xplane_version = xp.version().await.unwrap_or_default();
    log(format!("X-Plane {} up after {:.0}s", rec.xplane_version, vr.launch_s));
    if let Some(d) = dmn {
        vr.daemon_start = d.engine().await;
    }

    // Starting a flight while the main menu is still settling can leave the
    // Flight Configuration window open on top of the view.
    tokio::time::sleep(Duration::from_secs(15)).await;
    let first = &sc.spots[0];
    let t0 = Instant::now();
    let frames = Arc::new(Mutex::new(Vec::<FrameSample>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let monitor = tokio::spawn(monitor(xp.clone(), t0, frames.clone(), stop.clone()));
    let res = async {
        xp.start_flight(&xp::flight_init(
            &sc.aircraft,
            first.lat,
            first.lon,
            first.alt_ft * FT,
            0.0,
            sc.day_of_year,
            sc.local_time_h,
        ))
        .await?;
        // Freeze as soon as datarefs exist so the aircraft doesn't fly off.
        xp::wait_until("flight datarefs", Duration::from_secs(300), || async {
            xp.dataref_count().await.unwrap_or(0) > 0
                && xp.set_index("sim/operation/override/override_planepath", 0, json!(1)).await.is_ok()
        })
        .await?;
        wait_loaded(xp).await?;
        vr.load_s = t0.elapsed().as_secs_f64();
        log(format!("flight loaded after {:.0}s", vr.load_s));

        let shots_dir = run_dir.join("shots").join(format!("{}-{}", v.name, vr.repeat));
        std::fs::create_dir_all(&shots_dir)?;
        let screenshots = opts.install.join("Output").join("screenshots");
        for (i, spot) in sc.spots.iter().enumerate() {
            let mut sr =
                SpotRun { spot: spot.name.clone(), slew_start: t0.elapsed().as_secs_f64(), ..Default::default() };
            if i > 0 {
                let d = slew_to(xp, spot, spot.slew_speed_kt.unwrap_or(sc.slew_speed_kt) * KT).await?;
                log(format!("slewed to {} ({:.1} km)", spot.name, d / 1000.0));
            }
            camera(xp, sc.fov_deg).await?;
            let _ = xp
                .patch_flight(
                    &json!({ "local_time": { "day_of_year": sc.day_of_year, "time_in_24_hours": sc.local_time_h } }),
                )
                .await;
            let arrive = Instant::now();
            sr.arrive = t0.elapsed().as_secs_f64();
            if let Some(d) = dmn {
                sr.daemon_arrive = d.engine().await;
            }
            for &ts in &sc.shots_s {
                let wait = Duration::from_secs_f64(ts).saturating_sub(arrive.elapsed());
                tokio::time::sleep(wait).await;
                let t = arrive.elapsed().as_secs_f64();
                let name = format!("{:02}-{}-t{:05.1}.png", i, sanitize(&spot.name), ts);
                screenshot(xp, &screenshots, &shots_dir.join(&name)).await?;
                let rel = format!("shots/{}-{}/{name}", v.name, vr.repeat);
                sr.shots.push(Shot { t, file: rel, ..Default::default() });
            }
            sr.end = t0.elapsed().as_secs_f64();
            if let Some(d) = dmn {
                sr.daemon_end = d.engine().await;
            }
            log(format!("{}: {} shots", spot.name, sr.shots.len()));
            vr.spots.push(sr);
        }
        anyhow::Ok(())
    }
    .await;
    stop.store(true, Ordering::Relaxed);
    let _ = monitor.await;
    vr.frames = std::mem::take(&mut *frames.lock());
    res
}

/// Screenshot of the whole desktop (Windows only; best effort).
fn capture_desktop(path: &Path) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; \
         $b=[System.Windows.Forms.Screen]::PrimaryScreen.Bounds; \
         $bmp=New-Object System.Drawing.Bitmap $b.Width,$b.Height; \
         [System.Drawing.Graphics]::FromImage($bmp).CopyFromScreen($b.Location,[System.Drawing.Point]::Empty,$b.Size); \
         $bmp.Save('{}')",
        path.display().to_string().replace('\'', "''")
    );
    std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
        && path.exists()
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

async fn monitor(xp: Arc<Xp>, t0: Instant, out: Arc<Mutex<Vec<FrameSample>>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let t = Instant::now();
        let period = xp.get_f64("sim/time/framerate_period").await.unwrap_or(f64::NAN);
        out.lock().push(FrameSample {
            t: t.duration_since(t0).as_secs_f64(),
            period,
            latency: t.elapsed().as_secs_f64(),
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Loaded = 3 s of frames under 100 ms.
async fn wait_loaded(xp: &Xp) -> anyhow::Result<()> {
    let t = Instant::now();
    let mut good_since: Option<Instant> = None;
    loop {
        anyhow::ensure!(t.elapsed() < Duration::from_secs(600), "flight never finished loading");
        let rt = Instant::now();
        let p = xp.get_f64("sim/time/framerate_period").await.unwrap_or(f64::NAN);
        if p.is_finite() && p < 0.1 && rt.elapsed() < Duration::from_millis(100) {
            let since = *good_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_secs(3) {
                return Ok(());
            }
        } else {
            good_since = None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Camera: pilot's head 4 m below the fuselage (outside the 3D cockpit),
/// looking straight down; north up because the flight starts heading 0.
async fn camera(xp: &Xp, fov: f64) -> anyhow::Result<()> {
    // A flight started from the main menu can leave Flight Configuration open.
    let _ = xp.command("sim/operation/close_windows").await;
    xp.set_index("sim/operation/override/override_planepath", 0, json!(1)).await?;
    xp.set("sim/graphics/view/pilots_head_y", json!(-4.0)).await?;
    xp.set("sim/graphics/view/pilots_head_psi", json!(0.0)).await?;
    xp.set("sim/graphics/view/pilots_head_the", json!(-89.9)).await?;
    xp.set("sim/graphics/view/field_of_view_deg", json!(fov)).await?;
    Ok(())
}

/// Metres east/north/up from (lat, lon, elev) to the spot.
fn offset(lat: f64, lon: f64, elev: f64, s: &Spot) -> (f64, f64, f64) {
    let e = (s.lon - lon) * 111_320.0 * lat.to_radians().cos();
    let n = (s.lat - lat) * 110_574.0;
    (e, n, s.alt_ft * FT - elev)
}

/// Closed-loop slew: every step reads the true position and the current
/// OpenGL-local position (which jumps when X-Plane shifts its reference
/// point), then moves the aircraft `speed * dt` towards the spot.
/// Local axes: +x east, +y up, +z south. Returns the distance covered.
async fn slew_to(xp: &Xp, s: &Spot, speed: f64) -> anyhow::Result<f64> {
    let names = [
        "sim/flightmodel/position/latitude",
        "sim/flightmodel/position/longitude",
        "sim/flightmodel/position/elevation",
        "sim/flightmodel/position/local_x",
        "sim/flightmodel/position/local_y",
        "sim/flightmodel/position/local_z",
    ];
    let read = || async {
        let r = futures::future::join_all(names.iter().map(|n| xp.get_f64(n))).await;
        r.into_iter().collect::<anyhow::Result<Vec<f64>>>()
    };
    let p = read().await?;
    let (e, n, u) = offset(p[0], p[1], p[2], s);
    let total = (e * e + n * n + u * u).sqrt();
    let t = Instant::now();
    let budget = Duration::from_secs_f64(total / speed * 3.0 + 60.0);
    let mut last = Instant::now();
    loop {
        anyhow::ensure!(t.elapsed() < budget, "slew to {} timed out", s.name);
        let p = read().await?;
        let (e, n, u) = offset(p[0], p[1], p[2], s);
        let d = (e * e + n * n + u * u).sqrt();
        // Tight: at 3000 ft one screenshot pixel is ~0.4 m, and runs are compared pixel by pixel.
        if d < 0.15 {
            return Ok(total);
        }
        let dt = last.elapsed().as_secs_f64().clamp(0.0, 0.25);
        last = Instant::now();
        let f = ((speed * dt).max(1.0) / d).min(1.0);
        futures::future::try_join3(
            xp.set("sim/flightmodel/position/local_x", json!(p[3] + e * f)),
            xp.set("sim/flightmodel/position/local_y", json!(p[4] + u * f)),
            xp.set("sim/flightmodel/position/local_z", json!(p[5] - n * f)),
        )
        .await?;
    }
}

/// Take a screenshot and move it to `dest` once X-Plane has finished writing it.
async fn screenshot(xp: &Xp, dir: &Path, dest: &Path) -> anyhow::Result<()> {
    let before: std::collections::HashSet<_> =
        std::fs::read_dir(dir).map(|r| r.flatten().map(|e| e.file_name()).collect()).unwrap_or_default();
    xp.command("sim/operation/screenshot").await?;
    let t = Instant::now();
    let file = loop {
        anyhow::ensure!(t.elapsed() < Duration::from_secs(15), "no screenshot appeared in {}", dir.display());
        tokio::time::sleep(Duration::from_millis(200)).await;
        let new = std::fs::read_dir(dir)
            .map(|r| {
                r.flatten()
                    .find(|e| !before.contains(&e.file_name()) && e.path().extension().is_some_and(|x| x == "png"))
            })
            .ok()
            .flatten();
        if let Some(e) = new {
            break e.path();
        }
    };
    // Wait for the size to settle, then move it.
    let mut size = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let s = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        if s > 0 && s == size {
            break;
        }
        size = s;
        anyhow::ensure!(t.elapsed() < Duration::from_secs(30), "screenshot never finished writing");
    }
    if std::fs::rename(&file, dest).is_err() {
        std::fs::copy(&file, dest)?;
        let _ = std::fs::remove_file(&file);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets() {
        let s = Spot { name: "x".into(), lat: 40.65, lon: -73.77, alt_ft: 1000.0, slew_speed_kt: None };
        let (e, n, u) = offset(40.64, -73.78, 0.0, &s);
        assert!((e - 844.0).abs() < 5.0, "{e}");
        assert!((n - 1105.7).abs() < 1.0, "{n}");
        assert!((u - 304.8).abs() < 1e-9);
    }
}
