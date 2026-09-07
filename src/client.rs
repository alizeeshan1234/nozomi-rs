use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::json;
use tracing::{debug, instrument, warn};

use crate::error::Error;
use crate::region::{Region, Route};
use crate::tipfloor::TipFloor;
use crate::util::{encode_query_value, normalize_base_url, retry_after, REDACTED};
use crate::{Result, MAX_BATCH_BODY_BYTES, MAX_BATCH_TXS, MAX_TX_BYTES, MIN_TX_BYTES};

/// Base URL of the tip-floor API. Separate host from the send endpoints.
pub const TIP_API_BASE: &str = "https://api.nozomi.temporal.xyz";

/// Running counters kept by a [`Client`]. Cheap to read; export them however you like.
#[derive(Debug, Default)]
pub struct Stats {
    /// Transactions handed to `send`, `send_rpc`, or inside `send_batch`.
    pub submitted: AtomicU64,
    /// Transactions the server answered 200 for.
    pub accepted: AtomicU64,
    /// Calls that failed locally before any bytes left (tip, size, or batch
    /// checks). One per call, whatever the batch size.
    pub rejected_locally: AtomicU64,
    /// Sends that reached the server and were refused (4xx/5xx).
    pub rejected_by_server: AtomicU64,
    /// Calls that failed at the transport layer, timeouts included.
    pub transport_errors: AtomicU64,
    /// Sum of round-trip microseconds for calls that got any HTTP response.
    pub roundtrip_micros: AtomicU64,
    /// Number of calls counted in `roundtrip_micros`.
    pub roundtrips: AtomicU64,
}

impl Stats {
    /// Mean round trip in microseconds, or 0 if nothing has been sent.
    pub fn mean_roundtrip_micros(&self) -> u64 {
        self.roundtrip_micros
            .load(Ordering::Relaxed)
            .checked_div(self.roundtrips.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

/// Builder for [`Client`].
///
/// `Debug` output redacts the API key.
#[derive(Clone)]
pub struct ClientBuilder {
    api_key: Option<String>,
    region: Region,
    route: Route,
    timeout: Duration,
    user_agent: String,
    tip_api_base: String,
    base_url: Option<String>,
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("api_key", &self.api_key.as_ref().map(|_| REDACTED))
            .field("region", &self.region)
            .field("route", &self.route)
            .field("timeout", &self.timeout)
            .field("user_agent", &self.user_agent)
            .field("tip_api_base", &self.tip_api_base)
            .field("base_url", &self.base_url)
            .finish()
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            api_key: None,
            region: Region::Auto,
            route: Route::Cloudflare,
            timeout: Duration::from_secs(5),
            user_agent: format!("nozomi-client/{}", env!("CARGO_PKG_VERSION")),
            tip_api_base: TIP_API_BASE.to_string(),
            base_url: None,
        }
    }
}

impl ClientBuilder {
    /// Nozomi API key. Sent percent-encoded as the `c` query parameter, as the
    /// docs specify.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }
    /// Region to send to. Defaults to `Region::Auto`.
    pub fn region(mut self, region: Region) -> Self {
        self.region = region;
        self
    }
    /// Route to the region. Ignored for `Region::Auto`. Defaults to Cloudflare.
    pub fn route(mut self, route: Route) -> Self {
        self.route = route;
        self
    }
    /// Per-request timeout. Defaults to 5 seconds.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    /// Override the User-Agent header.
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }
    /// Override the tip-floor API base (for tests or proxies).
    pub fn tip_api_base(mut self, base: impl Into<String>) -> Self {
        self.tip_api_base = base.into();
        self
    }
    /// Override the send base URL (scheme + host, no trailing slash) instead of
    /// deriving it from `region` and `route`. For tests, proxies, or a private
    /// endpoint Temporal has given you.
    pub fn base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = Some(base.into());
        self
    }

    pub fn build(self) -> Result<Client> {
        let api_key = self
            .api_key
            .ok_or_else(|| Error::Config("api_key is required".into()))?;
        if api_key.trim().is_empty() {
            return Err(Error::Config("api_key is empty".into()));
        }
        let http = reqwest::Client::builder()
            .timeout(self.timeout)
            .user_agent(self.user_agent)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(4)
            .tcp_nodelay(true)
            .build()?;
        let base_url = match self.base_url {
            Some(b) => normalize_base_url("base_url", &b)?,
            None => self.region.base_url(self.route),
        };
        let tip_api_base = normalize_base_url("tip_api_base", &self.tip_api_base)?;
        Ok(Client {
            inner: Arc::new(Inner {
                http,
                base_url,
                tip_api_base,
                api_key_encoded: encode_query_value(&api_key),
                timeout: self.timeout,
                region: self.region,
                route: self.route,
                stats: Stats::default(),
            }),
        })
    }
}

