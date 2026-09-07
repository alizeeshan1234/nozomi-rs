//! Websocket tip stream: the live feed behind `GET /tip_floor`.
//!
//! Each message is the same shape as the REST response, a one-element array of
//! [`TipFloor`], and is parsed with the same code. Use it to keep a tip size
//! current without polling.
//!
//! Observed live on 2026-09-07: the server pushes one frame every 15 seconds,
//! the first up to 15 seconds after connecting, with or without an API key.
//!
//! ```no_run
//! use nozomi_client::{tipfloor::Percentile, TipStream};
//!
//! # async fn run() -> Result<(), nozomi_client::Error> {
//! let mut stream = TipStream::connect("wss://api.nozomi.temporal.xyz/tip_stream").await?;
//! while let Some(floor) = stream.next().await? {
//!     let lamports = floor.lamports(Percentile::P75);
//!     // store it where your sender reads it
//! }
//! # Ok(()) }
//! ```

use std::time::Duration;

use futures_util::StreamExt;
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, warn};

use crate::error::Error;
use crate::tipfloor::{Percentile, TipFloor};
use crate::Result;

/// Default stream URL, no key. It streamed without one when measured;
/// [`Client::tip_stream`](crate::Client::tip_stream) appends `?c=<key>` anyway,
/// since the docs show the key on every endpoint.
pub const TIP_STREAM_URL: &str = "wss://api.nozomi.temporal.xyz/tip_stream";

/// Default bound on the TCP connect plus websocket upgrade.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default bound on the wait for one frame: three times the observed 15-second
/// cadence. A half-open connection that never delivers a frame is reported as
/// an error, not waited on forever.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(45);

/// An open tip stream. `Debug` output does not include the URL.
pub struct TipStream {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    read_timeout: Duration,
}

impl std::fmt::Debug for TipStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TipStream")
            .field("read_timeout", &self.read_timeout)
            .finish_non_exhaustive()
    }
}

