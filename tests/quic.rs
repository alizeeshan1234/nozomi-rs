//! The QUIC client against a local HTTP/3 server with a self-signed cert.
#![cfg(feature = "quic")]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Buf, Bytes};
use nozomi_client::{Client, Error, QuicClient};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

struct Received {
    path_and_query: String,
    content_type: String,
    body: Vec<u8>,
}

#[derive(Clone)]
struct Script {
    /// (status, body) per request, in order; 200 with empty body when exhausted.
    responses: Arc<Mutex<VecDeque<(u16, String)>>>,
    received: Arc<Mutex<Vec<Received>>>,
    /// While set, requests are read and then never answered.
    stall: Arc<std::sync::atomic::AtomicBool>,
    connections: Arc<std::sync::atomic::AtomicUsize>,
}

struct TestServer {
    endpoint: quinn::Endpoint,
    cert: CertificateDer<'static>,
    script: Script,
}

impl TestServer {
    fn start() -> Self {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert: CertificateDer<'static> = ck.cert.der().clone();
        let key = PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der());
        Self::start_on(0, cert, key)
    }

    fn start_on(
        port: u16,
        cert: CertificateDer<'static>,
        key: PrivatePkcs8KeyDer<'static>,
    ) -> Self {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key.clone_key().into())
            .unwrap();
        tls.alpn_protocols = vec![b"h3".to_vec()];
        let qsc = quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap();
        let cfg = quinn::ServerConfig::with_crypto(Arc::new(qsc));
        let endpoint =
            quinn::Endpoint::server(cfg, format!("127.0.0.1:{port}").parse().unwrap()).unwrap();

        let script = Script {
            responses: Arc::new(Mutex::new(VecDeque::new())),
            received: Arc::new(Mutex::new(Vec::new())),
            stall: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            connections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let ep = endpoint.clone();
        let sc = script.clone();
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let Ok(conn) = incoming.await else { continue };
                sc.connections
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let sc = sc.clone();
                tokio::spawn(async move {
                    let Ok(mut h3) =
                        h3::server::Connection::new(h3_quinn::Connection::new(conn)).await
                    else {
                        return;
                    };
                    loop {
                        let resolver = match h3.accept().await {
                            Ok(Some(r)) => r,
                            _ => break,
                        };
                        let sc = sc.clone();
                        tokio::spawn(async move {
                            let Ok((req, mut stream)) = resolver.resolve_request().await else {
                                return;
                            };
                            let mut body = Vec::new();
                            while let Ok(Some(mut chunk)) = stream.recv_data().await {
                                while chunk.has_remaining() {
                                    let c = chunk.chunk();
                                    body.extend_from_slice(c);
                                    let n = c.len();
                                    chunk.advance(n);
                                }
                            }
                            sc.received.lock().unwrap().push(Received {
                                path_and_query: req
                                    .uri()
                                    .path_and_query()
                                    .map(|p| p.to_string())
                                    .unwrap_or_default(),
                                content_type: req
                                    .headers()
                                    .get("content-type")
                                    .and_then(|v| v.to_str().ok())
                                    .unwrap_or_default()
                                    .to_string(),
                                body,
                            });
                            if sc.stall.load(std::sync::atomic::Ordering::SeqCst) {
                                tokio::time::sleep(Duration::from_secs(30)).await;
                                return;
                            }
                            let (status, text) = sc
                                .responses
                                .lock()
                                .unwrap()
                                .pop_front()
                                .unwrap_or((200, String::new()));
                            let mut resp = http::Response::builder().status(status);
                            if status == 429 {
                                resp = resp.header("retry-after", "3");
                            }
                            let resp = resp.body(()).unwrap();
                            let _ = stream.send_response(resp).await;
                            if !text.is_empty() {
                                let _ = stream.send_data(Bytes::from(text)).await;
                            }
                            let _ = stream.finish().await;
                        });
                    }
                });
            }
        });
        Self {
            endpoint,
            cert,
            script,
        }
    }

    fn port(&self) -> u16 {
        self.endpoint.local_addr().unwrap().port()
    }

    fn client(&self) -> QuicClient {
        QuicClient::builder()
            .api_key("SECRETKEY")
            .host("localhost")
            .port(self.port())
            .timeout(Duration::from_secs(3))
            .root_certificate(self.cert.clone())
            .build()
            .unwrap()
    }

    fn respond(&self, status: u16, body: &str) {
        self.script
            .responses
            .lock()
            .unwrap()
            .push_back((status, body.to_string()));
    }

    fn received(&self) -> usize {
        self.script.received.lock().unwrap().len()
    }
}

