//! HTTP/3 over QUIC submission.
//!
//! Nozomi documents QUIC for workloads that cannot keep a TCP connection warm:
//! QUIC session resumption is cheaper than a fresh TCP + TLS handshake. If you
//! can keep a connection open, API v2 or batch send over a direct `http://`
//! endpoint with [`Client::spawn_keepalive`](crate::Client::spawn_keepalive)
//! is what the docs recommend instead. Transport does not change priority; the
//! tip does.
//!
//! The wire format is the same as [`Client::send_batch`](crate::Client::send_batch):
//! a `POST /api/sendBatch?c=<key>` with a big-endian `u16` length prefix per
//! transaction. Temporal's reference client sends the request and never reads
//! the response, so it cannot tell a 200 from a 400. This one reads it and maps
//! the status the same way as the HTTP client.
//!
//! ```no_run
//! use nozomi_client::{QuicClient, Region};
//!
//! # async fn run() -> Result<(), nozomi_client::Error> {
//! let client = QuicClient::builder()
//!     .api_key(std::env::var("NOZOMI_API_KEY").unwrap())
//!     .region(Region::Frankfurt)
//!     .build()?;
//! client.warmup().await?; // handshake now, not on the first send
//! let tx_bytes: Vec<u8> = todo!("serialized signed transaction with a tip");
//! client.send(&tx_bytes).await?;
//! # Ok(()) }
//! ```

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes};
use h3::client::SendRequest;
use h3_quinn::OpenStreams;
use http::HeaderValue;
use rustls::pki_types::CertificateDer;
use tokio::sync::watch;
use tracing::{debug, instrument, warn};

use crate::client::Stats;
use crate::error::Error;
use crate::region::Region;
use crate::util::{encode_query_value, retry_after, REDACTED};
use crate::Result;

/// Most of a response body that is kept. Nozomi answers with a short line;
/// anything past this is dropped so a misbehaving upstream cannot fill memory.
const MAX_RESPONSE_BODY: usize = 64 * 1024;

/// Builder for [`QuicClient`]. `Debug` output redacts the API key.
#[derive(Clone)]
pub struct QuicClientBuilder {
    api_key: Option<String>,
    region: Region,
    host: Option<String>,
    port: u16,
    timeout: Duration,
    idle_timeout: Duration,
    keep_alive: Duration,
    extra_roots: Vec<CertificateDer<'static>>,
}

impl fmt::Debug for QuicClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuicClientBuilder")
            .field("api_key", &self.api_key.as_ref().map(|_| REDACTED))
            .field("region", &self.region)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("timeout", &self.timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("keep_alive", &self.keep_alive)
            .field("extra_roots", &self.extra_roots.len())
            .finish()
    }
}

impl Default for QuicClientBuilder {
    fn default() -> Self {
        Self {
            api_key: None,
            region: Region::Auto,
            host: None,
            port: 443,
            timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(120),
            keep_alive: Duration::from_secs(15),
            extra_roots: Vec::new(),
        }
    }
}

