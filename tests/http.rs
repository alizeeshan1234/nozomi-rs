//! Every HTTP path in the client, against a mock server.

use std::time::Duration;

use nozomi_client::{Client, Error};
use wiremock::matchers::{body_string, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Client {
    Client::builder()
        .api_key("SECRETKEY")
        .base_url(server.uri())
        .tip_api_base(server.uri())
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}

fn tx(n: usize) -> Vec<u8> {
    vec![7u8; n]
}

#[tokio::test]
async fn send_posts_base64_with_key_and_counts_accepted() {
    let server = MockServer::start().await;
    let bytes = tx(100);
    let expected = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    };
    Mock::given(method("POST"))
        .and(path("/api/sendTransaction2"))
        .and(query_param("c", "SECRETKEY"))
        .and(header("content-type", "text/plain"))
        .and(body_string(expected))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server);
    c.send(&bytes).await.unwrap();

    let s = c.stats();
    assert_eq!(s.submitted.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(s.accepted.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(s.roundtrips.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(
        s.rejected_by_server
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[tokio::test]
async fn send_rejects_bad_sizes_locally_without_a_request() {
    let server = MockServer::start().await;
    // No mock mounted: any request would 404 and fail the assertions below.
    let c = client(&server);
    assert!(matches!(
        c.send(&tx(1233)).await,
        Err(Error::TransactionTooLarge { size: 1233, .. })
    ));
    assert!(matches!(
        c.send(&tx(65)).await,
        Err(Error::TransactionTooSmall { size: 65, .. })
    ));
    assert_eq!(
        c.stats()
            .rejected_locally
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );
    assert_eq!(
        c.stats()
            .submitted
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn status_codes_map_to_typed_errors() {
    let server = MockServer::start().await;
    let c = client(&server);

    let cases: Vec<(u16, Option<&str>)> = vec![
        (401, None),
        (429, Some("7")),
        (400, Some("insufficient tip")),
        (503, Some("upstream down")),
        (418, Some("teapot")),
    ];
    for (status, extra) in cases {
        server.reset().await;
        let mut resp = ResponseTemplate::new(status);
        match (status, extra) {
            (429, Some(secs)) => resp = resp.insert_header("retry-after", secs),
            (_, Some(body)) => resp = resp.set_body_string(body),
            _ => {}
        }
        Mock::given(method("POST"))
            .and(path("/api/sendTransaction2"))
            .respond_with(resp)
            .mount(&server)
            .await;
        let err = c.send(&tx(100)).await.unwrap_err();
        match status {
            401 => assert!(matches!(err, Error::Unauthorized)),
            429 => assert!(matches!(
                err,
                Error::RateLimited {
                    retry_after: Some(7)
                }
            )),
            400 => assert!(matches!(err, Error::BadRequest(ref b) if b == "insufficient tip")),
            503 => assert!(
                matches!(err, Error::Server { status: 503, ref body } if body == "upstream down")
            ),
            418 => assert!(matches!(err, Error::Http { status: 418, .. })),
            _ => unreachable!(),
        }
    }
    let s = c.stats();
    assert_eq!(
        s.rejected_by_server
            .load(std::sync::atomic::Ordering::Relaxed),
        5
    );
    assert_eq!(s.accepted.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(s.submitted.load(std::sync::atomic::Ordering::Relaxed), 5);
    assert_eq!(s.roundtrips.load(std::sync::atomic::Ordering::Relaxed), 5);
}

#[tokio::test]
async fn send_rpc_returns_signature_and_maps_rpc_error() {
    let server = MockServer::start().await;
    let c = client(&server);

    Mock::given(method("POST"))
        .and(path("/"))
        .and(query_param("c", "SECRETKEY"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "result": "5sig"
        })))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(c.send_rpc(&tx(100)).await.unwrap(), "5sig");

    let body: serde_json::Value =
        serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(body["method"], "sendTransaction");
    assert_eq!(body["params"][1]["encoding"], "base64");

    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "error": { "code": -32003, "message": "Transaction signature verification failure" }
        })))
        .mount(&server)
        .await;
    let err = c.send_rpc(&tx(100)).await.unwrap_err();
    assert!(matches!(err, Error::Rpc { code: -32003, .. }), "{err}");

    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;
    let err = c.send_rpc(&tx(100)).await.unwrap_err();
    assert!(matches!(err, Error::Decode(_)), "{err}");
    assert!(!format!("{err}").contains("SECRETKEY"));
}

#[tokio::test]
async fn send_batch_frames_body_and_sets_octet_stream() {
    let server = MockServer::start().await;
    let c = client(&server);
    let a = tx(66);
    let b = tx(300);
    let expected = Client::frame_batch(&[&a, &b]).unwrap();
    Mock::given(method("POST"))
        .and(path("/api/sendBatch"))
        .and(query_param("c", "SECRETKEY"))
        .and(header("content-type", "application/octet-stream"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    c.send_batch(&[&a, &b]).await.unwrap();
    let got = &server.received_requests().await.unwrap()[0].body;
    assert_eq!(got, &expected);
    let s = c.stats();
    assert_eq!(s.submitted.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert_eq!(s.accepted.load(std::sync::atomic::Ordering::Relaxed), 2);

    // Local rejection counts one call, whatever the batch size, and sends nothing.
    let seventeen: Vec<&[u8]> = (0..17).map(|_| a.as_slice()).collect();
    assert!(matches!(
        c.send_batch(&seventeen).await,
        Err(Error::BatchTooLarge { count: 17, .. })
    ));
    assert!(matches!(c.send_batch(&[]).await, Err(Error::EmptyBatch)));
    assert_eq!(
        s.rejected_locally
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn ping_hits_ping_path_without_key() {
    let server = MockServer::start().await;
    let c = client(&server);
    Mock::given(method("GET"))
        .and(path("/ping"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    c.ping().await.unwrap();
    let req = &server.received_requests().await.unwrap()[0];
    assert!(req.url.query().is_none());

    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/ping"))
        .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
        .mount(&server)
        .await;
    assert!(matches!(
        c.ping().await,
        Err(Error::Server { status: 502, .. })
    ));
}

#[tokio::test]
async fn keepalive_pings_on_interval_and_stops_on_drop() {
    let server = MockServer::start().await;
    let c = client(&server);
    Mock::given(method("GET"))
        .and(path("/ping"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let ka = c.spawn_keepalive_every(Duration::from_millis(30));
    tokio::time::sleep(Duration::from_millis(110)).await;
    let n = server.received_requests().await.unwrap().len();
    assert!(n >= 3, "expected >= 3 pings, got {n}");
    drop(ka);
    tokio::time::sleep(Duration::from_millis(80)).await;
    let after = server.received_requests().await.unwrap().len();
    assert!(after <= n + 1, "pings continued after drop: {n} -> {after}");
}

#[tokio::test]
async fn tip_floor_parses_array_response() {
    let server = MockServer::start().await;
    let c = client(&server);
    Mock::given(method("GET"))
        .and(path("/tip_floor"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "time": "2026-09-04T00:00:00Z",
                "landed_tips_25th_percentile": 0.0011,
                "landed_tips_50th_percentile": 0.005,
                "landed_tips_75th_percentile": 0.011,
                "landed_tips_95th_percentile": 0.085,
                "landed_tips_99th_percentile": 0.2
            }])),
        )
        .mount(&server)
        .await;
    let floor = c.tip_floor().await.unwrap();
    assert_eq!(floor.landed_tips_50th_percentile, Some(0.005));
    assert_eq!(
        floor.lamports(nozomi_client::tipfloor::Percentile::P75),
        11_000_000
    );

    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/tip_floor"))
        .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
        .mount(&server)
        .await;
    assert!(matches!(
        c.tip_floor().await,
        Err(Error::Server { status: 500, .. })
    ));
}

#[tokio::test]
async fn transport_error_counts_and_hides_key() {
    // Nothing listens on this port; connection refused.
    let c = Client::builder()
        .api_key("SECRETKEY")
        .base_url("http://127.0.0.1:9")
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let err = c.send(&tx(100)).await.unwrap_err();
    assert!(matches!(err, Error::Transport(_)), "{err}");
    assert!(!format!("{err}").contains("SECRETKEY"), "{err}");
    assert!(!format!("{err:?}").contains("SECRETKEY"), "{err:?}");
    assert_eq!(
        c.stats()
            .transport_errors
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(
        c.stats()
            .roundtrips
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[cfg(feature = "solana")]
#[tokio::test]
async fn send_transaction_checks_tip_then_sends() {
    use solana_message::{Message, VersionedMessage};
    use solana_pubkey::Pubkey;
    use solana_transaction::versioned::VersionedTransaction;

    let server = MockServer::start().await;
    let c = client(&server);
    Mock::given(method("POST"))
        .and(path("/api/sendTransaction2"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let payer = Pubkey::new_unique();
    let build = |lamports: u64| VersionedTransaction {
        signatures: vec![Default::default()],
        message: VersionedMessage::Legacy(Message::new(
            &[nozomi_client::tip::tip_instruction(&payer, lamports)],
            Some(&payer),
        )),
    };

    assert!(matches!(
        c.send_transaction(&build(999_999)).await,
        Err(Error::TipBelowMinimum { .. })
    ));
    assert!(server.received_requests().await.unwrap().is_empty());

    c.send_transaction(&build(1_000_000)).await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    // Unchecked path sends the under-tipped transaction anyway.
    c.send_transaction_unchecked(&build(1)).await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn slow_server_is_a_timeout_not_a_transport_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/sendTransaction2"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(400)))
        .mount(&server)
        .await;
    let c = Client::builder()
        .api_key("SECRETKEY")
        .base_url(server.uri())
        .timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    let err = c.send(&tx(100)).await.unwrap_err();
    assert!(
        matches!(err, Error::Timeout { after: Some(d) } if d == Duration::from_millis(100)),
        "{err:?}"
    );
    assert!(!format!("{err:?}").contains("SECRETKEY"));
    assert_eq!(
        c.stats()
            .transport_errors
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}
