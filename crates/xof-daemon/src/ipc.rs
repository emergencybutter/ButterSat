//! JSON-RPC server over the per-user local socket (design §12).

use std::sync::Arc;

use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::ListenerOptions;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use xof_proto::{method, RpcNotification, RpcRequest, RpcResponse, JSONRPC};

use crate::app::App;

pub async fn serve(app: Arc<App>) -> anyhow::Result<()> {
    if !cfg!(windows) {
        if let Some(dir) = xof_proto::socket_path().parent() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let listener = ListenerOptions::new().name(xof_proto::socket_name()?).try_overwrite(true).create_tokio()?;
    tracing::info!("IPC listening");
    loop {
        let conn = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("IPC accept: {e}");
                continue;
            }
        };
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(app, conn).await {
                tracing::debug!("IPC connection ended: {e}");
            }
        });
    }
}

async fn send_line<W: AsyncWriteExt + Unpin>(w: &Mutex<W>, v: &impl serde::Serialize) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(v).map_err(std::io::Error::other)?;
    line.push(b'\n');
    let mut w = w.lock().await;
    w.write_all(&line).await?;
    w.flush().await
}

async fn handle_conn(app: Arc<App>, conn: interprocess::local_socket::tokio::Stream) -> anyhow::Result<()> {
    let (r, w) = tokio::io::split(conn);
    let w = Arc::new(Mutex::new(w));
    let mut lines = BufReader::new(r).lines();
    let mut subscribed = false;
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: RpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                send_line(&w, &RpcResponse::err(None, -32700, format!("parse error: {e}"))).await?;
                continue;
            }
        };
        if req.method == method::SUBSCRIBE {
            if !subscribed {
                subscribed = true;
                let mut rx = app.events.subscribe();
                let w2 = w.clone();
                tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(ev) => {
                                let n = RpcNotification {
                                    jsonrpc: JSONRPC.into(),
                                    method: method::EVENT.into(),
                                    params: ev,
                                };
                                if send_line(&w2, &n).await.is_err() {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => break,
                        }
                    }
                });
            }
            send_line(&w, &RpcResponse::ok(req.id, Value::Bool(true))).await?;
            continue;
        }
        // Handle concurrently so a slow call (mount, import) doesn't block others.
        let (app, w) = (app.clone(), w.clone());
        tokio::spawn(async move {
            let resp = match app.handle(&req.method, req.params).await {
                Ok(v) => RpcResponse::ok(req.id, v),
                Err((code, msg)) => RpcResponse::err(req.id, code, msg),
            };
            if req.id.is_some() {
                let _ = send_line(&w, &resp).await;
            }
        });
    }
    Ok(())
}
