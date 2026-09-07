# Changelog

## 0.2.0 (2026-09-07)

### Security

- Transport errors strip the request URL, and `Debug` on `Client`, `ClientBuilder`, `QuicClient`, and `Verifier` redacts secrets. In 0.1.0 a logged transport error printed the Nozomi API key, and a logged verifier error printed the RPC key. Decode errors are scrubbed the same way.

### Added

- `QuicClient` (feature `quic`): HTTP/3 over QUIC to `/api/sendBatch`, with warmup, lazy connect, transparent reconnect, a no-double-submit retry rule, per-request timeout, and the same typed status mapping as the HTTP client. Unlike Temporal's reference client it reads the response.
- `TipStream` and `Client::tip_stream` (feature `tip-stream`): the websocket feed behind `/tip_floor`.
- `ClientBuilder::base_url` to point at a proxy or test server.
- `Client::send_transaction_unchecked` to skip the local tip check.
- `tip::inspect_with_loaded` and `Client::send_transaction_with_loaded`: pass the addresses a transaction loads from its lookup tables and a tip whose destination lives there is checked exactly. Without them, such a transfer is reported in `TipInspection::unresolved` and `check()` returns the new `Error::TipUnresolved` instead of guessing either way.
- `Verifier::with_timeout`; `Verifier::new` now applies a 30-second timeout.
- `TipStream::watch`, `TipWatch`, and `Client::tip_watch`: a reconnecting background subscription that keeps the latest tip floor in a `watch` channel, for reading at send time without touching the network.
- `Region::quic_host`, `Error::Timeout`, `Error::Quic`, `Error::WebSocket`, `Error::EmptyBatch`, `Error::TipUnresolved`, `TipFloor::is_complete`, `TipWatch::latest_lamports`, `TipStream::connect_with` and `watch_with` with connect and per-frame read timeouts (defaults 10 s and 45 s).
- The API key is percent-encoded into the query string on every transport, so a key with a stray newline, space, or `#` fails loudly at build or is sent intact instead of truncating the URL.
- `LICENSE-MIT` and `LICENSE-APACHE` files, which the manifest already declared.
- CI runs the full feature matrix, an MSRV (1.91) check, strict rustdoc, and `cargo audit`.
- Mock-server test suites for every network path (HTTP, QUIC, websocket, RPC) and a live `smoke` example.

### Changed (breaking)

- `TipFloor` percentiles are `Option<f64>` and `TipFloor::sol` returns `Option<f64>`; the live API returns `null` when it has no recent data. `lamports()` still returns the minimum in that case.
- `Error::Transport` no longer derives `From` via thiserror; the manual `From<reqwest::Error>` strips the URL. Matching on the variant is unchanged.
- `ping()` and `tip_floor()` map non-success statuses through the same table as sends, so a 5xx is `Error::Server`, not `Error::Http`.
- Timeouts are `Error::Timeout { after }` on every transport, HTTP and verifier included; in 0.1.0 an HTTP timeout was an `Error::Transport`, indistinguishable from a connection refused even though the server may have accepted the send.
- `tip_api_base` is normalized like `base_url` (trailing slash trimmed, scheme required), so a trailing slash no longer produces a `//tip_floor` path that the API answers with 404.
- `QuicClient::is_connected` and `disconnect` are synchronous. `QuicClientBuilder::build` no longer needs a tokio runtime; the UDP socket is bound on first connect, in the address family of the resolved peer, so IPv6 hosts work.
- `send_transaction` records `tip_lamports` on its own span; in 0.1.0 the field was recorded on an undeclared span and dropped.
- `Stats::rejected_locally` counts one per call for every path, batches included, and `rejected_by_server`/`roundtrips` are populated by the QUIC client too.
- `Error` and `Region` are `#[non_exhaustive]`, and `Region::ALL` is a slice instead of a `[Region; 9]`, so new variants and regions can ship in minor releases. Keep a wildcard arm.
- `frame_batch`, `send_batch`, and the QUIC sends refuse an empty batch with `Error::EmptyBatch` instead of letting the server answer 400.

## 0.1.0 (2026-09-03)

Initial release.
