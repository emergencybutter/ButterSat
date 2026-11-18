//! Airport lookup in X-Plane's own `apt.dat` (for "prepare departure") and
//! SimBrief route fetching (route prefetch).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Airport {
    pub icao: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
}

/// `apt.dat` files to search, most specific first.
pub fn apt_dat_files(xp: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    let global = xp.join("Global Scenery/Global Airports/Earth nav data/apt.dat");
    if global.exists() {
        v.push(global);
    }
    v
}

/// Find an airport by ICAO/ident in an `apt.dat` stream. Uses the
/// `1302 datum_lat/datum_lon` metadata when present, otherwise the midpoint
/// of the first runway.
pub fn find_in<R: BufRead>(reader: R, icao: &str) -> Option<Airport> {
    let want = icao.trim().to_ascii_uppercase();
    let mut current: Option<(String, String)> = None;
    let (mut dlat, mut dlon, mut rwy): (Option<f64>, Option<f64>, Option<(f64, f64)>) = (None, None, None);
    let finish = |cur: &Option<(String, String)>, dlat: Option<f64>, dlon: Option<f64>, rwy: Option<(f64, f64)>| {
        let (id, name) = cur.clone()?;
        let (lat, lon) = match (dlat, dlon) {
            (Some(a), Some(b)) => (a, b),
            _ => rwy?,
        };
        Some(Airport { icao: id, name, lat, lon })
    };
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let mut it = line.split_whitespace();
        let Some(code) = it.next() else { continue };
        match code {
            "1" | "16" | "17" => {
                if current.is_some() {
                    return finish(&current, dlat, dlon, rwy);
                }
                let toks: Vec<&str> = it.collect();
                if toks.len() >= 4 && toks[3].eq_ignore_ascii_case(&want) {
                    current = Some((toks[3].to_string(), toks[4..].join(" ")));
                    (dlat, dlon, rwy) = (None, None, None);
                }
            }
            "1302" if current.is_some() => {
                let key = it.next().unwrap_or("");
                let val = it.next().and_then(|v| v.parse::<f64>().ok());
                match key {
                    "datum_lat" => dlat = val,
                    "datum_lon" => dlon = val,
                    _ => {}
                }
            }
            "100" if current.is_some() && rwy.is_none() => {
                let t: Vec<&str> = it.collect();
                // 100 width surf shoulder smooth centre edge auto | num lat lon ... | num lat lon ...
                if t.len() >= 17 {
                    let p = |i: usize| t.get(i).and_then(|v| v.parse::<f64>().ok());
                    if let (Some(a), Some(b), Some(c), Some(d)) = (p(8), p(9), p(17), p(18)) {
                        rwy = Some(((a + c) / 2.0, (b + d) / 2.0));
                    }
                }
            }
            "99" => break,
            _ => {}
        }
    }
    finish(&current, dlat, dlon, rwy)
}

/// Search the install's `apt.dat` files. Blocking.
pub fn find(xp: &Path, icao: &str) -> Option<Airport> {
    apt_dat_files(xp).into_iter().find_map(|f| {
        let file = std::fs::File::open(f).ok()?;
        find_in(BufReader::with_capacity(1 << 20, file), icao)
    })
}

/// The user's latest SimBrief flight plan, as far as ButterSat needs it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SimbriefPlan {
    /// `params.request_id`: changes with every generated plan.
    pub id: String,
    /// `params.time_generated` (unix seconds), 0 if absent.
    pub generated: i64,
    pub origin_icao: String,
    pub origin: Option<(f64, f64)>,
    pub destination_icao: String,
    /// Origin, navlog fixes, destination.
    pub points: Vec<(f64, f64)>,
}

/// Waypoints (lat, lon) of the user's latest SimBrief flight plan.
pub async fn simbrief_route(user: &str) -> anyhow::Result<Vec<(f64, f64)>> {
    Ok(simbrief_latest(user).await?.points)
}

/// The user's latest SimBrief plan (Navigraph `xml.fetcher.php`, JSON).
///
/// Load on SimBrief stays small (Navigraph developer terms §5.5: no undue
/// burden on their APIs): identified via `User-Agent` and the `client`
/// parameter; one response cached per user for 60 s; called on user action
/// ("Prefetch SimBrief route") and by the departure watcher at most every
/// 5 minutes, never while X-Plane is running.
pub async fn simbrief_latest(user: &str) -> anyhow::Result<SimbriefPlan> {
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    const CLIENT_ID: &str = concat!("ButterSat/", env!("CARGO_PKG_VERSION"));
    const CACHE_TTL: Duration = Duration::from_secs(60);

    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, SimbriefPlan)>>> = OnceLock::new();
    static HTTP: OnceLock<reqwest::Client> = OnceLock::new();

    let user = user.trim();
    anyhow::ensure!(!user.is_empty(), "SimBrief username or pilot ID is empty");
    let norm_user = user.to_ascii_lowercase();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((ts, plan)) = cache.lock().get(&norm_user) {
        if ts.elapsed() < CACHE_TTL {
            return Ok(plan.clone());
        }
    }

    let client = HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(CLIENT_ID)
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    });

    let key = if user.chars().all(|c| c.is_ascii_digit()) { "userid" } else { "username" };
    let url = "https://www.simbrief.com/api/xml.fetcher.php";
    let v: serde_json::Value = client
        .get(url)
        .query(&[(key, user), ("json", "1"), ("client", CLIENT_ID)])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let plan = parse_plan(&v);
    cache.lock().insert(norm_user, (Instant::now(), plan.clone()));
    Ok(plan)
}

