//! The xporthofs daemon under test: an isolated instance (own `XOF_HOME`)
//! with a generated config, plus a throwaway HTTP origin for a local catalog.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use xof_proto::{method, Client, EngineStatsSnapshot, Status};

pub struct Daemon {
    child: Child,
    client: Client,
}

/// Is some xporthofs daemon already listening on this user's socket?
pub async fn already_running() -> bool {
    tokio::time::timeout(Duration::from_secs(2), Client::connect()).await.map(|r| r.is_ok()).unwrap_or(false)
}

pub fn daemon_exe(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let me = std::env::current_exe()?;
    let p = me.with_file_name(format!("xof-daemon{}", std::env::consts::EXE_SUFFIX));
    anyhow::ensure!(p.exists(), "{} not found (build xof-daemon, or pass --daemon)", p.display());
    Ok(p)
}

/// Generated config.toml: the bench owns scenery_packs.ini, so the daemon
/// must not touch it; `overrides` are merged on top.
pub fn config_toml(
    install: &Path,
    port: u16,
    mirror: Option<&str>,
    keys: &[String],
    overrides: &toml::Table,
) -> anyhow::Result<String> {
    let mut t: toml::Table = toml::from_str(&format!(
        "[xplane]\npath = {path}\nweb_api_port = {port}\n\
         [mount]\nauto_mount = true\nmanage_scenery_packs = false\n\
         [imagery]\nprovider_notice_accepted = true\n\
         [startup]\n",
        path = toml::Value::String(install.display().to_string()),
    ))?;
    let mesh = t.entry("mesh").or_insert_with(|| toml::Value::Table(Default::default()));
    if let toml::Value::Table(m) = mesh {
        if let Some(url) = mirror {
            m.insert("mirrors".into(), toml::Value::Array(vec![toml::Value::String(url.into())]));
        }
        if !keys.is_empty() {
            m.insert("public_keys".into(), toml::Value::Array(keys.iter().cloned().map(toml::Value::String).collect()));
        }
    }
    merge(&mut t, overrides);
    Ok(toml::to_string(&t)?)
}

fn merge(dst: &mut toml::Table, src: &toml::Table) {
    for (k, v) in src {
        match (dst.get_mut(k), v) {
            (Some(toml::Value::Table(d)), toml::Value::Table(s)) => merge(d, s),
            _ => {
                dst.insert(k.clone(), v.clone());
            }
        }
    }
}

impl Daemon {
    pub async fn start(exe: &Path, home: &Path, config: &str) -> anyhow::Result<Self> {
        std::fs::create_dir_all(home)?;
        std::fs::write(home.join("config.toml"), config)?;
        let log = std::fs::File::create(home.join("daemon.log"))?;
        let child = Command::new(exe)
            .env("XOF_HOME", home)
            .env("RUST_LOG", "info")
            .env("XOF_READ_TRACE", home.join("reads.trace"))
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        let t = Instant::now();
        let client = loop {
            if let Ok(Ok(c)) = tokio::time::timeout(Duration::from_secs(2), Client::connect()).await {
                break c;
            }
            anyhow::ensure!(t.elapsed() < Duration::from_secs(30), "daemon did not start (see {})", home.display());
            tokio::time::sleep(Duration::from_millis(300)).await;
        };
        let mut d = Self { child, client };
        loop {
            let s = d.status().await?;
            if s.mounted {
                break;
            }
            if let Some(e) = s.mount_error {
                d.stop().await;
                anyhow::bail!("daemon mount failed: {e}");
            }
            anyhow::ensure!(t.elapsed() < Duration::from_secs(60), "daemon did not mount");
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Ok(d)
    }

    pub async fn status(&self) -> anyhow::Result<Status> {
        self.client.call(method::STATUS, Value::Null).await
    }

    pub async fn prepare(&self, lat: f64, lon: f64) -> anyhow::Result<()> {
        let _: Value = self
            .client
            .call(method::PREPARE_DEPARTURE, xof_proto::PrepareDeparture { icao: None, lat: Some(lat), lon: Some(lon) })
            .await?;
        Ok(())
    }

    pub async fn engine(&self) -> Option<EngineStatsSnapshot> {
        self.status().await.ok().map(|s| s.engine)
    }

    pub async fn stop(&mut self) {
        let _ =
            tokio::time::timeout(Duration::from_secs(5), self.client.call::<_, Value>(method::SHUTDOWN, Value::Null))
                .await;
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(30) {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Minimal static HTTP/1.1 server (GET only) for a local catalog origin.
pub struct Origin {
    pub url: String,
}

pub fn serve(root: &Path) -> anyhow::Result<Origin> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let root = root.clone();
            std::thread::spawn(move || {
                let _ = handle(stream, &root);
            });
        }
    });
    Ok(Origin { url: format!("http://127.0.0.1:{port}/v1") })
}

fn handle(mut stream: TcpStream, root: &Path) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
    }
    let mut parts = line.split_whitespace();
    let (m, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    let path = target.split('?').next().unwrap_or("/").trim_start_matches('/');
    let safe = !path.split('/').any(|c| c == ".." || c.contains('\\') || c.contains(':'));
    let file = root.join(path);
    match (m, safe, std::fs::File::open(&file)) {
        ("GET" | "HEAD", true, Ok(mut f)) if file.is_file() => {
            let mut body = Vec::new();
            if m == "GET" {
                f.read_to_end(&mut body)?;
            }
            let len = std::fs::metadata(&file)?.len();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n")?;
            stream.write_all(&body)?;
        }
        _ => {
            stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
        }
    }
    stream.flush()?;
    // Graceful close: closing with unread input makes Windows send RST,
    // which can destroy the response before the client reads it.
    stream.shutdown(std::net::Shutdown::Write)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut sink = [0u8; 1024];
    while matches!(reader.read(&mut sink), Ok(n) if n > 0) {}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_merges_overrides() {
        let o: toml::Table = toml::from_str("[imagery]\ndeadline_ms = 500\n[prefetch]\nenabled = false\n").unwrap();
        let s = config_toml(Path::new("C:/XP"), 8086, Some("http://x/v1"), &["ab".into()], &o).unwrap();
        let cfg: xof_proto::config::DaemonConfig = toml::from_str(&s).unwrap();
        assert_eq!(cfg.imagery.deadline_ms, 500);
        assert!(cfg.imagery.provider_notice_accepted);
        assert!(!cfg.prefetch.enabled);
        assert!(!cfg.mount.manage_scenery_packs);
        assert_eq!(cfg.mesh.mirrors, vec!["http://x/v1".to_string()]);
        assert_eq!(cfg.mesh.public_keys, vec!["ab".to_string()]);
    }

    #[test]
    fn origin_serves_files_and_refuses_traversal() {
        let dir = std::env::temp_dir().join(format!("xofbench-origin-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("v1")).unwrap();
        std::fs::write(dir.join("v1/a.txt"), b"hello").unwrap();
        let o = serve(&dir).unwrap();
        let get = |p: &str| {
            let addr = o.url.trim_start_matches("http://").trim_end_matches("/v1").to_string();
            let mut s = TcpStream::connect(addr).unwrap();
            write!(s, "GET {p} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut r = String::new();
            s.read_to_string(&mut r).unwrap();
            r
        };
        let ok = get("/v1/a.txt");
        assert!(ok.starts_with("HTTP/1.1 200") && ok.ends_with("hello"));
        assert!(get("/v1/../v1/a.txt").starts_with("HTTP/1.1 404"));
        assert!(get("/v1/missing").starts_with("HTTP/1.1 404"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
