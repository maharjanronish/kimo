//! Minimal, dependency-light Chrome DevTools Protocol client over WebSocket.
//!
//! One connection to the browser-level DevTools endpoint. Commands carry an
//! optional flattened `sessionId` so the same socket serves both browser-level
//! and per-target commands. Events are broadcast to subscribers.

use crate::error::{KimoError, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const EVENT_BUFFER: usize = 4096;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Cdp {
    write: AsyncMutex<futures_util::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<std::result::Result<Value, String>>>>,
    events: broadcast::Sender<Value>,
}

impl Cdp {
    /// Connect to the browser DevTools WebSocket endpoint.
    pub async fn connect(ws_url: &str) -> Result<Arc<Self>> {
        let (ws, _resp) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| KimoError::Cdp(format!("failed to connect to DevTools WebSocket at {ws_url}: {e}")))?;
        let (write, mut read) = ws.split();

        let (events_tx, _events_rx) = broadcast::channel::<Value>(EVENT_BUFFER);
        let pending: Mutex<HashMap<i64, oneshot::Sender<std::result::Result<Value, String>>>> =
            Mutex::new(HashMap::new());

        let cdp = Arc::new(Cdp {
            write: AsyncMutex::new(write),
            next_id: AtomicI64::new(1),
            pending,
            events: events_tx,
        });

        // Reader task: dispatch command responses and events. Holds a weak
        // reference so the task ends with the connection, not vice versa.
        let reader_cdp = Arc::downgrade(&cdp);
        tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                let text = match msg {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Close(_)) => break,
                    Ok(_) => continue,
                    Err(_) => break,
                };
                let cdp = match reader_cdp.upgrade() {
                    Some(c) => c,
                    None => break,
                };
                let v: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v.get("id").is_some() {
                    let id = v["id"].as_i64().unwrap_or(0);
                    if let Some(tx) = cdp.pending.lock().unwrap().remove(&id) {
                        if let Some(err) = v.get("error") {
                            let msg = err
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("unknown CDP error")
                                .to_string();
                            let _ = tx.send(Err(msg));
                        } else if let Some(res) = v.get("result") {
                            let _ = tx.send(Ok(res.clone()));
                        } else {
                            let _ = tx.send(Err("CDP response had neither result nor error".into()));
                        }
                    }
                } else if v.get("method").is_some() {
                    let _ = cdp.events.send(v);
                }
            }
            // Socket closed: fail all still-pending commands so callers wake up.
            if let Some(cdp) = reader_cdp.upgrade() {
                cdp.pending.lock().unwrap().clear();
            }
        });

        Ok(cdp)
    }

    /// Send a CDP command and await its response.
    pub async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut envelope = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            envelope["sessionId"] = json!(s);
        }
        let payload = serde_json::to_string(&envelope)
            .map_err(|e| KimoError::Cdp(format!("failed to serialize CDP command {method}: {e}")))?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        {
            let mut w = self.write.lock().await;
            w.send(Message::Text(payload))
                .await
                .map_err(|e| {
                    self.pending.lock().unwrap().remove(&id);
                    KimoError::Cdp(format!("failed to send CDP command {method}: {e}"))
                })?;
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => match result {
                Ok(v) => Ok(v),
                Err(msg) => Err(KimoError::Cdp(format!("{method} failed: {msg}"))),
            },
            Ok(Err(_recv_err)) => Err(KimoError::Cdp(format!(
                "CDP connection closed before {method} answered (browser likely exited or crashed)"
            ))),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(KimoError::Cdp(format!(
                    "timed out after {}s waiting for CDP response to {method}",
                    timeout.as_secs()
                )))
            }
        }
    }

    /// Convenience wrapper with the default timeout.
    pub async fn cmd(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        self.call(session, method, params, DEFAULT_TIMEOUT).await
    }

    /// Subscribe to CDP events.
    pub fn events(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    /// Wait for the next event whose method matches (and session, if given).
    /// Unrelated events are skipped.
    pub async fn wait_event(
        &self,
        rx: &mut broadcast::Receiver<Value>,
        method: &str,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(KimoError::Cdp(format!(
                    "timed out waiting for CDP event {method}"
                )));
            }
            let v = tokio::select! {
                r = rx.recv() => match r {
                    Ok(v) => v,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(KimoError::Cdp("CDP event channel closed (browser exited?)".into()));
                    }
                },
                _ = tokio::time::sleep(remaining) => {
                    return Err(KimoError::Cdp(format!("timed out waiting for CDP event {method}")));
                }
            };
            let m = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
            if m != method {
                continue;
            }
            if let Some(want) = session {
                let got = v.get("sessionId").and_then(|s| s.as_str());
                if got != Some(want) {
                    continue;
                }
            }
            return Ok(v.get("params").cloned().unwrap_or(Value::Null));
        }
    }
}