struct Inner {
    http: reqwest::Client,
    base_url: String,
    tip_api_base: String,
    /// Percent-encoded once at build time.
    api_key_encoded: String,
    timeout: Duration,
    region: Region,
    route: Route,
    stats: Stats,
}

/// A Nozomi client. Cheap to clone; clones share one connection pool and one set
/// of counters.
///
/// `Debug` output redacts the API key. Transport errors have their URL stripped
/// for the same reason: the key travels in the query string.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base_url", &self.inner.base_url)
            .field("tip_api_base", &self.inner.tip_api_base)
            .field("api_key", &REDACTED)
            .field("region", &self.inner.region)
            .field("route", &self.inner.route)
            .field("stats", &self.inner.stats)
            .finish()
    }
}

/// Handle for the keep-alive task. Dropping it stops the pings.
#[derive(Debug)]
pub struct KeepAlive {
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for KeepAlive {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Client {
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// Region this client sends to.
    pub fn region(&self) -> Region {
        self.inner.region
    }

    /// Route this client uses.
    pub fn route(&self) -> Route {
        self.inner.route
    }

    /// Base URL this client sends to (scheme + host).
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    /// Running counters.
    pub fn stats(&self) -> &Stats {
        &self.inner.stats
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}{}?c={}",
            self.inner.base_url, path, self.inner.api_key_encoded
        )
    }

    /// Map a request failure, counting it and classifying timeouts.
    fn transport(&self, e: reqwest::Error) -> Error {
        self.inner
            .stats
            .transport_errors
            .fetch_add(1, Ordering::Relaxed);
        Error::from_reqwest(e, Some(self.inner.timeout))
    }

    fn decode(&self, e: reqwest::Error) -> Error {
        Error::decode_reqwest(e, Some(self.inner.timeout))
    }

    fn check_size(&self, tx: &[u8]) -> Result<()> {
        if tx.len() > MAX_TX_BYTES {
            self.inner
                .stats
                .rejected_locally
                .fetch_add(1, Ordering::Relaxed);
            return Err(Error::TransactionTooLarge {
                size: tx.len(),
                max: MAX_TX_BYTES,
            });
        }
        if tx.len() < MIN_TX_BYTES {
            self.inner
                .stats
                .rejected_locally
                .fetch_add(1, Ordering::Relaxed);
            return Err(Error::TransactionTooSmall {
                size: tx.len(),
                min: MIN_TX_BYTES,
            });
        }
        Ok(())
    }

    async fn classify(
        &self,
        resp: reqwest::Response,
        started: Instant,
    ) -> Result<reqwest::Response> {
        let elapsed = started.elapsed().as_micros() as u64;
        self.inner
            .stats
            .roundtrip_micros
            .fetch_add(elapsed, Ordering::Relaxed);
        self.inner.stats.roundtrips.fetch_add(1, Ordering::Relaxed);
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        self.inner
            .stats
            .rejected_by_server
            .fetch_add(1, Ordering::Relaxed);
        let retry_after = retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        Err(Error::from_status(status, retry_after, body))
    }

    /// Send one signed transaction over API v2 (`POST /api/sendTransaction2`).
    ///
    /// This is the lowest-latency path. The server returns an empty 200 and no
    /// signature; compute the signature locally from the transaction if you need it.
    /// `tx` is the serialized transaction; it is base64-encoded here as the API requires.
    #[instrument(skip(self, tx), fields(region = %self.inner.region, bytes = tx.len()))]
    pub async fn send(&self, tx: &[u8]) -> Result<()> {
        self.check_size(tx)?;
        self.inner.stats.submitted.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let resp = self
            .inner
            .http
            .post(self.url("/api/sendTransaction2"))
            .header(reqwest::header::CONTENT_TYPE, "text/plain")
            .body(B64.encode(tx))
            .send()
            .await
            .map_err(|e| self.transport(e))?;
        self.classify(resp, started).await?;
        self.inner.stats.accepted.fetch_add(1, Ordering::Relaxed);
        debug!(micros = started.elapsed().as_micros() as u64, "accepted");
        Ok(())
    }

