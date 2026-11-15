//! X-Plane 12 control through the local Web API (v3, X-Plane 12.4+), plus
//! process launch/kill.
//!
//! Facts verified against 12.4.4:
//! - datarefs exist only once a flight is loaded (count is 0 at the menu);
//! - `POST /api/v3/flight` starts a flight anywhere; weather presets do not
//!   include a clear sky, so a custom definition is used;
//! - `sim/operation/quit` opens a confirmation dialog, so we kill instead;
//! - use 127.0.0.1, not localhost (IPv6 fallback costs ~200 ms per request).

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{json, Value};

pub struct Xp {
    http: reqwest::Client,
    root: String,
    ids: Mutex<HashMap<String, u64>>,
    cmds: Mutex<HashMap<String, u64>>,
}

impl Xp {
    pub fn new(port: u16) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("http client");
        Self {
            http,
            root: format!("http://127.0.0.1:{port}/api"),
            ids: Mutex::new(HashMap::new()),
            cmds: Mutex::new(HashMap::new()),
        }
    }

    /// Forget resolved ids (after a restart).
    pub fn reset(&self) {
        self.ids.lock().clear();
        self.cmds.lock().clear();
    }

    async fn json(&self, rb: reqwest::RequestBuilder) -> anyhow::Result<Value> {
        let r = rb.header("Accept", "application/json").send().await?;
        let status = r.status();
        let text = r.text().await?;
        let v: Value = if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text)? };
        if let Some(code) = v.get("error_code").and_then(|c| c.as_str()) {
            if code != "success" {
                anyhow::bail!("{code}: {}", v["error_message"].as_str().unwrap_or(""));
            }
        }
        anyhow::ensure!(status.is_success(), "HTTP {status}: {text}");
        Ok(v)
    }

    pub async fn alive(&self) -> bool {
        let rb = self.http.get(format!("{}/capabilities", self.root)).timeout(Duration::from_secs(2));
        self.json(rb).await.is_ok()
    }

    pub async fn version(&self) -> anyhow::Result<String> {
        let v = self.json(self.http.get(format!("{}/capabilities", self.root))).await?;
        Ok(v["x-plane"]["version"].as_str().unwrap_or("?").to_string())
    }

    pub async fn dataref_count(&self) -> anyhow::Result<u64> {
        let v = self.json(self.http.get(format!("{}/v3/datarefs/count", self.root))).await?;
        Ok(v["data"].as_u64().unwrap_or(0))
    }

    async fn lookup(&self, kind: &str, name: &str) -> anyhow::Result<u64> {
        let cache = if kind == "datarefs" { &self.ids } else { &self.cmds };
        if let Some(id) = cache.lock().get(name) {
            return Ok(*id);
        }
        let v = self.json(self.http.get(format!("{}/v3/{kind}", self.root)).query(&[("filter[name]", name)])).await?;
        let id = v["data"]
            .as_array()
            .and_then(|a| a.iter().find(|d| d["name"] == name))
            .and_then(|d| d["id"].as_u64())
            .ok_or_else(|| anyhow::anyhow!("{name} not found"))?;
        cache.lock().insert(name.to_string(), id);
        Ok(id)
    }

    pub async fn get(&self, name: &str) -> anyhow::Result<Value> {
        let id = self.lookup("datarefs", name).await?;
        let v = self.json(self.http.get(format!("{}/v3/datarefs/{id}/value", self.root))).await?;
        Ok(v["data"].clone())
    }

    pub async fn get_f64(&self, name: &str) -> anyhow::Result<f64> {
        self.get(name).await?.as_f64().ok_or_else(|| anyhow::anyhow!("{name} is not a number"))
    }

    pub async fn set(&self, name: &str, value: Value) -> anyhow::Result<()> {
        let id = self.lookup("datarefs", name).await?;
        let rb = self.http.patch(format!("{}/v3/datarefs/{id}/value", self.root)).json(&json!({ "data": value }));
        self.json(rb).await.map(|_| ())
    }

    pub async fn set_index(&self, name: &str, index: usize, value: Value) -> anyhow::Result<()> {
        let id = self.lookup("datarefs", name).await?;
        let rb = self
            .http
            .patch(format!("{}/v3/datarefs/{id}/value", self.root))
            .query(&[("index", index)])
            .json(&json!({ "data": value }));
        self.json(rb).await.map(|_| ())
    }

    pub async fn command(&self, name: &str) -> anyhow::Result<()> {
        let id = self.lookup("commands", name).await?;
        let rb = self.http.post(format!("{}/v3/command/{id}/activate", self.root)).json(&json!({ "duration": 0 }));
        self.json(rb).await.map(|_| ())
    }

    /// Start a flight. X-Plane loads it while the request is open and often
    /// drops the connection: a transport error means "probably started"
    /// (the caller waits for datarefs); an API error is real.
    pub async fn start_flight(&self, init: &Value) -> anyhow::Result<()> {
        let rb = self
            .http
            .post(format!("{}/v3/flight", self.root))
            .timeout(Duration::from_secs(300))
            .json(&json!({ "data": init }));
        match self.json(rb).await {
            Ok(_) => Ok(()),
            Err(e) if e.downcast_ref::<reqwest::Error>().is_some() => {
                println!("   (flight request: {e}; X-Plane drops it while loading, continuing)");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub async fn patch_flight(&self, patch: &Value) -> anyhow::Result<()> {
        let rb = self.http.patch(format!("{}/v3/flight", self.root)).json(&json!({ "data": patch }));
        self.json(rb).await.map(|_| ())
    }
}

/// Flight init for a frozen-able air start in clear, calm weather.
pub fn flight_init(aircraft: &str, lat: f64, lon: f64, elev_m: f64, heading: f64, day: u32, hour: f64) -> Value {
    json!({
        "aircraft": { "path": aircraft },
        "lle_air_start": {
            "latitude": lat, "longitude": lon, "elevation_in_meters": elev_m,
            "heading_true": heading, "speed_in_meters_per_second": 60.0
        },
        "local_time": { "day_of_year": day, "time_in_24_hours": hour },
        "weather": {
            "definition": {
                "latitude_in_degrees": lat, "longitude_in_degrees": lon, "elevation_in_meters": 0.0,
                "visibility_in_kilometers": 80.0, "clouds": [],
                "wind": [{ "altitude_in_feet_msl": 0.0, "speed_in_knots": 0.0, "direction_in_degrees_true": 0.0 }]
            },
            "vertical_speed_in_thermal_in_feet_per_minute": 0.0,
            "wave_height_in_meters": 0.0, "wave_direction_in_degrees": 0.0,
            "terrain_state": "dry", "variation_across_region_percentage": 0.0,
            "evolution_over_time_enum": "static"
        }
    })
}

pub fn exe_path(install: &Path) -> std::path::PathBuf {
    if cfg!(windows) {
        install.join("X-Plane.exe")
    } else if cfg!(target_os = "macos") {
        install.join("X-Plane.app/Contents/MacOS/X-Plane")
    } else {
        install.join("X-Plane-x86_64")
    }
}

pub fn launch(install: &Path) -> anyhow::Result<()> {
    let exe = exe_path(install);
    anyhow::ensure!(exe.exists(), "{} not found", exe.display());
    std::process::Command::new(&exe)
        .current_dir(install)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

pub fn process_running() -> bool {
    if cfg!(windows) {
        std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq X-Plane.exe", "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase().contains("x-plane.exe"))
            .unwrap_or(false)
    } else {
        std::process::Command::new("pgrep").args(["-f", "X-Plane"]).status().map(|s| s.success()).unwrap_or(false)
    }
}

pub fn kill() {
    let _ = if cfg!(windows) {
        std::process::Command::new("taskkill").args(["/IM", "X-Plane.exe", "/F"]).output()
    } else {
        std::process::Command::new("pkill").args(["-9", "-f", "X-Plane"]).output()
    };
}

/// Make sure no X-Plane is running (killing it if needed).
pub async fn ensure_stopped() -> anyhow::Result<()> {
    if !process_running() {
        return Ok(());
    }
    kill();
    let t = Instant::now();
    while process_running() {
        anyhow::ensure!(t.elapsed() < Duration::from_secs(60), "X-Plane did not exit");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // Let the driver release files and the GPU.
    tokio::time::sleep(Duration::from_secs(3)).await;
    Ok(())
}

pub async fn wait_until<F, Fut>(what: &str, timeout: Duration, mut f: F) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let t = Instant::now();
    loop {
        if f().await {
            return Ok(());
        }
        anyhow::ensure!(t.elapsed() < timeout, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
