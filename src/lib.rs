//! Rust client for [Nozomi](https://use.temporal.xyz/nozomi/readme.md), Temporal's
//! Solana transaction landing service.
//!
//! What this crate does that the raw HTTP samples do not:
//!
//! * **Typed endpoints.** Every region and route from the docs is an enum, not a
//!   string you copy from a page.
//! * **Tip rotation and validation.** The 17 public tip accounts are built in.
//!   Before a transaction leaves your process the client checks that it carries a
//!   system transfer to one of them at or above the minimum. Nozomi silently drops
//!   under-tipped transactions with no error; this crate refuses to send them.
//! * **One warm connection.** A pooled HTTP client with a keep-alive task that
//!   pings `/ping` every 60 seconds, so you never pay a reconnect on the hot path.
//! * **No client-side resubmission.** Nozomi retries for you and their docs say
//!   resubmitting lowers your priority. The client sends once and tells you what
//!   happened.
//! * **Batch framing done right.** Length-prefixed framing, size and count limits
//!   enforced locally, and the stream-processing semantics documented on the type.
//! * **Landing verification.** Give it a signature and a Solana RPC and it tells
//!   you the slot it landed in, which tip account was targeted, what tip was
//!   intended, and what tip was actually paid from on-chain balances. On a
//!   revert those differ: the tip is rolled back with the transaction and only
//!   the fee is charged, whatever the docs say.
//! * **Tracing built in.** Every send is a span with region, byte size and tip
//!   lamports, and the client keeps running counters you can export.
//!
//! ```no_run
//! use nozomi_client::{Client, Region, Route};
//!
//! # async fn run() -> Result<(), nozomi_client::Error> {
//! let client = Client::builder()
//!     .api_key(std::env::var("NOZOMI_API_KEY").unwrap())
//!     .region(Region::Frankfurt)
//!     .route(Route::Direct { tls: false })
//!     .build()?;
//!
//! let _keepalive = client.spawn_keepalive();
//! let tx_bytes: Vec<u8> = todo!("serialize a signed VersionedTransaction that includes a tip");
//! client.send(&tx_bytes).await?;
//! # Ok(()) }
//! ```

pub mod client;
pub mod error;
pub mod region;
pub mod tip;
pub mod tipfloor;
pub mod verify;

pub use client::{Client, ClientBuilder, KeepAlive, Stats};
pub use error::Error;
pub use region::{Region, Route};
pub use tip::{MIN_TIP_LAMPORTS, TIP_ACCOUNTS};
pub use tipfloor::TipFloor;
pub use verify::{LandingReport, SignatureInfo, Verifier};

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Largest serialized transaction Nozomi accepts, in bytes.
pub const MAX_TX_BYTES: usize = 1_232;
/// Smallest serialized transaction Nozomi accepts, in bytes.
pub const MIN_TX_BYTES: usize = 66;
/// Maximum transactions per `sendBatch` call.
pub const MAX_BATCH_TXS: usize = 16;
/// Maximum `sendBatch` body size, in bytes.
pub const MAX_BATCH_BODY_BYTES: usize = 19_744;