impl QuicClientBuilder {
    /// Nozomi API key. Sent percent-encoded as the `c` query parameter.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }
    /// Region to send to. Defaults to `Region::Auto` (`edge.nozomi.temporal.xyz`).
    pub fn region(mut self, region: Region) -> Self {
        self.region = region;
        self
    }
    /// Override the host instead of deriving it from `region`. A DNS name or
    /// an IP literal (IPv6 with or without brackets). Also used as the TLS
    /// server name.
    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }
    /// UDP port. Defaults to 443.
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
    /// Per-request timeout, covering connect (if needed), send, and response.
    /// Defaults to 5 seconds. A timeout never cancels a reconnect in progress;
    /// that runs to completion on its own task so later sends can use it.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    /// QUIC idle timeout. Defaults to 120 seconds.
    pub fn idle_timeout(mut self, d: Duration) -> Self {
        self.idle_timeout = d;
        self
    }
    /// QUIC keep-alive ping interval. Defaults to 15 seconds.
    pub fn keep_alive(mut self, d: Duration) -> Self {
        self.keep_alive = d;
        self
    }
    /// Trust an additional root certificate, on top of the Mozilla roots. For
    /// private endpoints and tests.
    pub fn root_certificate(mut self, der: CertificateDer<'static>) -> Self {
        self.extra_roots.push(der);
        self
    }

    /// Build the client. Nothing is bound or connected until
    /// [`QuicClient::warmup`] or the first send; the UDP socket is opened then,
    /// in the address family of the resolved peer.
    pub fn build(self) -> Result<QuicClient> {
        let api_key = self
            .api_key
            .ok_or_else(|| Error::Config("api_key is required".into()))?;
        if api_key.trim().is_empty() {
            return Err(Error::Config("api_key is empty".into()));
        }
        let host = self.host.unwrap_or_else(|| self.region.quic_host());
        let host = host.trim().to_string();
        if host.is_empty() {
            return Err(Error::Config("host is empty".into()));
        }
        // "[::1]" and "::1" both mean the IPv6 literal; keep one canonical form.
        let server_name = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let is_ipv6_literal = server_name.parse::<std::net::Ipv6Addr>().is_ok();
        let host_for_lookup = if is_ipv6_literal {
            format!("[{server_name}]")
        } else {
            server_name.clone()
        };
        let authority = if self.port == 443 {
            host_for_lookup.clone()
        } else {
            format!("{host_for_lookup}:{}", self.port)
        };
        let authority = HeaderValue::from_str(&authority)
            .map_err(|e| Error::Config(format!("host is not a valid header value: {e}")))?;
        let uri: http::Uri = format!("/api/sendBatch?c={}", encode_query_value(&api_key))
            .parse()
            .map_err(|e| Error::Config(format!("request uri: {e}")))?;

        let mut roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        for der in self.extra_roots {
            roots
                .add(der)
                .map_err(|e| Error::Config(format!("bad root certificate: {e}")))?;
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::Config(format!("tls: {e}")))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"h3".to_vec()];

        let quic_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
            .map_err(|e| Error::Config(format!("quic tls: {e}")))?;
        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(
            self.idle_timeout
                .try_into()
                .map_err(|e| Error::Config(format!("idle_timeout: {e}")))?,
        ));
        transport.keep_alive_interval(Some(self.keep_alive));
        let mut client_config = quinn::ClientConfig::new(Arc::new(quic_tls));
        client_config.transport_config(Arc::new(transport));

        let (epoch, _) = watch::channel(0u64);
        Ok(QuicClient {
            inner: Arc::new(Inner {
                client_config,
                endpoints: Mutex::new([None, None]),
                host_for_lookup,
                server_name,
                port: self.port,
                authority,
                uri,
                timeout: self.timeout,
                state: Mutex::new(State::default()),
                epoch,
                stats: Stats::default(),
            }),
        })
    }
}

struct Conn {
    quic: quinn::Connection,
    send_request: SendRequest<OpenStreams, Bytes>,
    driver: tokio::task::JoinHandle<()>,
    generation: u64,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

#[derive(Default)]
struct State {
    conn: Option<Arc<Conn>>,
    /// A connect task is running; its result arrives through `epoch`.
    connecting: bool,
    next_generation: u64,
    /// Why the most recent connect attempt failed, for waiters that missed it.
    last_error: Option<String>,
}

struct Inner {
    client_config: quinn::ClientConfig,
    /// One endpoint per address family, created on first use: `[v4, v6]`.
    endpoints: Mutex<[Option<quinn::Endpoint>; 2]>,
    host_for_lookup: String,
    server_name: String,
    port: u16,
    authority: HeaderValue,
    uri: http::Uri,
    timeout: Duration,
    state: Mutex<State>,
    /// Bumped whenever a connect attempt finishes, so waiters re-check `state`.
    epoch: watch::Sender<u64>,
    stats: Stats,
}

/// HTTP/3 over QUIC client for `/api/sendBatch`. Cheap to clone; clones share
/// one connection and one set of counters.
///
/// Connection handling: a connect runs on its own task, single-flighted, and is
/// never cancelled by a caller's timeout, so a burst of sends during an outage
/// does not restart the handshake over and over. A connection is dropped as
/// soon as any send sees it fail, or QUIC reports it closed, and the next send
/// reconnects. A send is retried once only if the failure happened before any
/// request bytes left, so nothing is ever submitted twice.
///
/// `Debug` output redacts the API key.
#[derive(Clone)]
pub struct QuicClient {
    inner: Arc<Inner>,
}

impl fmt::Debug for QuicClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuicClient")
            .field("host", &self.inner.host_for_lookup)
            .field("port", &self.inner.port)
            .field("api_key", &REDACTED)
            .field("stats", &self.inner.stats)
            .finish()
    }
}

enum Attempt {
    /// The request was never sent; safe to retry.
    NotSent(Error),
    /// The request left the process; this is its outcome.
    Sent(Result<()>),
}

impl QuicClient {
    pub fn builder() -> QuicClientBuilder {
        QuicClientBuilder::default()
    }

    /// Host this client sends to.
    pub fn host(&self) -> &str {
        &self.inner.host_for_lookup
    }