impl TipStream {
    /// Connect to a `ws://` or `wss://` URL with the default timeouts.
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_with(url, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT).await
    }

    /// Connect with explicit bounds on the connect and on each frame wait.
    pub async fn connect_with(
        url: &str,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Self> {
        let (ws, resp) =
            tokio::time::timeout(connect_timeout, tokio_tungstenite::connect_async(url))
                .await
                .map_err(|_| Error::Timeout {
                    after: Some(connect_timeout),
                })?
                .map_err(|e| Error::WebSocket(scrub(e)))?;
        debug!(status = %resp.status(), "tip stream connected");
        Ok(Self { ws, read_timeout })
    }

    /// Next tip floor, or `None` once the server closes the stream. Pings are
    /// answered and non-JSON frames skipped with a warning. Returns
    /// [`Error::Timeout`] if no frame arrives within the read timeout; treat
    /// that as a dead connection and reconnect.
    pub async fn next(&mut self) -> Result<Option<TipFloor>> {
        let deadline = tokio::time::Instant::now() + self.read_timeout;
        loop {
            let msg = match tokio::time::timeout_at(deadline, self.ws.next()).await {
                Err(_) => {
                    return Err(Error::Timeout {
                        after: Some(self.read_timeout),
                    })
                }
                Ok(None) => return Ok(None),
                Ok(Some(Err(e))) => return Err(Error::WebSocket(scrub(e))),
                Ok(Some(Ok(m))) => m,
            };
            let text = match msg {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                Message::Close(_) => return Ok(None),
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            };
            let v: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "tip stream sent non-JSON frame");
                    continue;
                }
            };
            match TipFloor::from_value(&v) {
                Ok(f) => return Ok(Some(f)),
                Err(e) => {
                    warn!(error = %e, "tip stream frame did not parse");
                    continue;
                }
            }
        }
    }

    /// Close the stream politely.
    pub async fn close(mut self) -> Result<()> {
        self.ws
            .close(None)
            .await
            .map_err(|e| Error::WebSocket(scrub(e)))?;
        Ok(())
    }

    /// Keep the latest tip floor in a [`watch`] channel, reconnecting for as
    /// long as the returned [`TipWatch`] is alive, with the default timeouts.
    /// This is the shape a sender wants: read [`TipWatch::latest_lamports`] at
    /// send time, never block on the network.
    ///
    /// Reconnects after any disconnect, read timeout included, with a delay
    /// that starts at 1 s and doubles to 30 s; receiving a frame resets it, so
    /// a server that closes every connection at once cannot drive a hot loop.
    /// The channel holds `None` until the first frame arrives and keeps the
    /// last frame across reconnects; check [`TipFloor::time`] if staleness
    /// matters.
    ///
    /// # Panics
    ///
    /// Panics if called outside a tokio runtime, like `tokio::spawn`.
    pub fn watch(url: impl Into<String>) -> TipWatch {
        Self::watch_with(url, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT)
    }

    /// [`watch`](Self::watch) with explicit connect and per-frame timeouts.
    ///
    /// # Panics
    ///
    /// Panics if called outside a tokio runtime, like `tokio::spawn`.
    pub fn watch_with(
        url: impl Into<String>,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> TipWatch {
        let url = url.into();
        let (tx, rx) = watch::channel(None);
        let handle = tokio::spawn(async move {
            const MIN_BACKOFF: Duration = Duration::from_secs(1);
            const MAX_BACKOFF: Duration = Duration::from_secs(30);
            let mut backoff = MIN_BACKOFF;
            loop {
                match TipStream::connect_with(&url, connect_timeout, read_timeout).await {
                    Ok(mut stream) => loop {
                        match stream.next().await {
                            Ok(Some(floor)) => {
                                backoff = MIN_BACKOFF;
                                if tx.send(Some(floor)).is_err() {
                                    return; // every receiver is gone
                                }
                            }
                            Ok(None) => {
                                debug!(retry_in = ?backoff, "tip stream closed by server");
                                break;
                            }
                            Err(e) => {
                                warn!(error = %e, retry_in = ?backoff, "tip stream read failed");
                                break;
                            }
                        }
                    },
                    Err(e) => {
                        warn!(error = %e, retry_in = ?backoff, "tip stream connect failed");
                    }
                }
                if tx.is_closed() {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        });
        TipWatch { rx, handle }
    }
}

/// A background tip-stream subscription. Dropping it stops the task.
#[derive(Debug)]
pub struct TipWatch {
    rx: watch::Receiver<Option<TipFloor>>,
    handle: tokio::task::JoinHandle<()>,
}

impl TipWatch {
    /// The latest tip floor, or `None` before the first frame. Clones the
    /// frame; on a hot path prefer [`latest_lamports`](Self::latest_lamports).
    pub fn latest(&self) -> Option<TipFloor> {
        self.rx.borrow().clone()
    }

    /// The tip at `p` from the latest frame, in lamports, or the Nozomi
    /// minimum before the first frame. No allocation.
    pub fn latest_lamports(&self, p: Percentile) -> u64 {
        self.rx
            .borrow()
            .as_ref()
            .map_or(crate::MIN_TIP_LAMPORTS, |f| f.lamports(p))
    }

    /// A receiver you can hand to other tasks. `changed().await` wakes on each
    /// new frame.
    pub fn subscribe(&self) -> watch::Receiver<Option<TipFloor>> {
        self.rx.clone()
    }

    /// Wait for the next frame after the current one.
    pub async fn changed(&mut self) -> Option<TipFloor> {
        self.rx.changed().await.ok()?;
        self.rx.borrow_and_update().clone()
    }
}

impl Drop for TipWatch {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// tungstenite errors can carry the request URL on handshake failure; keep
/// only the kind so a keyed URL never reaches a log line.
fn scrub(e: tokio_tungstenite::tungstenite::Error) -> String {
    use tokio_tungstenite::tungstenite::Error as E;
    match e {
        E::Http(resp) => format!("http status {}", resp.status()),
        E::Url(_) => "invalid url".to_string(),
        E::Io(io) => format!("io: {}", io.kind()),
        other => other.to_string(),
    }
}
