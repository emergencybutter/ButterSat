//! Partial mirrors: one signed manifest, objects split across hosts (the US on
//! selfhost, the rest of the world on R2). A 404 means "not on this mirror".

use std::path::PathBuf;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use xof_mesh::catalog::{CatalogClient, Object};

/// Minimal static file server rooted at `root`; returns the base URL.
async fn serve(root: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let root = root.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("/").trim_start_matches("/v1/").to_string();
                let resp = match std::fs::read(root.join(&path)) {
                    Ok(body) => {
                        let mut r =
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
                                .into_bytes();
                        r.extend_from_slice(&body);
                        r
                    }
                    Err(_) => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                };
                let _ = s.write_all(&resp).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

fn object(name: &str, data: &[u8]) -> Object {
    Object { object: name.into(), size: data.len() as u64, blake3: blake3::hash(data).to_hex().to_string() }
}

#[tokio::test]
async fn object_missing_on_first_mirror_comes_from_the_next() {
    let base = std::env::temp_dir().join(format!("xof-partial-{}", std::process::id()));
    let (us, world, cache) = (base.join("us"), base.join("world"), base.join("cache"));
    for d in [&us, &world, &cache] {
        std::fs::create_dir_all(d.join("tiles")).unwrap();
    }
    let paris = b"paris dsf".to_vec();
    let nyc = b"new york dsf".to_vec();
    std::fs::write(world.join("tiles/paris.dsf.zst"), &paris).unwrap();
    std::fs::write(us.join("tiles/nyc.dsf.zst"), &nyc).unwrap();
    let (us_url, world_url) = (serve(us.clone()).await, serve(world.clone()).await);
    let client = CatalogClient::new(vec![], &cache).unwrap();

    // World-first order (as shipped): US objects fall through to selfhost.
    let mirrors = vec![world_url.clone(), us_url.clone()];
    assert_eq!(client.fetch_object(&mirrors, &object("tiles/nyc.dsf.zst", &nyc)).await.unwrap(), nyc);
    assert_eq!(client.fetch_object(&mirrors, &object("tiles/paris.dsf.zst", &paris)).await.unwrap(), paris);
    // And the other way round.
    let mirrors = vec![us_url.clone(), world_url.clone()];
    assert_eq!(client.fetch_object(&mirrors, &object("tiles/paris.dsf.zst", &paris)).await.unwrap(), paris);

    // Missing everywhere: an error, not a hang.
    let err = client.fetch_object(&mirrors, &object("tiles/nowhere.dsf.zst", b"x")).await.unwrap_err();
    assert!(err.to_string().contains("not found on any mirror"), "{err}");

    // A corrupted copy on the first mirror is still rejected, then served by the next.
    std::fs::write(us.join("tiles/paris.dsf.zst"), b"tampered").unwrap();
    assert_eq!(client.fetch_object(&mirrors, &object("tiles/paris.dsf.zst", &paris)).await.unwrap(), paris);

    let _ = std::fs::remove_dir_all(base);
}