    /// Running counters, with the same meaning as on [`Client`](crate::Client).
    /// `roundtrip_micros` and `roundtrips` cover every request that got an
    /// HTTP/3 response, whatever its status.
    pub fn stats(&self) -> &Stats {
        &self.inner.stats
    }

    /// Resolve DNS and complete the QUIC and HTTP/3 handshakes now, so the
    /// first send does not pay for them. Safe to call again; a live connection
    /// is kept. Bounded by the client timeout; the connect itself continues in
    /// the background if that fires.
    pub async fn warmup(&self) -> Result<()> {
        tokio::time::timeout(self.inner.timeout, self.sender(None))
            .await
            .map_err(|_| Error::Timeout {
                after: Some(self.inner.timeout),
            })?
            .map(|_| ())
    }

    /// True if a connection is held and QUIC has not reported it closed. A
    /// silently dead path still shows as connected until a send finds out.
    pub fn is_connected(&self) -> bool {
        let st = self.inner.state.lock().expect("state mutex");
        st.conn
            .as_ref()
            .is_some_and(|c| c.quic.close_reason().is_none())
    }

    /// Drop the connection. The next send reconnects.
    pub fn disconnect(&self) {
        self.inner.state.lock().expect("state mutex").conn = None;
    }

    /// Send one transaction, as a batch of one.
    #[instrument(skip(self, tx), fields(host = %self.inner.host_for_lookup, bytes = tx.len()))]
    pub async fn send(&self, tx: &[u8]) -> Result<()> {
        self.send_batch(&[tx]).await
    }

    /// Send up to 16 transactions. Same framing and same partial-acceptance
    /// caveat as [`Client::send_batch`](crate::Client::send_batch).
    #[instrument(skip(self, txs), fields(host = %self.inner.host_for_lookup, count = txs.len()))]
    pub async fn send_batch(&self, txs: &[&[u8]]) -> Result<()> {
        let body = crate::Client::frame_batch(txs).inspect_err(|_| {
            self.inner
                .stats
                .rejected_locally
                .fetch_add(1, Ordering::Relaxed);
        })?;
        self.send_framed(Bytes::from(body), txs.len() as u64).await
    }

    /// Send a body already framed by [`Client::frame_batch`](crate::Client::frame_batch).
    /// The fastest path: frame once outside the hot loop, then send the same
    /// `Bytes` (a clone is a refcount bump). `count` is only used for the
    /// counters.
    ///
    /// On [`Error::Timeout`] or an [`Error::Quic`] raised after the request
    /// left, the server may still have accepted the batch. Nozomi retries
    /// landing on its side, so do not resubmit; verify by signature instead.
    #[instrument(skip(self, body), fields(host = %self.inner.host_for_lookup, bytes = body.len()))]
    pub async fn send_raw(&self, body: Bytes, count: u64) -> Result<()> {
        if body.is_empty() {
            self.inner
                .stats
                .rejected_locally
                .fetch_add(1, Ordering::Relaxed);
            return Err(Error::EmptyBatch);
        }
        if body.len() > crate::MAX_BATCH_BODY_BYTES {
            self.inner
                .stats
                .rejected_locally
                .fetch_add(1, Ordering::Relaxed);
            return Err(Error::BatchBodyTooLarge {
                size: body.len(),
                max: crate::MAX_BATCH_BODY_BYTES,
            });
        }
        self.send_framed(body, count).await
    }

    async fn send_framed(&self, body: Bytes, count: u64) -> Result<()> {
        self.inner
            .stats
            .submitted
            .fetch_add(count, Ordering::Relaxed);
        let started = Instant::now();
        // The generation a request actually went out on, if any. Set inside
        // the future, read after it completes or is dropped by the timeout.
        let mut used = None;
        let result =
            tokio::time::timeout(self.inner.timeout, self.send_with_retry(body, &mut used))
                .await
                .unwrap_or(Err(Error::Timeout {
                    after: Some(self.inner.timeout),
                }));
        let elapsed = started.elapsed().as_micros() as u64;
        match &result {
            Ok(()) => {
                self.inner
                    .stats
                    .accepted
                    .fetch_add(count, Ordering::Relaxed);
                self.record_roundtrip(elapsed);
                debug!(micros = elapsed, "accepted");
            }
            Err(Error::Quic(_)) | Err(Error::Timeout { .. }) => {
                self.inner
                    .stats
                    .transport_errors
                    .fetch_add(1, Ordering::Relaxed);
                // Whatever connection this ran on is suspect; drop it so the
                // next send reconnects instead of waiting on a dead path.
                if let Some(generation) = used {
                    self.evict(generation);
                }
            }
            Err(_) => {
                self.inner
                    .stats
                    .rejected_by_server
                    .fetch_add(1, Ordering::Relaxed);
                self.record_roundtrip(elapsed);
            }
        }
        result
    }