    /// Send one signed transaction over JSON-RPC (`sendTransaction`, base64 encoding).
    ///
    /// Slightly more overhead than [`send`](Self::send) but returns the signature
    /// the server saw. Drop-in for a Solana RPC `sendTransaction`.
    #[instrument(skip(self, tx), fields(region = %self.inner.region, bytes = tx.len()))]
    pub async fn send_rpc(&self, tx: &[u8]) -> Result<String> {
        self.check_size(tx)?;
        self.inner.stats.submitted.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [B64.encode(tx), { "encoding": "base64" }],
        });
        let started = Instant::now();
        let resp = self
            .inner
            .http
            .post(self.url("/"))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.transport(e))?;
        let resp = self.classify(resp, started).await?;
        let v: serde_json::Value = resp.json().await.map_err(|e| self.decode(e))?;
        if let Some(err) = v.get("error") {
            self.inner
                .stats
                .rejected_by_server
                .fetch_add(1, Ordering::Relaxed);
            return Err(Error::Rpc {
                code: err.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                message: err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
        let sig = v
            .get("result")
            .and_then(|r| r.as_str())
            .ok_or_else(|| Error::Decode(format!("no result in {v}")))?
            .to_string();
        self.inner.stats.accepted.fetch_add(1, Ordering::Relaxed);
        debug!(%sig, micros = started.elapsed().as_micros() as u64, "accepted");
        Ok(sig)
    }

    /// Frame transactions for `sendBatch`: `[len_hi][len_lo][bytes]...`, big-endian u16 lengths.
    /// Validates count, per-transaction size, and total body size before framing.
    pub fn frame_batch(txs: &[&[u8]]) -> Result<Vec<u8>> {
        if txs.is_empty() {
            return Err(Error::EmptyBatch);
        }
        if txs.len() > MAX_BATCH_TXS {
            return Err(Error::BatchTooLarge {
                count: txs.len(),
                max: MAX_BATCH_TXS,
            });
        }
        let mut body = Vec::with_capacity(txs.iter().map(|t| t.len() + 2).sum());
        for tx in txs {
            if tx.len() > MAX_TX_BYTES {
                return Err(Error::TransactionTooLarge {
                    size: tx.len(),
                    max: MAX_TX_BYTES,
                });
            }
            if tx.len() < MIN_TX_BYTES {
                return Err(Error::TransactionTooSmall {
                    size: tx.len(),
                    min: MIN_TX_BYTES,
                });
            }
            let len = tx.len() as u16;
            body.extend_from_slice(&len.to_be_bytes());
            body.extend_from_slice(tx);
        }
        if body.len() > MAX_BATCH_BODY_BYTES {
            return Err(Error::BatchBodyTooLarge {
                size: body.len(),
                max: MAX_BATCH_BODY_BYTES,
            });
        }
        Ok(body)
    }

    /// Send up to 16 signed transactions in one call (`POST /api/sendBatch`).
    ///
    /// The server processes the stream as it parses it: if transaction N is
    /// rejected, transactions 1..N-1 may already have been forwarded. There is no
    /// rollback and no signatures come back. A `BadRequest` here therefore means
    /// "at least one was refused", not "none were sent".
    #[instrument(skip(self, txs), fields(region = %self.inner.region, count = txs.len()))]
    pub async fn send_batch(&self, txs: &[&[u8]]) -> Result<()> {
        let body = Self::frame_batch(txs).inspect_err(|_| {
            self.inner
                .stats
                .rejected_locally
                .fetch_add(1, Ordering::Relaxed);
        })?;
        self.inner
            .stats
            .submitted
            .fetch_add(txs.len() as u64, Ordering::Relaxed);
        let started = Instant::now();
        let resp = self
            .inner
            .http
            .post(self.url("/api/sendBatch"))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body)
            .send()
            .await
            .map_err(|e| self.transport(e))?;
        self.classify(resp, started).await?;
        self.inner
            .stats
            .accepted
            .fetch_add(txs.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    /// `GET /ping`. Not a health check; it exists to keep the idle connection open.
    pub async fn ping(&self) -> Result<()> {
        let resp = self
            .inner
            .http
            .get(format!("{}/ping", self.inner.base_url))
            .send()
            .await
            .map_err(|e| Error::from_reqwest(e, Some(self.inner.timeout)))?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(Error::from_status(status, None, body))
        }
    }

    /// Spawn a task that pings every 60 seconds. The server closes idle
    /// connections after 65 seconds, so this keeps one warm connection ready for
    /// the next send. Drop the returned handle to stop.
    ///
    /// One ping keeps one connection warm. The pool holds up to four idle
    /// connections per host, so a burst of concurrent sends after a quiet
    /// minute pays a connect on all but one of them. If you send concurrently
    /// and care about that, run one client per sending task.
    ///
    /// # Panics
    ///
    /// Panics if called outside a tokio runtime, like `tokio::spawn`.
    pub fn spawn_keepalive(&self) -> KeepAlive {
        self.spawn_keepalive_every(Duration::from_secs(60))
    }

    /// Like [`spawn_keepalive`](Self::spawn_keepalive) with a custom interval.
    /// Don't go below the default without a reason; the docs ask you not to.
    ///
    /// # Panics
    ///
    /// Panics if called outside a tokio runtime, like `tokio::spawn`.
    pub fn spawn_keepalive_every(&self, interval: Duration) -> KeepAlive {
        let client = self.clone();
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if let Err(e) = client.ping().await {
                    warn!(error = %e, "keepalive ping failed");
                }
            }
        });
        KeepAlive { handle }
    }

    /// Current landed-tip percentiles from `GET /tip_floor`.
    ///
    /// The API answers with `null` percentiles when it has no recent data and
    /// has been seen to return 503 from nginx (both on 2026-09-07); the first is
    /// an `Ok` with [`TipFloor::is_complete`] false, the second an
    /// [`Error::Server`]. Size the tip from [`TipFloor::lamports`], which falls
    /// back to the minimum, and keep the last good value on error.
    pub async fn tip_floor(&self) -> Result<TipFloor> {
        let resp = self
            .inner
            .http
            .get(format!("{}/tip_floor", self.inner.tip_api_base))
            .send()
            .await
            .map_err(|e| Error::from_reqwest(e, Some(self.inner.timeout)))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::from_status(status, None, body));
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| self.decode(e))?;
        TipFloor::from_value(&v)
    }

    /// Open the websocket tip stream behind [`tip_floor`](Self::tip_floor), on
    /// the same API host, authenticated with this client's key.
    ///
    /// The connect is bounded by this client's timeout; frames by
    /// [`DEFAULT_READ_TIMEOUT`](crate::tipstream::DEFAULT_READ_TIMEOUT).
    #[cfg(feature = "tip-stream")]
    pub async fn tip_stream(&self) -> Result<crate::tipstream::TipStream> {
        crate::tipstream::TipStream::connect_with(
            &self.tip_stream_url(),
            self.inner.timeout,
            crate::tipstream::DEFAULT_READ_TIMEOUT,
        )
        .await
    }

    /// A reconnecting background subscription to the tip stream; see
    /// [`TipStream::watch`](crate::tipstream::TipStream::watch).
    #[cfg(feature = "tip-stream")]
    pub fn tip_watch(&self) -> crate::tipstream::TipWatch {
        crate::tipstream::TipStream::watch_with(
            self.tip_stream_url(),
            self.inner.timeout,
            crate::tipstream::DEFAULT_READ_TIMEOUT,
        )
    }

    #[cfg(feature = "tip-stream")]
    fn tip_stream_url(&self) -> String {
        let ws_base = self
            .inner
            .tip_api_base
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        format!("{ws_base}/tip_stream?c={}", self.inner.api_key_encoded)
    }

    /// Send a signed `VersionedTransaction`, after checking locally that it carries
    /// a tip of at least the minimum. Uses API v2 and returns the transaction's
    /// first signature, computed locally.
    ///
    /// The check resolves static account keys only. A system transfer whose
    /// destination is loaded from an address lookup table is refused with
    /// [`Error::TipUnresolved`] rather than guessed at; pass the loaded
    /// addresses to [`send_transaction_with_loaded`](Self::send_transaction_with_loaded)
    /// to check it exactly, or use
    /// [`send_transaction_unchecked`](Self::send_transaction_unchecked).
    #[cfg(feature = "solana")]
    pub async fn send_transaction(
        &self,
        tx: &solana_transaction::versioned::VersionedTransaction,
    ) -> Result<String> {
        self.send_transaction_with_loaded(tx, &[]).await
    }

    /// Like [`send_transaction`](Self::send_transaction), with the addresses
    /// the transaction loads from its lookup tables so a tip whose destination
    /// lives there is checked exactly. `loaded` is the writable loaded
    /// addresses followed by the readonly ones, in table order, the same
    /// layout the runtime uses (`LoadedAddresses::writable` then `readonly`).
    #[cfg(feature = "solana")]
    #[instrument(
        skip(self, tx, loaded),
        fields(region = %self.inner.region, tip_lamports = tracing::field::Empty)
    )]
    pub async fn send_transaction_with_loaded(
        &self,
        tx: &solana_transaction::versioned::VersionedTransaction,
        loaded: &[solana_pubkey::Pubkey],
    ) -> Result<String> {
        let inspection = crate::tip::inspect_with_loaded(tx, loaded);
        if let Err(e) = inspection.check() {
            self.inner
                .stats
                .rejected_locally
                .fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
        tracing::Span::current().record("tip_lamports", inspection.total_lamports());
        self.send_versioned(tx).await
    }

    /// Send a signed `VersionedTransaction` without inspecting it for a tip.
    /// Nozomi still drops under-tipped transactions silently.
    #[cfg(feature = "solana")]
    #[instrument(skip(self, tx), fields(region = %self.inner.region))]
    pub async fn send_transaction_unchecked(
        &self,
        tx: &solana_transaction::versioned::VersionedTransaction,
    ) -> Result<String> {
        self.send_versioned(tx).await
    }

    #[cfg(feature = "solana")]
    async fn send_versioned(
        &self,
        tx: &solana_transaction::versioned::VersionedTransaction,
    ) -> Result<String> {
        let bytes = crate::tip::serialize(tx)?;
        self.send(&bytes).await?;
        Ok(tx
            .signatures
            .first()
            .map(|s| s.to_string())
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_requires_key() {
        assert!(matches!(Client::builder().build(), Err(Error::Config(_))));
        assert!(matches!(
            Client::builder().api_key("  ").build(),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn urls_include_key_and_path() {
        let c = Client::builder()
            .api_key("k")
            .region(Region::Tokyo)
            .route(Route::Direct { tls: false })
            .build()
            .unwrap();
        assert_eq!(
            c.url("/api/sendTransaction2"),
            "http://tyo1.nozomi.temporal.xyz/api/sendTransaction2?c=k"
        );
        assert_eq!(c.url("/"), "http://tyo1.nozomi.temporal.xyz/?c=k");
        let c = Client::builder()
            .api_key("ab c#d&e\n")
            .region(Region::Tokyo)
            .route(Route::Direct { tls: false })
            .build()
            .unwrap();
        assert_eq!(
            c.url("/ping"),
            "http://tyo1.nozomi.temporal.xyz/ping?c=ab%20c%23d%26e%0A"
        );
    }

    #[test]
    fn tip_api_base_is_normalized() {
        let c = Client::builder()
            .api_key("k")
            .tip_api_base("https://api.example.test/")
            .build()
            .unwrap();
        assert_eq!(c.inner.tip_api_base, "https://api.example.test");
        assert!(matches!(
            Client::builder()
                .api_key("k")
                .tip_api_base("api.example.test")
                .build(),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn debug_redacts_api_key() {
        let b = Client::builder().api_key("SECRETKEY");
        assert!(!format!("{b:?}").contains("SECRETKEY"));
        let c = b.build().unwrap();
        assert!(!format!("{c:?}").contains("SECRETKEY"));
    }

    #[test]
    fn base_url_override() {
        let c = Client::builder()
            .api_key("k")
            .base_url("http://127.0.0.1:8080/")
            .build()
            .unwrap();
        assert_eq!(c.base_url(), "http://127.0.0.1:8080");
        assert_eq!(c.url("/ping"), "http://127.0.0.1:8080/ping?c=k");
        assert!(matches!(
            Client::builder().api_key("k").base_url("localhost").build(),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn frame_batch_matches_docs() {
        let a = vec![1u8; 66];
        let b = vec![2u8; 300];
        let body = Client::frame_batch(&[&a, &b]).unwrap();
        assert_eq!(&body[0..2], &66u16.to_be_bytes());
        assert_eq!(&body[2..68], &a[..]);
        assert_eq!(&body[68..70], &300u16.to_be_bytes());
        assert_eq!(body.len(), 2 + 66 + 2 + 300);
    }

    #[test]
    fn frame_batch_enforces_limits() {
        assert!(matches!(Client::frame_batch(&[]), Err(Error::EmptyBatch)));
        let ok = vec![0u8; 100];
        let seventeen: Vec<&[u8]> = (0..17).map(|_| ok.as_slice()).collect();
        assert!(matches!(
            Client::frame_batch(&seventeen),
            Err(Error::BatchTooLarge { count: 17, .. })
        ));
        let big = vec![0u8; 1233];
        assert!(matches!(
            Client::frame_batch(&[&big]),
            Err(Error::TransactionTooLarge { .. })
        ));
        let tiny = vec![0u8; 65];
        assert!(matches!(
            Client::frame_batch(&[&tiny]),
            Err(Error::TransactionTooSmall { .. })
        ));
        // 16 max-size transactions is exactly the documented body cap (19,744 bytes).
        let max = vec![0u8; 1232];
        let sixteen: Vec<&[u8]> = (0..16).map(|_| max.as_slice()).collect();
        assert_eq!(
            Client::frame_batch(&sixteen).unwrap().len(),
            crate::MAX_BATCH_BODY_BYTES
        );
    }
}
