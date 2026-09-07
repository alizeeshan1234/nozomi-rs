//! The tip stream against a local websocket server.
#![cfg(feature = "tip-stream")]

use futures_util::{SinkExt, StreamExt};
use nozomi_client::tipfloor::Percentile;
use nozomi_client::{Client, Error, TipStream};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

fn frame(p50: f64) -> String {
    serde_json::json!([{
        "time": "2026-09-07T00:00:00Z",
        "landed_tips_25th_percentile": 0.001,
        "landed_tips_50th_percentile": p50,
        "landed_tips_75th_percentile": 0.011,
        "landed_tips_95th_percentile": 0.085,
        "landed_tips_99th_percentile": 0.2
    }])
    .to_string()
}

/// Serve one connection: record the request path, send `frames`, then close.
async fn serve_once(frames: Vec<Message>) -> (u16, tokio::sync::oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut path = String::new();
        let ws = tokio_tungstenite::accept_hdr_async(
            stream,
            |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
             resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                path = req.uri().to_string();
                Ok(resp)
            },
        )
        .await
        .unwrap();
        let _ = tx.send(path);
        let (mut sink, mut source) = ws.split();
        for f in frames {
            sink.send(f).await.unwrap();
        }
        sink.send(Message::Close(None)).await.unwrap();
        // Drain until the peer closes so the close handshake completes.
        while let Some(Ok(_)) = source.next().await {}
    });
    (port, rx)
}

#[tokio::test]
async fn parses_frames_skips_noise_and_ends_on_close() {
    let (port, path) = serve_once(vec![
        Message::Text(frame(0.005).into()),
        Message::Ping(vec![1].into()),
        Message::Text("not json".into()),
        Message::Text(serde_json::json!({"unexpected": true}).to_string().into()),
        Message::Binary(frame(0.007).into_bytes().into()),
    ])
    .await;
    let mut s = TipStream::connect(&format!("ws://127.0.0.1:{port}/tip_stream"))
        .await
        .unwrap();
    assert_eq!(path.await.unwrap(), "/tip_stream");

    let a = s.next().await.unwrap().unwrap();
    assert_eq!(a.lamports(Percentile::P50), 5_000_000);
    let b = s.next().await.unwrap().unwrap();
    assert_eq!(b.lamports(Percentile::P50), 7_000_000);
    assert!(s.next().await.unwrap().is_none());
}

#[tokio::test]
async fn client_tip_stream_uses_api_host_and_key() {
    let (port, path) = serve_once(vec![Message::Text(frame(0.002).into())]).await;
    let c = Client::builder()
        .api_key("SECRETKEY")
        .tip_api_base(format!("http://127.0.0.1:{port}"))
        .build()
        .unwrap();
    let mut s = c.tip_stream().await.unwrap();
    assert_eq!(path.await.unwrap(), "/tip_stream?c=SECRETKEY");
    let f = s.next().await.unwrap().unwrap();
    assert_eq!(f.lamports(Percentile::P50), 2_000_000);
    s.close().await.unwrap();
}

#[tokio::test]
async fn connect_failure_is_typed_and_hides_url() {
    let err = TipStream::connect("ws://127.0.0.1:9/tip_stream?c=SECRETKEY")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::WebSocket(_)), "{err:?}");
    assert!(!format!("{err}").contains("SECRETKEY"));
    assert!(!format!("{err:?}").contains("SECRETKEY"));
}

#[tokio::test]
async fn http_rejection_is_typed_and_hides_url() {
    // A plain HTTP server that refuses the upgrade.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = s.read(&mut buf).await;
        let _ = s
            .write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n")
            .await;
    });
    let err = TipStream::connect(&format!("ws://127.0.0.1:{port}/tip_stream?c=SECRETKEY"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::WebSocket(ref m) if m.contains("401")),
        "{err:?}"
    );
    assert!(!format!("{err}").contains("SECRETKEY"));
}

/// Serve `rounds` connections in a row, each sending one frame with the given
/// p50 and then closing. Returns the port and a counter of accepted connections.
async fn serve_rounds(p50s: Vec<f64>) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = accepted.clone();
    tokio::spawn(async move {
        for p50 in p50s {
            let (stream, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut sink, mut source) = ws.split();
            sink.send(Message::Text(frame(p50).into())).await.unwrap();
            sink.send(Message::Close(None)).await.unwrap();
            while let Some(Ok(_)) = source.next().await {}
        }
        // Then stop accepting: connects fail until the watcher is dropped.
    });
    (port, accepted)
}

#[tokio::test]
async fn watch_reconnects_and_keeps_last_frame() {
    let (port, accepted) = serve_rounds(vec![0.003, 0.004]).await;
    let mut w = TipStream::watch(format!("ws://127.0.0.1:{port}/tip_stream"));
    assert!(w.latest().is_none());

    let first = tokio::time::timeout(std::time::Duration::from_secs(5), w.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.lamports(Percentile::P50), 3_000_000);
    // Server closed; watcher reconnects at once and gets the second frame.
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), w.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.lamports(Percentile::P50), 4_000_000);
    assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 2);

    // Nothing more will ever arrive, but the last frame stays readable.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(w.latest().unwrap().lamports(Percentile::P50), 4_000_000);
    let mut rx = w.subscribe();
    assert_eq!(
        rx.borrow_and_update()
            .as_ref()
            .unwrap()
            .lamports(Percentile::P50),
        4_000_000
    );
    drop(w);
    // Dropping the watch ends the task; the receiver sees the sender go away.
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.changed())
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn client_tip_watch_uses_key() {
    let (port, path) = serve_once(vec![Message::Text(frame(0.006).into())]).await;
    let c = Client::builder()
        .api_key("SECRETKEY")
        .tip_api_base(format!("http://127.0.0.1:{port}"))
        .build()
        .unwrap();
    let mut w = c.tip_watch();
    assert_eq!(path.await.unwrap(), "/tip_stream?c=SECRETKEY");
    let f = tokio::time::timeout(std::time::Duration::from_secs(5), w.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.lamports(Percentile::P50), 6_000_000);
}

#[tokio::test]
async fn silent_server_is_a_read_timeout() {
    // Accepts the upgrade and then says nothing.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        drop(ws);
    });
    let mut s = TipStream::connect_with(
        &format!("ws://127.0.0.1:{port}/tip_stream"),
        std::time::Duration::from_secs(2),
        std::time::Duration::from_millis(200),
    )
    .await
    .unwrap();
    let started = std::time::Instant::now();
    let err = s.next().await.unwrap_err();
    assert!(matches!(err, Error::Timeout { .. }), "{err:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

#[tokio::test]
async fn watch_recovers_from_a_silent_connection() {
    // First connection: silent. Second: one frame.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let silent = tokio_tungstenite::accept_async(stream).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let (mut sink, mut source) = ws.split();
        sink.send(Message::Text(frame(0.009).into())).await.unwrap();
        drop(silent);
        while let Some(Ok(_)) = source.next().await {}
    });
    let mut w = TipStream::watch_with(
        format!("ws://127.0.0.1:{port}/tip_stream"),
        std::time::Duration::from_secs(2),
        std::time::Duration::from_millis(200),
    );
    assert_eq!(
        w.latest_lamports(Percentile::P50),
        nozomi_client::MIN_TIP_LAMPORTS
    );
    let f = tokio::time::timeout(std::time::Duration::from_secs(5), w.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.lamports(Percentile::P50), 9_000_000);
    assert_eq!(w.latest_lamports(Percentile::P50), 9_000_000);
}
