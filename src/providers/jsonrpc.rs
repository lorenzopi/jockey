//! Minimal line-delimited JSON-RPC 2.0 client, shared by the Codex app-server
//! and (later) ACP adapters. Transport-agnostic so tests can use in-memory pipes.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    /// Server-to-client request; must be answered with `respond`/`respond_error`.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

pub struct RpcClient {
    out: mpsc::Sender<String>,
    pending: Pending,
    next: AtomicU64,
}

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

impl RpcClient {
    pub fn start<R, W>(reader: R, writer: W) -> (Arc<Self>, mpsc::Receiver<Incoming>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
        let (in_tx, in_rx) = mpsc::channel(1024);
        let pending: Pending = Arc::default();

        tokio::spawn(async move {
            let mut w = writer;
            while let Some(line) = out_rx.recv().await {
                if w.write_all(line.as_bytes()).await.is_err()
                    || w.write_all(b"\n").await.is_err()
                    || w.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let pend = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue; // non-JSON noise on stdout
                };
                let method = msg.get("method").and_then(Value::as_str);
                match (msg.get("id"), method) {
                    (Some(id), Some(m)) => {
                        let req = Incoming::Request {
                            id: id.clone(),
                            method: m.into(),
                            params: msg.get("params").cloned().unwrap_or(Value::Null),
                        };
                        if in_tx.send(req).await.is_err() {
                            break;
                        }
                    }
                    (Some(id), None) => {
                        let Some(id) = id.as_u64() else { continue };
                        let Some(tx) = pend.lock().unwrap().remove(&id) else {
                            continue;
                        };
                        let res = match msg.get("error") {
                            Some(e) => Err(anyhow!(
                                "{}",
                                e.get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("rpc error")
                            )),
                            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(res);
                    }
                    (None, Some(m)) => {
                        let n = Incoming::Notification {
                            method: m.into(),
                            params: msg.get("params").cloned().unwrap_or(Value::Null),
                        };
                        if in_tx.send(n).await.is_err() {
                            break;
                        }
                    }
                    (None, None) => {}
                }
            }
            // EOF: fail everything still waiting so callers don't hang.
            for (_, tx) in pend.lock().unwrap().drain() {
                let _ = tx.send(Err(anyhow!("provider process closed the connection")));
            }
        });

        let client = Arc::new(Self {
            out: out_tx,
            pending,
            next: AtomicU64::new(1),
        });
        (client, in_rx)
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_timeout(method, params, Some(REQUEST_TIMEOUT))
            .await
    }

    /// `None` waits indefinitely (e.g. ACP `session/prompt`, which lasts a whole turn).
    pub async fn request_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.out
            .send(msg.to_string())
            .await
            .map_err(|_| anyhow!("provider process is gone"))?;
        let wait = async {
            match timeout {
                Some(t) => tokio::time::timeout(t, rx).await,
                None => Ok(rx.await),
            }
        };
        match wait.await {
            Ok(Ok(res)) => res.map_err(|e| anyhow!("{method}: {e}")),
            Ok(Err(_)) => Err(anyhow!("{method}: connection closed")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(anyhow!("{method}: timed out"))
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        let mut msg = json!({"jsonrpc": "2.0", "method": method});
        if let Some(p) = params {
            msg["params"] = p;
        }
        self.send(msg).await
    }

    pub async fn respond(&self, id: Value, result: Value) -> Result<()> {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await
    }

    pub async fn respond_error(&self, id: Value, message: &str) -> Result<()> {
        self.send(
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": message}}),
        )
        .await
    }

    async fn send(&self, msg: Value) -> Result<()> {
        self.out
            .send(msg.to_string())
            .await
            .map_err(|_| anyhow!("provider process is gone"))
    }
}
