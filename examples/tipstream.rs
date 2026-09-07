//! Print tip-floor frames from the websocket stream for a while.
//!
//! cargo run --example tipstream --features tip-stream -- [seconds=120]
#[cfg(feature = "tip-stream")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::{Duration, Instant};
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let url = match std::env::var("NOZOMI_API_KEY") {
        Ok(k) => format!("{}?c={k}", nozomi_client::tipstream::TIP_STREAM_URL),
        Err(_) => nozomi_client::tipstream::TIP_STREAM_URL.to_string(),
    };
    let started = Instant::now();
    let mut s = nozomi_client::TipStream::connect(&url).await?;
    println!(
        "connected after {} ms; waiting up to {secs} s",
        started.elapsed().as_millis()
    );
    loop {
        let left = Duration::from_secs(secs).saturating_sub(started.elapsed());
        if left.is_zero() {
            println!("no more frames; done");
            break;
        }
        match tokio::time::timeout(left, s.next()).await {
            Ok(Ok(Some(f))) => println!(
                "+{:>6.1}s {}",
                started.elapsed().as_secs_f64(),
                serde_json::to_string(&f)?
            ),
            Ok(Ok(None)) => {
                println!("+{:>6.1}s server closed", started.elapsed().as_secs_f64());
                break;
            }
            Ok(Err(e)) => {
                println!("+{:>6.1}s error: {e}", started.elapsed().as_secs_f64());
                break;
            }
            Err(_) => {
                println!(
                    "+{:>6.1}s timeout, no frame",
                    started.elapsed().as_secs_f64()
                );
                break;
            }
        }
    }
    Ok(())
}

#[cfg(not(feature = "tip-stream"))]
fn main() {
    eprintln!("build with --features tip-stream");
}