pub fn parse_plan(v: &serde_json::Value) -> SimbriefPlan {
    let num = |x: &serde_json::Value| x.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| x.as_f64());
    let text = |x: &serde_json::Value| match x {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    let origin = match (num(&v["origin"]["pos_lat"]), num(&v["origin"]["pos_long"])) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => None,
    };
    SimbriefPlan {
        id: text(&v["params"]["request_id"]),
        generated: num(&v["params"]["time_generated"]).unwrap_or(0.0) as i64,
        origin_icao: text(&v["origin"]["icao_code"]),
        origin,
        destination_icao: text(&v["destination"]["icao_code"]),
        points: parse_simbrief(v),
    }
}

pub fn parse_simbrief(v: &serde_json::Value) -> Vec<(f64, f64)> {
    let num = |x: &serde_json::Value| x.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| x.as_f64());
    let mut pts = Vec::new();
    let mut push = |o: &serde_json::Value| {
        if let (Some(a), Some(b)) = (num(&o["pos_lat"]), num(&o["pos_long"])) {
            pts.push((a, b));
        }
    };
    push(&v["origin"]);
    match &v["navlog"]["fix"] {
        serde_json::Value::Array(a) => a.iter().for_each(&mut push),
        o @ serde_json::Value::Object(_) => push(o),
        _ => {}
    }
    push(&v["destination"]);
    pts
}

#[cfg(test)]
mod tests {
    use super::*;

    const APT: &str = "I\n1200 Generated\n\n1    13 0 0 KJFK John F Kennedy Intl\n1302 city New York\n1302 datum_lat 40.639447\n1302 datum_lon -73.779317\n100 61.0 1 0 0.25 1 3 0 04L 40.62203 -73.78553 0 0 3 0 0 0 22R 40.64592 -73.75834 0 0 3 0 0 0\n1    21 0 0 KLGA La Guardia\n100 45.72 1 0 0.25 1 3 0 04 40.76922 -73.88405 0 0 3 0 0 0 22 40.78479 -73.86808 0 0 3 0 0 0\n99\n";

    #[test]
    fn datum_and_runway_fallback() {
        let a = find_in(APT.as_bytes(), "kjfk").unwrap();
        assert_eq!(a.name, "John F Kennedy Intl");
        assert!((a.lat - 40.639447).abs() < 1e-9 && (a.lon + 73.779317).abs() < 1e-9);
        let b = find_in(APT.as_bytes(), "KLGA").unwrap();
        assert!((b.lat - 40.777).abs() < 0.01 && (b.lon + 73.876).abs() < 0.01, "{b:?}");
        assert!(find_in(APT.as_bytes(), "EGLL").is_none());
    }

    #[test]
    fn simbrief_parse() {
        let v = serde_json::json!({
            "origin": {"pos_lat": "40.639", "pos_long": "-73.779"},
            "navlog": {"fix": [{"pos_lat": "41.0", "pos_long": "-73.0"}, {"pos_lat": "42.0", "pos_long": "-72.0"}]},
            "destination": {"pos_lat": "42.36", "pos_long": "-71.0"}
        });
        assert_eq!(parse_simbrief(&v).len(), 4);
    }

    #[test]
    fn simbrief_plan_identity_and_origin() {
        let v = serde_json::json!({
            "params": {"request_id": "123456789", "time_generated": "1795010000"},
            "origin": {"icao_code": "EGLL", "pos_lat": "51.4706", "pos_long": "-0.461941"},
            "navlog": {"fix": {"pos_lat": "51.0", "pos_long": "-1.0"}},
            "destination": {"icao_code": "KJFK", "pos_lat": "40.639", "pos_long": "-73.779"}
        });
        let p = parse_plan(&v);
        assert_eq!(p.id, "123456789");
        assert_eq!(p.generated, 1_795_010_000);
        assert_eq!((p.origin_icao.as_str(), p.destination_icao.as_str()), ("EGLL", "KJFK"));
        assert!((p.origin.unwrap().0 - 51.4706).abs() < 1e-9);
        assert_eq!(p.points.len(), 3);
        assert_eq!(parse_plan(&serde_json::json!({})).origin, None);
    }
}
