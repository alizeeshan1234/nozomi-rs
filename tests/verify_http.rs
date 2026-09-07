//! The verifier's RPC layer against a mock Solana RPC.

use std::time::Duration;

use nozomi_client::{Error, Verifier, TIP_ACCOUNTS};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn rpc_ok(result: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "result": result
    }))
}

fn keyed_url(server: &MockServer) -> String {
    format!("{}/?api-key=SECRETKEY", server.uri())
}

#[tokio::test]
async fn current_slot_and_signatures_for_send_documented_params() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(serde_json::json!({
            "method": "getSlot", "params": [{ "commitment": "confirmed" }]
        })))
        .respond_with(rpc_ok(serde_json::json!(444_059_106u64)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(serde_json::json!({
            "method": "getSignaturesForAddress",
            "params": [TIP_ACCOUNTS[0], { "limit": 1000, "commitment": "confirmed", "before": "prev" }]
        })))
        .respond_with(rpc_ok(serde_json::json!([
            { "signature": "s1", "slot": 10, "err": null, "blockTime": 1700 },
            { "signature": "s2", "slot": 9, "err": { "InstructionError": [1, "Custom"] } }
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let v = Verifier::new(keyed_url(&server));
    assert_eq!(v.current_slot().await.unwrap(), 444_059_106);
    // limit is capped at the RPC maximum of 1000
    let sigs = v
        .signatures_for(TIP_ACCOUNTS[0], 5000, Some("prev"))
        .await
        .unwrap();
    assert_eq!(sigs.len(), 2);
    assert!(sigs[0].succeeded());
    assert!(!sigs[1].succeeded());
    assert_eq!(sigs[1].block_time, None);
}

#[tokio::test]
async fn report_fetches_jsonparsed_and_maps_not_found() {
    let server = MockServer::start().await;
    let tx = serde_json::json!({
        "slot": 100, "blockTime": 1700000000,
        "meta": { "err": null, "fee": 5000, "computeUnitsConsumed": 1200, "innerInstructions": [],
                  "preBalances": [10_000_000, 500], "postBalances": [10_000_000 - 5000 - 1_500_000, 500 + 1_500_000] },
        "transaction": { "message": {
            "accountKeys": [ { "pubkey": "Payer111111111111111111111111111111111111111" }, { "pubkey": TIP_ACCOUNTS[3] } ],
            "instructions": [ { "program": "system", "programId": "11111111111111111111111111111111",
                "parsed": { "type": "transfer", "info": { "source": "Payer", "destination": TIP_ACCOUNTS[3], "lamports": 1500000 } } } ]
        } }
    });
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_partial_json(serde_json::json!({
            "method": "getTransaction",
            "params": ["sig1", { "encoding": "jsonParsed", "commitment": "confirmed", "maxSupportedTransactionVersion": 0 }]
        })))
        .respond_with(rpc_ok(tx))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            serde_json::json!({ "method": "getTransaction", "params": ["missing"] }),
        ))
        .respond_with(rpc_ok(serde_json::Value::Null))
        .mount(&server)
        .await;

    let v = Verifier::new(keyed_url(&server));
    let r = v.report("sig1", Some(98)).await.unwrap();
    assert!(r.succeeded);
    assert_eq!(r.tip_paid_lamports, 1_500_000);
    assert_eq!(r.slots_after_submit, Some(2));
    assert!(matches!(
        v.report("missing", None).await,
        Err(Error::NotFound(ref s)) if s == "missing"
    ));
}

#[tokio::test]
async fn rpc_error_http_error_and_decode_error_hide_the_key() {
    let server = MockServer::start().await;
    let v = Verifier::new(keyed_url(&server));

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "error": { "code": -32602, "message": "Invalid param" }
        })))
        .mount(&server)
        .await;
    let e = v.current_slot().await.unwrap_err();
    assert!(matches!(e, Error::Rpc { code: -32602, .. }), "{e:?}");

    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
        .mount(&server)
        .await;
    let e = v.current_slot().await.unwrap_err();
    assert!(matches!(e, Error::Http { status: 429, .. }), "{e:?}");

    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
        .mount(&server)
        .await;
    let e = v.current_slot().await.unwrap_err();
    assert!(matches!(e, Error::Decode(_)), "{e:?}");
    assert!(!format!("{e}").contains("SECRETKEY"), "{e}");
    assert!(!format!("{v:?}").contains("SECRETKEY"));
}

#[tokio::test]
async fn transport_error_and_timeout_hide_the_key() {
    let v = Verifier::with_timeout(
        "http://127.0.0.1:9/?api-key=SECRETKEY",
        Duration::from_secs(2),
    );
    let e = v.current_slot().await.unwrap_err();
    assert!(matches!(e, Error::Transport(_)), "{e:?}");
    assert!(!format!("{e}").contains("SECRETKEY"));
    assert!(!format!("{e:?}").contains("SECRETKEY"));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(rpc_ok(serde_json::json!(1)).set_delay(Duration::from_millis(500)))
        .mount(&server)
        .await;
    let v = Verifier::with_timeout(keyed_url(&server), Duration::from_millis(100));
    let e = v.current_slot().await.unwrap_err();
    assert!(
        matches!(e, Error::Timeout { after: Some(d) } if d == Duration::from_millis(100)),
        "{e:?}"
    );
    assert!(!format!("{e:?}").contains("SECRETKEY"));

    // A caller-supplied client: the crate does not know its timeout.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    let v = Verifier::with_client(http, keyed_url(&server));
    let e = v.current_slot().await.unwrap_err();
    assert!(matches!(e, Error::Timeout { after: None }), "{e:?}");
}