fn load(c: &QuicClient, f: impl Fn(&nozomi_client::Stats) -> &std::sync::atomic::AtomicU64) -> u64 {
    f(c.stats()).load(std::sync::atomic::Ordering::Relaxed)
}

#[tokio::test]
async fn send_batch_over_h3_with_key_and_framing() {
    let server = TestServer::start();
    let c = server.client();
    assert!(!c.is_connected());
    c.warmup().await.unwrap();
    assert!(c.is_connected());

    let a = vec![1u8; 66];
    let b = vec![2u8; 300];
    c.send_batch(&[&a, &b]).await.unwrap();

    let got = server.script.received.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].path_and_query, "/api/sendBatch?c=SECRETKEY");
    assert_eq!(got[0].content_type, "application/octet-stream");
    assert_eq!(got[0].body, Client::frame_batch(&[&a, &b]).unwrap());
    drop(got);

    assert_eq!(load(&c, |s| &s.submitted), 2);
    assert_eq!(load(&c, |s| &s.accepted), 2);
    assert_eq!(load(&c, |s| &s.rejected_by_server), 0);
}

#[tokio::test]
async fn first_send_connects_lazily_and_maps_statuses() {
    let server = TestServer::start();
    let c = server.client();
    let tx = vec![3u8; 100];

    server.respond(401, "Unauthorized");
    assert!(matches!(c.send(&tx).await, Err(Error::Unauthorized)));
    assert!(c.is_connected());

    server.respond(429, "");
    assert!(matches!(
        c.send(&tx).await,
        Err(Error::RateLimited {
            retry_after: Some(3)
        })
    ));
    server.respond(400, "insufficient tip");
    assert!(matches!(c.send(&tx).await, Err(Error::BadRequest(ref b)) if b == "insufficient tip"));
    server.respond(503, "down");
    assert!(matches!(
        c.send(&tx).await,
        Err(Error::Server { status: 503, .. })
    ));
    server.respond(200, "ok");
    c.send(&tx).await.unwrap();

    assert_eq!(server.received(), 5);
    assert_eq!(load(&c, |s| &s.submitted), 5);
    assert_eq!(load(&c, |s| &s.accepted), 1);
    assert_eq!(load(&c, |s| &s.rejected_by_server), 4);
    assert_eq!(load(&c, |s| &s.transport_errors), 0);
    // Every request that got a response counts toward the round-trip mean.
    assert_eq!(load(&c, |s| &s.roundtrips), 5);
    assert!(c.stats().mean_roundtrip_micros() > 0);
}

#[tokio::test]
async fn local_limits_reject_without_sending() {
    let server = TestServer::start();
    let c = server.client();
    let big = vec![0u8; 1233];
    assert!(matches!(
        c.send(&big).await,
        Err(Error::TransactionTooLarge { .. })
    ));
    let ok = vec![0u8; 100];
    let seventeen: Vec<&[u8]> = (0..17).map(|_| ok.as_slice()).collect();
    assert!(matches!(
        c.send_batch(&seventeen).await,
        Err(Error::BatchTooLarge { count: 17, .. })
    ));
    let huge = Bytes::from(vec![0u8; nozomi_client::MAX_BATCH_BODY_BYTES + 1]);
    assert!(matches!(
        c.send_raw(huge, 1).await,
        Err(Error::BatchBodyTooLarge { .. })
    ));
    assert!(matches!(
        c.send_raw(Bytes::new(), 0).await,
        Err(Error::EmptyBatch)
    ));
    assert_eq!(load(&c, |s| &s.rejected_locally), 4);
    assert_eq!(load(&c, |s| &s.submitted), 0);
    assert_eq!(server.received(), 0);
    assert!(!c.is_connected());
}

#[tokio::test]
async fn send_raw_reuses_preframed_body() {
    let server = TestServer::start();
    let c = server.client();
    let tx = vec![9u8; 200];
    let body = Bytes::from(Client::frame_batch(&[&tx]).unwrap());
    for _ in 0..3 {
        c.send_raw(body.clone(), 1).await.unwrap();
    }
    assert_eq!(server.received(), 3);
    assert_eq!(load(&c, |s| &s.accepted), 3);
    let got = server.script.received.lock().unwrap();
    assert!(got.iter().all(|r| r.body == body));
}