    fn record_roundtrip(&self, micros: u64) {
        self.inner
            .stats
            .roundtrip_micros
            .fetch_add(micros, Ordering::Relaxed);
        self.inner.stats.roundtrips.fetch_add(1, Ordering::Relaxed);
    }

    async fn send_with_retry(&self, body: Bytes, used: &mut Option<u64>) -> Result<()> {
        let (sr, generation) = self.sender(None).await?;
        *used = Some(generation);
        match self.attempt(sr, body.clone()).await {
            Attempt::Sent(r) => r,
            Attempt::NotSent(e) => {
                warn!(error = %e, "quic stream open failed; reconnecting once");
                let (sr, generation) = self.sender(Some(generation)).await?;
                *used = Some(generation);
                match self.attempt(sr, body).await {
                    Attempt::Sent(r) => r,
                    Attempt::NotSent(e) => Err(e),
                }
            }
        }
    }

    /// Drop the held connection if it is still the given generation.
    fn evict(&self, generation: u64) {
        let mut st = self.inner.state.lock().expect("state mutex");
        if st.conn.as_ref().is_some_and(|c| c.generation == generation) {
            debug!(generation, "evicting quic connection");
            st.conn = None;
        }
    }

    /// A request sender on a live connection. If `stale` is given, a held
    /// connection of that generation is dropped and replaced first. Waits for
    /// an in-flight connect rather than starting another; the connect runs on
    /// its own task, so dropping this future (a caller timeout) does not
    /// cancel it.
    async fn sender(&self, stale: Option<u64>) -> Result<(SendRequest<OpenStreams, Bytes>, u64)> {
        let mut stale = stale;
        loop {
            // Subscribe before looking, so a completion between the look and
            // the wait still wakes us.
            let mut rx = self.inner.epoch.subscribe();
            {
                let mut st = self.inner.state.lock().expect("state mutex");
                if let Some(c) = st.conn.as_ref() {
                    let dead = stale == Some(c.generation) || c.quic.close_reason().is_some();
                    if dead {
                        st.conn = None;
                    } else {
                        return Ok((c.send_request.clone(), c.generation));
                    }
                }
                stale = None;
                if !st.connecting {
                    st.connecting = true;
                    st.last_error = None;
                    let generation = st.next_generation;
                    st.next_generation += 1;
                    let inner = Arc::clone(&self.inner);
                    tokio::spawn(async move {
                        let outcome = connect(&inner, generation).await;
                        {
                            let mut st = inner.state.lock().expect("state mutex");
                            st.connecting = false;
                            match outcome {
                                Ok(conn) => st.conn = Some(Arc::new(conn)),
                                // Keep the bare message; waiters re-wrap it as Error::Quic.
                                Err(Error::Quic(m)) => st.last_error = Some(m),
                                Err(e) => st.last_error = Some(e.to_string()),
                            }
                        }
                        inner.epoch.send_modify(|e| *e = e.wrapping_add(1));
                    });
                }
            }
            if rx.changed().await.is_err() {
                return Err(Error::Quic("client dropped during connect".into()));
            }
            let st = self.inner.state.lock().expect("state mutex");
            if let Some(c) = st.conn.as_ref() {
                return Ok((c.send_request.clone(), c.generation));
            }
            if !st.connecting {
                if let Some(e) = st.last_error.as_ref() {
                    return Err(Error::Quic(e.clone()));
                }
            }
            // Another attempt is already running; wait for it.
        }
    }

    async fn attempt(&self, mut sr: SendRequest<OpenStreams, Bytes>, body: Bytes) -> Attempt {
        let mut req = http::Request::new(());
        *req.method_mut() = http::Method::POST;
        *req.uri_mut() = self.inner.uri.clone();
        let headers = req.headers_mut();
        headers.insert(http::header::HOST, self.inner.authority.clone());
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from(body.len()));

