//! Self-contained HTML report (images referenced relatively): summary table,
//! then per spot a convergence chart and the screenshot grid.

use std::fmt::Write as _;

use crate::record::*;

const LIGHT: [&str; 8] = ["#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4", "#008300", "#6250d6", "#e34948"];
const DARK: [&str; 8] = ["#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#0ca30c", "#9085e9", "#e66767"];

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn label(r: &VariantRun, multi_repeat: bool) -> String {
    if multi_repeat {
        format!("{} #{}", r.variant, r.repeat + 1)
    } else {
        r.variant.clone()
    }
}

/// Per spot: first t where sharpness reaches 90 % of the run's final value.
pub fn time_to_sharp(s: &SpotRun) -> Option<f64> {
    let last = s.shots.last()?.sharpness?;
    s.shots.iter().find(|x| x.sharpness.is_some_and(|v| v >= 0.9 * last)).map(|x| x.t)
}

fn mean(it: impl Iterator<Item = f64>) -> Option<f64> {
    let v: Vec<f64> = it.filter(|x| x.is_finite()).collect();
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

fn f(v: Option<f64>, digits: usize) -> String {
    v.map(|x| format!("{x:.digits$}")).unwrap_or_else(|| "–".into())
}

pub struct Summary {
    pub label: String,
    pub error: Option<String>,
    pub load_s: f64,
    pub sim_first: Option<f64>,
    pub sim_final: Option<f64>,
    pub t_sharp: Option<f64>,
    pub slew: FrameStats,
    pub hold: FrameStats,
    pub engine: Option<xof_proto::EngineStatsSnapshot>,
}

pub fn summarize(rec: &RunRecord) -> Vec<Summary> {
    let multi = rec.runs.iter().any(|r| r.repeat > 0);
    rec.runs
        .iter()
        .map(|r| {
            let slews: Vec<FrameSample> = r
                .spots
                .iter()
                .skip(1)
                .flat_map(|s| r.frames.iter().filter(move |x| x.t >= s.slew_start && x.t < s.arrive).copied())
                .collect();
            let slew_secs: f64 = r.spots.iter().skip(1).map(|s| s.arrive - s.slew_start).sum();
            let holds: Vec<FrameSample> = r
                .spots
                .iter()
                .flat_map(|s| r.frames.iter().filter(move |x| x.t >= s.arrive && x.t <= s.end).copied())
                .collect();
            let hold_secs: f64 = r.spots.iter().map(|s| s.end - s.arrive).sum();
            // Concatenated windows: rebase time so the per-minute rate uses the windows' total length.
            let stats = |v: &[FrameSample], secs: f64| {
                let mut st = frame_stats(
                    &v.iter().enumerate().map(|(i, x)| FrameSample { t: i as f64, ..*x }).collect::<Vec<_>>(),
                    0.0,
                    f64::MAX,
                );
                st.stalls_per_min = v.iter().filter(|x| x.latency > 0.1).count() as f64 / (secs / 60.0).max(1.0 / 60.0);
                st
            };
            Summary {
                label: label(r, multi),
                error: r.error.clone(),
                load_s: r.load_s,
                sim_first: mean(r.spots.iter().filter_map(|s| s.shots.first()?.similarity)),
                sim_final: mean(r.spots.iter().filter_map(|s| s.shots.last()?.similarity)),
                t_sharp: mean(r.spots.iter().filter_map(time_to_sharp)),
                slew: stats(&slews, slew_secs),
                hold: stats(&holds, hold_secs),
                engine: match (&r.daemon_start, &r.daemon_end) {
                    (Some(a), Some(b)) => Some(engine_delta(a, b)),
                    _ => None,
                },
            }
        })
        .collect()
}

pub fn html(rec: &RunRecord) -> String {
    let multi = rec.runs.iter().any(|r| r.repeat > 0);
    let sc = &rec.scenario;
    let mut h = String::new();
    let _ = write!(
        h,
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>xof-bench {name}</title><style>
:root{{color-scheme:light;--bg:#fcfcfb;--panel:#f3f2ef;--text:#0b0b0b;--muted:#52514e;--grid:#dddcd6;--bad:#b42318;{lc}}}
@media (prefers-color-scheme: dark){{:root:not([data-theme="light"]){{color-scheme:dark;--bg:#1a1a19;--panel:#242422;--text:#f0efec;--muted:#c3c2b7;--grid:#3a3a37;--bad:#f97066;{dc}}}}}
:root[data-theme="dark"]{{color-scheme:dark;--bg:#1a1a19;--panel:#242422;--text:#f0efec;--muted:#c3c2b7;--grid:#3a3a37;--bad:#f97066;{dc}}}
body{{margin:0;background:var(--bg);color:var(--text);font:14px/1.45 system-ui,-apple-system,"Segoe UI",sans-serif}}
main{{max-width:1200px;margin:0 auto;padding:24px 16px 64px}}
h1{{font-size:22px;margin:0 0 4px}} h2{{font-size:17px;margin:36px 0 8px}} .muted{{color:var(--muted)}}
.scroll{{overflow-x:auto}} table{{border-collapse:collapse;font-variant-numeric:tabular-nums;width:100%}}
th,td{{padding:6px 10px;border-bottom:1px solid var(--grid);text-align:right;white-space:nowrap}} th:first-child,td:first-child{{text-align:left}}
th{{font-weight:600;color:var(--muted);font-size:12px}}
.sw{{display:inline-block;width:10px;height:10px;border-radius:2px;margin-right:6px;vertical-align:baseline}}
.grid{{display:grid;gap:6px;align-items:start}} .grid img{{width:100%;display:block;border-radius:4px}}
.cell{{font-size:12px;color:var(--muted)}} .rowlabel{{font-size:13px;font-weight:600;align-self:center}}
svg text{{fill:var(--muted);font-size:11px}} .err{{color:var(--bad)}}
.legend{{display:flex;gap:16px;flex-wrap:wrap;font-size:13px;margin:6px 0}}
</style></head><body><main>
<h1>xof-bench: {name}</h1>
<p class="muted">X-Plane {ver} · {spots} spots · slew {speed} kt · shots at {shots} s after arrival · north up, camera straight down, FOV {fov}°{cmp}</p>
"#,
        name = esc(&sc.name),
        ver = esc(&rec.xplane_version),
        spots = sc.spots.len(),
        speed = sc.slew_speed_kt,
        shots = sc.shots_s.iter().map(|s| format!("{s}")).collect::<Vec<_>>().join(", "),
        fov = sc.fov_deg,
        cmp = sc
            .compare_to
            .as_ref()
            .map(|c| format!(" · similarity = SSIM vs final <b>{}</b> shot", esc(c)))
            .unwrap_or_default(),
        lc = LIGHT.iter().enumerate().map(|(i, c)| format!("--s{i}:{c};")).collect::<String>(),
        dc = DARK.iter().enumerate().map(|(i, c)| format!("--s{i}:{c};")).collect::<String>(),
    );

    // Colour follows the variant name (not the run order).
    let mut names: Vec<String> = Vec::new();
    for r in &rec.runs {
        if !names.contains(&r.variant) {
            names.push(r.variant.clone());
        }
    }
    let color = |v: &str| format!("var(--s{})", names.iter().position(|n| n == v).unwrap_or(0) % 8);

    h.push_str("<h2>Summary</h2><div class=\"scroll\"><table><tr><th>Variant</th><th>Load s</th><th>Similarity at arrival</th><th>Final similarity</th><th>Time to sharp s</th><th>Slew fps</th><th>Slew stalls/min</th><th>Slew worst stall ms</th><th>Hold fps</th><th>DDS reads</th><th>Degraded bands</th><th>Upgrades</th><th>Reads &gt;1 s</th><th>Fetches</th></tr>");
    for (s, r) in summarize(rec).iter().zip(&rec.runs) {
        let e = s.engine.as_ref();
        let slow = e.map(|e| e.latency_buckets_ms.iter().filter(|(b, _)| *b > 1000).map(|(_, n)| n).sum::<u64>());
        let _ = write!(
            h,
            "<tr><td><span class=\"sw\" style=\"background:{c}\"></span>{l}{err}</td><td>{load:.0}</td><td>{s0}</td><td>{s1}</td><td>{ts}</td><td>{sf:.0}</td><td>{sst:.1}</td><td>{smax:.0}</td><td>{hf:.0}</td><td>{reads}</td><td>{deg}</td><td>{up}</td><td>{slow}</td><td>{fetch}</td></tr>",
            c = color(&r.variant),
            l = esc(&s.label),
            err = s.error.as_ref().map(|e| format!(" <span class=\"err\" title=\"{}\">failed</span>", esc(e))).unwrap_or_default(),
            load = s.load_s,
            s0 = f(s.sim_first, 3),
            s1 = f(s.sim_final, 3),
            ts = f(s.t_sharp, 1),
            sf = s.slew.fps_mean,
            sst = s.slew.stalls_per_min,
            smax = s.slew.stall_max_ms,
            hf = s.hold.fps_mean,
            reads = e.map(|e| e.reads.to_string()).unwrap_or("–".into()),
            deg = e.map(|e| e.degraded_built.to_string()).unwrap_or("–".into()),
            up = e.map(|e| e.upgrades.to_string()).unwrap_or("–".into()),
            slow = slow.map(|v| v.to_string()).unwrap_or("–".into()),
            fetch = e.map(|e| e.fetch_requests.to_string()).unwrap_or("–".into()),
        );
    }
    h.push_str("</table></div><p class=\"muted\">Similarity: SSIM of the centre 70 % (1 = identical). Time to sharp: first shot whose sharpness reaches 90 % of that run's final shot (mean over spots). Stalls: Web API round trips over 100 ms (X-Plane answers between frames).</p>");

    let use_sim = sc.compare_to.is_some();
    for (si, spot) in sc.spots.iter().enumerate() {
        let _ = write!(
            h,
            "<h2>{}. {} <span class=\"muted\">({:.4}, {:.4}, {} ft)</span></h2>",
            si + 1,
            esc(&spot.name),
            spot.lat,
            spot.lon,
            spot.alt_ft
        );
        // Chart.
        let series: Vec<(&VariantRun, Vec<(f64, f64)>)> = rec
            .runs
            .iter()
            .filter_map(|r| {
                let s = r.spots.iter().find(|s| s.spot == spot.name)?;
                let pts: Vec<(f64, f64)> = s
                    .shots
                    .iter()
                    .filter_map(|x| Some((x.t, if use_sim { x.similarity? } else { x.sharpness? })))
                    .collect();
                (!pts.is_empty()).then_some((r, pts))
            })
            .collect();
        if !series.is_empty() {
            h.push_str(&chart(&series, use_sim, multi, &color));
        }
        // Screenshot grid.
        let cols = sc.shots_s.len();
        let _ = write!(h, "<div class=\"scroll\"><div class=\"grid\" style=\"grid-template-columns:110px repeat({cols},minmax(150px,1fr));min-width:{}px\"><div></div>", 110 + cols * 156);
        for t in &sc.shots_s {
            let _ = write!(h, "<div class=\"cell\">t = {t} s</div>");
        }
        for r in &rec.runs {
            let _ = write!(
                h,
                "<div class=\"rowlabel\"><span class=\"sw\" style=\"background:{}\"></span>{}</div>",
                color(&r.variant),
                esc(&label(r, multi))
            );
            let s = r.spots.iter().find(|s| s.spot == spot.name);
            for i in 0..cols {
                match s.and_then(|s| s.shots.get(i)) {
                    Some(x) => {
                        let img = x.thumb.as_deref().unwrap_or(&x.file);
                        let tip = format!(
                            "{} at {:.1} s · sharpness {} · similarity {}",
                            label(r, multi),
                            x.t,
                            f(x.sharpness, 0),
                            f(x.similarity, 3)
                        );
                        let _ = write!(
                            h,
                            "<a href=\"{}\" title=\"{}\"><img loading=\"lazy\" src=\"{}\" alt=\"{}\"></a>",
                            esc(&x.file),
                            esc(&tip),
                            esc(img),
                            esc(&tip)
                        );
                    }
                    None => h.push_str("<div class=\"cell\">–</div>"),
                }
            }
        }
        h.push_str("</div></div>");
    }
    h.push_str("</main></body></html>\n");
    h
}

fn chart(
    series: &[(&VariantRun, Vec<(f64, f64)>)],
    use_sim: bool,
    multi: bool,
    color: &dyn Fn(&str) -> String,
) -> String {
    let (w, ht, l, r, t, b) = (640.0, 220.0, 48.0, 16.0, 12.0, 30.0);
    let tmax = series.iter().flat_map(|(_, p)| p.iter().map(|x| x.0)).fold(1.0, f64::max);
    let (ymin, ymax) = if use_sim {
        let lo = series.iter().flat_map(|(_, p)| p.iter().map(|x| x.1)).fold(1.0, f64::min);
        (((lo * 10.0).floor() / 10.0).min(0.9), 1.0)
    } else {
        (0.0, series.iter().flat_map(|(_, p)| p.iter().map(|x| x.1)).fold(1.0, f64::max) * 1.05)
    };
    let x = |v: f64| l + (v / tmax) * (w - l - r);
    let y = |v: f64| t + (1.0 - (v - ymin) / (ymax - ymin)) * (ht - t - b);
    let mut s = format!("<div class=\"legend\">{}</div><div class=\"scroll\"><svg viewBox=\"0 0 {w} {ht}\" width=\"100%\" style=\"max-width:{w}px\" role=\"img\" aria-label=\"{} over time per variant\">",
        series.iter().map(|(r, _)| format!("<span><span class=\"sw\" style=\"background:{}\"></span>{}</span>", color(&r.variant), esc(&label(r, multi)))).collect::<String>(),
        if use_sim { "Similarity" } else { "Sharpness" });
    for i in 0..=4 {
        let v = ymin + (ymax - ymin) * i as f64 / 4.0;
        let _ = write!(s, "<line x1=\"{l}\" x2=\"{}\" y1=\"{yy:.1}\" y2=\"{yy:.1}\" stroke=\"var(--grid)\" stroke-width=\"1\"/><text x=\"{}\" y=\"{:.1}\" text-anchor=\"end\">{}</text>", w - r, l - 6.0, y(v) + 4.0, if use_sim { format!("{v:.2}") } else { format!("{v:.0}") }, yy = y(v));
    }
    for tick in [0.0, tmax / 4.0, tmax / 2.0, 3.0 * tmax / 4.0, tmax] {
        let _ = write!(s, "<text x=\"{:.1}\" y=\"{}\" text-anchor=\"middle\">{:.0} s</text>", x(tick), ht - 8.0, tick);
    }
    for (run, pts) in series {
        let c = color(&run.variant);
        let path: String = pts
            .iter()
            .enumerate()
            .map(|(i, p)| format!("{}{:.1},{:.1}", if i == 0 { "M" } else { "L" }, x(p.0), y(p.1)))
            .collect();
        let _ =
            write!(s, "<path d=\"{path}\" fill=\"none\" stroke=\"{c}\" stroke-width=\"2\" stroke-linejoin=\"round\"/>");
        for p in pts {
            let _ = write!(
                s,
                "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"4\" fill=\"{c}\" stroke=\"var(--bg)\" stroke-width=\"2\"><title>{} · {:.1} s · {}</title></circle>",
                x(p.0),
                y(p.1),
                esc(&label(run, multi)),
                p.0,
                if use_sim { format!("similarity {:.3}", p.1) } else { format!("sharpness {:.0}", p.1) }
            );
        }
    }
    s.push_str("</svg></div>");
    s
}