#[tokio::test]
async fn reconnects_after_disconnect_and_after_server_restart() {
    let server = TestServer::start();
    let c = server.client();
    let tx = vec![4u8; 100];
    c.send(&tx).await.unwrap();
    c.disconnect();
    assert!(!c.is_connected());
    c.send(&tx).await.unwrap();
    assert_eq!(server.received(), 2);

    // Kill every server-side connection; the client's next send must notice
    // the dead connection and reconnect rather than fail.
    server.endpoint.close(0u32.into(), b"restart");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let server2 = TestServer::start();
    let c2 = QuicClient::builder()
        .api_key("SECRETKEY")
        .host("localhost")
        .port(server2.port())
        .timeout(Duration::from_secs(3))
        .root_certificate(server2.cert.clone())
        .build()
        .unwrap();
    c2.send(&tx).await.unwrap();
    assert_eq!(server2.received(), 1);
}

#[tokio::test]
async fn dead_connection_is_replaced_transparently() {
    let server = TestServer::start();
    let c = server.client();
    let tx = vec![5u8; 100];
    c.send(&tx).await.unwrap();
    // The server closes its endpoint for good, so this send has nowhere to
    // go; it must surface as a transport error, not hang or panic, must not
    // count as accepted, and must leave the client disconnected.
    server.endpoint.close(0u32.into(), b"bye");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let err = c.send(&tx).await.unwrap_err();
    assert!(
        matches!(err, Error::Quic(_) | Error::Timeout { .. }),
        "unexpected {err:?}"
    );
    assert_eq!(load(&c, |s| &s.accepted), 1);
    assert_eq!(load(&c, |s| &s.transport_errors), 1);
    assert!(!format!("{err}").contains("SECRETKEY"));
    assert!(!c.is_connected());
}

/// The path goes dark without a CONNECTION_CLOSE: the server reads the
/// request and never answers. The send must time out, the connection must be
/// evicted, and the next send must go out on a fresh connection.
#[tokio::test]
async fn timed_out_connection_is_evicted_and_replaced() {
    let server = TestServer::start();
    let c = QuicClient::builder()
        .api_key("SECRETKEY")
        .host("localhost")
        .port(server.port())
        .timeout(Duration::from_millis(300))
        .root_certificate(server.cert.clone())
        .build()
        .unwrap();
    let tx = vec![6u8; 100];
    c.send(&tx).await.unwrap();
    assert_eq!(
        server
            .script
            .connections
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );

    server
        .script
        .stall
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let started = std::time::Instant::now();
    let err = c.send(&tx).await.unwrap_err();
    assert!(matches!(err, Error::Timeout { .. }), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!c.is_connected(), "dead connection must be evicted");
    assert_eq!(load(&c, |s| &s.transport_errors), 1);

    server
        .script
        .stall
        .store(false, std::sync::atomic::Ordering::SeqCst);
    c.send(&tx).await.unwrap();
    assert!(c.is_connected());
    assert_eq!(
        server
            .script
            .connections
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(load(&c, |s| &s.accepted), 2);
}

/// Many senders during an outage must not each restart the handshake; the
/// connect is single-flighted on its own task and survives caller timeouts.
#[tokio::test]
async fn burst_during_outage_recovers_once_server_returns() {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = sock.local_addr().unwrap().port();
    drop(sock);
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert: CertificateDer<'static> = ck.cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der());

    let c = QuicClient::builder()
        .api_key("SECRETKEY")
        .host("localhost")
        .port(port)
        .timeout(Duration::from_millis(250))
        .root_certificate(cert.clone())
        .build()
        .unwrap();

    // Nothing is listening for the first 600 ms; senders time out meanwhile.
    let c2 = c.clone();
    let bringup = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(600)).await;
        TestServer::start_on(port, cert, key)
    });
    let tx = vec![7u8; 100];
    let mut ok = 0;
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(6) {
        if c2.send(&tx).await.is_ok() {
            ok += 1;
            if ok >= 3 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let server = bringup.await.unwrap();
    assert!(
        ok >= 3,
        "never recovered: {ok} successes, {} received",
        server.received()
    );
}

#[tokio::test]
async fn timeout_when_nothing_listens() {
    // A bound UDP socket that never answers: the QUIC handshake stalls until the
    // client's own timeout fires.
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = sock.local_addr().unwrap().port();
    let c = QuicClient::builder()
        .api_key("SECRETKEY")
        .host("localhost")
        .port(port)
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let started = std::time::Instant::now();
    let err = c.send(&[0u8; 100]).await.unwrap_err();
    assert!(matches!(err, Error::Timeout { .. }), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(load(&c, |s| &s.transport_errors), 1);
}