        let mut stream = match sr.send_request(req).await {
            Ok(s) => s,
            Err(e) => return Attempt::NotSent(Error::Quic(format!("open stream: {e}"))),
        };
        // From here the headers are on the wire: never retry.
        if let Err(e) = stream.send_data(body).await {
            return Attempt::Sent(Err(Error::Quic(format!("send body: {e}"))));
        }
        if let Err(e) = stream.finish().await {
            return Attempt::Sent(Err(Error::Quic(format!("finish: {e}"))));
        }
        let resp = match stream.recv_response().await {
            Ok(r) => r,
            Err(e) => return Attempt::Sent(Err(Error::Quic(format!("response: {e}")))),
        };
        let status = resp.status();
        let retry_after = retry_after(resp.headers());
        let mut buf = Vec::new();
        loop {
            match stream.recv_data().await {
                Ok(Some(mut chunk)) => {
                    while chunk.has_remaining() {
                        let c = chunk.chunk();
                        let room = MAX_RESPONSE_BODY.saturating_sub(buf.len());
                        buf.extend_from_slice(&c[..c.len().min(room)]);
                        let n = c.len();
                        chunk.advance(n);
                    }
                }
                Ok(None) => break,
                Err(e) => return Attempt::Sent(Err(Error::Quic(format!("body: {e}")))),
            }
        }
        if status.is_success() {
            Attempt::Sent(Ok(()))
        } else {
            let body = String::from_utf8_lossy(&buf).into_owned();
            Attempt::Sent(Err(Error::from_status(status, retry_after, body)))
        }
    }
}

/// The endpoint for `addr`'s address family, created on first use.
fn endpoint_for(inner: &Inner, addr: SocketAddr) -> Result<quinn::Endpoint> {
    let slot = usize::from(addr.is_ipv6());
    let mut endpoints = inner.endpoints.lock().expect("endpoints mutex");
    if let Some(ep) = endpoints[slot].as_ref() {
        return Ok(ep.clone());
    }
    let bind: SocketAddr = match addr.ip() {
        IpAddr::V4(_) => "0.0.0.0:0".parse().expect("static addr"),
        IpAddr::V6(_) => "[::]:0".parse().expect("static addr"),
    };
    let mut ep =
        quinn::Endpoint::client(bind).map_err(|e| Error::Quic(format!("udp bind {bind}: {e}")))?;
    ep.set_default_client_config(inner.client_config.clone());
    endpoints[slot] = Some(ep.clone());
    Ok(ep)
}

async fn connect(inner: &Inner, generation: u64) -> Result<Conn> {
    let target = format!("{}:{}", inner.host_for_lookup, inner.port);
    let addr = tokio::net::lookup_host(&target)
        .await
        .map_err(|e| Error::Quic(format!("dns: {e}")))?
        .next()
        .ok_or_else(|| Error::Quic("dns: no addresses".into()))?;
    let endpoint = endpoint_for(inner, addr)?;
    let quic = endpoint
        .connect(addr, &inner.server_name)
        .map_err(|e| Error::Quic(format!("connect: {e}")))?
        .await
        .map_err(|e| Error::Quic(format!("handshake: {e}")))?;
    let (mut driver, send_request) = h3::client::new(h3_quinn::Connection::new(quic.clone()))
        .await
        .map_err(|e| Error::Quic(format!("h3 handshake: {e}")))?;
    let driver = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
    });
    debug!(%addr, generation, "quic connected");
    Ok(Conn {
        quic,
        send_request,
        driver,
        generation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_requires_key_and_redacts() {
        assert!(matches!(
            QuicClient::builder().build(),
            Err(Error::Config(_))
        ));
        let b = QuicClient::builder().api_key("SECRETKEY");
        assert!(!format!("{b:?}").contains("SECRETKEY"));
        let c = b.region(Region::Tokyo).build().unwrap();
        assert!(!format!("{c:?}").contains("SECRETKEY"));
        assert_eq!(c.host(), "tyo1.nozomi.temporal.xyz");
        assert_eq!(c.inner.authority, "tyo1.nozomi.temporal.xyz");
        assert!(!c.is_connected());
    }

    #[test]
    fn builder_encodes_key_and_handles_ip_literals() {
        let c = QuicClient::builder()
            .api_key("ab c#d\n")
            .host("localhost")
            .port(4433)
            .build()
            .unwrap();
        assert_eq!(c.inner.authority, "localhost:4433");
        assert_eq!(c.inner.uri.query(), Some("c=ab%20c%23d%0A"));

        let c = QuicClient::builder()
            .api_key("k")
            .host("[fd00::10]")
            .build()
            .unwrap();
        assert_eq!(c.inner.server_name, "fd00::10");
        assert_eq!(c.host(), "[fd00::10]");
        assert_eq!(c.inner.authority, "[fd00::10]");
        let c = QuicClient::builder()
            .api_key("k")
            .host("fd00::10")
            .port(8443)
            .build()
            .unwrap();
        assert_eq!(c.inner.authority, "[fd00::10]:8443");
    }
}
