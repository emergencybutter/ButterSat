//! What a run records (`run.json`); metrics and the report are derived from it.

use serde::{Deserialize, Serialize};
use xof_proto::EngineStatsSnapshot;

use crate::scenario::Scenario;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRecord {
    pub scenario: Scenario,
    pub xplane_version: String,
    pub started_unix: u64,
    pub runs: Vec<VariantRun>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VariantRun {
    pub variant: String,
    /// 0-based repetition (persistent caches are warm from 1 on).
    pub repeat: u32,
    pub error: Option<String>,
    /// X-Plane launch to Web API up, and flight start to scenery loaded.
    pub launch_s: f64,
    pub load_s: f64,
    pub spots: Vec<SpotRun>,
    /// Frame-time samples over the whole run (t: seconds since flight start).
    pub frames: Vec<FrameSample>,
    pub daemon_start: Option<EngineStatsSnapshot>,
    pub daemon_end: Option<EngineStatsSnapshot>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SpotRun {
    pub spot: String,
    /// Slew window [start, arrival] and hold window [arrival, end], seconds since flight start.
    pub slew_start: f64,
    pub arrive: f64,
    pub end: f64,
    pub shots: Vec<Shot>,
    pub daemon_arrive: Option<EngineStatsSnapshot>,
    pub daemon_end: Option<EngineStatsSnapshot>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Shot {
    /// Seconds after arrival.
    pub t: f64,
    /// Path relative to the run folder (PNG until scored, then JPEG).
    pub file: String,
    #[serde(default)]
    pub thumb: Option<String>,
    #[serde(default)]
    pub sharpness: Option<f64>,
    /// SSIM against the comparison variant's final shot of the same spot.
    #[serde(default)]
    pub similarity: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct FrameSample {
    pub t: f64,
    /// `sim/time/framerate_period` (s); NaN when the request failed.
    pub period: f64,
    /// Round trip of the request (s): X-Plane answers between frames, so a
    /// long round trip is a stall even if the period is averaged.
    pub latency: f64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct FrameStats {
    pub samples: usize,
    pub fps_mean: f64,
    pub period_p99_ms: f64,
    pub stall_max_ms: f64,
    /// Request round trips over 100 ms per minute.
    pub stalls_per_min: f64,
}

pub fn frame_stats(frames: &[FrameSample], from: f64, to: f64) -> FrameStats {
    let w: Vec<&FrameSample> = frames.iter().filter(|f| f.t >= from && f.t <= to).collect();
    if w.is_empty() {
        return FrameStats::default();
    }
    let mut periods: Vec<f64> = w.iter().map(|f| f.period).filter(|p| p.is_finite() && *p > 0.0).collect();
    periods.sort_by(|a, b| a.total_cmp(b));
    let fps_mean = if periods.is_empty() { 0.0 } else { periods.len() as f64 / periods.iter().sum::<f64>() };
    let p99 = periods.get(((periods.len() as f64 * 0.99) as usize).min(periods.len().saturating_sub(1))).copied();
    let stalls = w.iter().filter(|f| f.latency > 0.1).count();
    let minutes = ((to - from) / 60.0).max(1.0 / 60.0);
    FrameStats {
        samples: w.len(),
        fps_mean,
        period_p99_ms: p99.unwrap_or(0.0) * 1000.0,
        stall_max_ms: w.iter().map(|f| f.latency).fold(0.0, f64::max) * 1000.0,
        stalls_per_min: stalls as f64 / minutes,
    }
}

/// Difference of two cumulative engine snapshots.
pub fn engine_delta(a: &EngineStatsSnapshot, b: &EngineStatsSnapshot) -> EngineStatsSnapshot {
    EngineStatsSnapshot {
        reads: b.reads.saturating_sub(a.reads),
        read_bytes: b.read_bytes.saturating_sub(a.read_bytes),
        segments_built: b.segments_built.saturating_sub(a.segments_built),
        degraded_built: b.degraded_built.saturating_sub(a.degraded_built),
        upgrades: b.upgrades.saturating_sub(a.upgrades),
        tiles_decoded: b.tiles_decoded.saturating_sub(a.tiles_decoded),
        latency_buckets_ms: b
            .latency_buckets_ms
            .iter()
            .map(|(bound, n)| {
                let before = a.latency_buckets_ms.iter().find(|(x, _)| x == bound).map(|(_, m)| *m).unwrap_or(0);
                (*bound, n.saturating_sub(before))
            })
            .collect(),
        fetch_requests: b.fetch_requests.saturating_sub(a.fetch_requests),
        fetch_bytes: b.fetch_bytes.saturating_sub(a.fetch_bytes),
        fetch_no_data: b.fetch_no_data.saturating_sub(a.fetch_no_data),
        fetch_errors: b.fetch_errors.saturating_sub(a.fetch_errors),
        segment_ram_bytes: b.segment_ram_bytes,
        decoded_ram_bytes: b.decoded_ram_bytes,
        reduced_zoom: b.reduced_zoom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_window() {
        let f: Vec<FrameSample> = (0..100)
            .map(|i| FrameSample { t: i as f64 * 0.1, period: 0.02, latency: if i == 50 { 0.5 } else { 0.01 } })
            .collect();
        let s = frame_stats(&f, 0.0, 9.9);
        assert_eq!(s.samples, 100);
        assert!((s.fps_mean - 50.0).abs() < 1e-6);
        assert!((s.stall_max_ms - 500.0).abs() < 1e-6);
        assert!(s.stalls_per_min > 5.0);
        assert_eq!(frame_stats(&f, 20.0, 30.0).samples, 0);
    }
}
