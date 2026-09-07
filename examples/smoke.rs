//! Live smoke test against Nozomi. Needs no API key for most checks; set
//! NOZOMI_API_KEY to also exercise authenticated sends (a deliberately
//! malformed transaction, expected to come back as a 400).
//!
//! cargo run --example smoke --all-features
use std::time::{Duration, Instant};

use nozomi_client::{Client, Error, Region, Route};

fn row(name: &str, started: Instant, outcome: String) {
    println!(
        "{name:<48} {:>7} ms  {outcome}",
        started.elapsed().as_millis()
    );
}

#[tokio::main]
async fn main() {
    let key = std::env::var("NOZOMI_API_KEY").ok();
    let auth = key.clone().unwrap_or_else(|| "no-key".into());
    let garbage = vec![0u8; 100];

    println!("== tip floor (REST, no key) ==");
    let c = Client::builder().api_key(&auth).build().unwrap();
    let t = Instant::now();
    match c.tip_floor().await {
        Ok(f) => row(
            "api.nozomi.temporal.xyz/tip_floor",
            t,
            format!(
                "p50 {:?} SOL (complete: {})",
                f.landed_tips_50th_percentile,
                f.is_complete()
            ),
        ),
        Err(e) => row("api.nozomi.temporal.xyz/tip_floor", t, format!("ERR {e}")),
    }

    println!("\n== /ping on every region and route ==");
    let mut targets = vec![(Region::Auto, Route::Cloudflare)];
    for &r in Region::ALL {
        targets.push((r, Route::Cloudflare));
        targets.push((r, Route::Direct { tls: false }));
        targets.push((r, Route::Direct { tls: true }));
    }
    for (region, route) in targets {
        let c = Client::builder()
            .api_key(&auth)
            .region(region)
            .route(route)
            .timeout(Duration::from_secs(8))
            .build()
            .unwrap();
        let t = Instant::now();
        let out = match c.ping().await {
            Ok(()) => "200".to_string(),
            Err(e) => format!("ERR {e}"),
        };
        row(c.base_url(), t, out);
    }

    println!(
        "\n== sendTransaction2 over Cloudflare auto ({}): malformed tx ==",
        if key.is_some() {
            "real key"
        } else {
            "no key, expect 401"
        }
    );
    let c = Client::builder().api_key(&auth).build().unwrap();
    let t = Instant::now();
    row(
        "POST /api/sendTransaction2",
        t,
        describe(c.send(&garbage).await),
    );
    let t = Instant::now();
    row(
        "POST /api/sendBatch",
        t,
        describe(c.send_batch(&[&garbage]).await),
    );
    let t = Instant::now();
    row(
        "POST / (json-rpc sendTransaction)",
        t,
        describe(c.send_rpc(&garbage).await.map(|_| ())),
    );

    #[cfg(feature = "quic")]
    {
        println!("\n== HTTP/3 over QUIC: handshake + sendBatch with malformed tx ==");
        for region in std::iter::once(Region::Auto).chain(Region::ALL.iter().copied()) {
            let q = nozomi_client::QuicClient::builder()
                .api_key(&auth)
                .region(region)
                .timeout(Duration::from_secs(8))
                .build()
                .unwrap();
            let t = Instant::now();
            let hs = match q.warmup().await {
                Ok(()) => format!("handshake {} ms", t.elapsed().as_millis()),
                Err(e) => {
                    row(q.host(), t, format!("ERR {e}"));
                    continue;
                }
            };
            let t2 = Instant::now();
            let out = describe(q.send(&garbage).await);
            row(q.host(), t2, format!("{hs}; send: {out}"));
        }
    }

    #[cfg(feature = "tip-stream")]
    {
        println!("\n== tip stream (websocket) ==");
        for url in [
            nozomi_client::tipstream::TIP_STREAM_URL.to_string(),
            format!("{}?c={auth}", nozomi_client::tipstream::TIP_STREAM_URL),
        ] {
            let label = if url.contains("?c=") {
                "wss .../tip_stream?c=<key>"
            } else {
                "wss .../tip_stream (no key)"
            };
            let t = Instant::now();
            match nozomi_client::TipStream::connect(&url).await {
                Ok(mut s) => {
                    match tokio::time::timeout(Duration::from_secs(20), s.next()).await {
                        Ok(Ok(Some(f))) => row(
                            label,
                            t,
                            format!("first frame p50 {:?} SOL", f.landed_tips_50th_percentile),
                        ),
                        Ok(Ok(None)) => row(label, t, "closed before first frame".into()),
                        Ok(Err(e)) => row(label, t, format!("ERR {e}")),
                        Err(_) => row(label, t, "connected, no frame within 20 s".into()),
                    }
                    let _ = s.close().await;
                }
                Err(e) => row(label, t, format!("ERR {e}")),
            }
        }
    }
}

fn describe(r: Result<(), Error>) -> String {
    match r {
        Ok(()) => "200 accepted".into(),
        Err(Error::Unauthorized) => "401 Unauthorized (typed)".into(),
        Err(Error::BadRequest(b)) => format!("400 BadRequest: {b}"),
        Err(e) => format!("ERR {e}"),
    }
}
