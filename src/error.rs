use thiserror::Error;

/// Every way a Nozomi call can fail, split so callers can branch on it.
///
/// Marked `#[non_exhaustive]`: new variants can appear in minor releases, so
/// keep a wildcard arm.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Client configuration was invalid (missing API key, bad URL).
    #[error("invalid client configuration: {0}")]
    Config(String),

    /// The transaction carries no system transfer to a Nozomi tip account.
    /// Nozomi would drop it silently; the client refuses to send it.
    #[error("transaction has no tip transfer to a Nozomi tip account")]
    MissingTip,

    /// The tip is below the minimum Nozomi accepts. It would be dropped silently.
    #[error("tip of {lamports} lamports is below the {minimum} lamport minimum")]
    TipBelowMinimum { lamports: u64, minimum: u64 },

    /// The transaction has a system transfer to an address loaded from an
    /// address lookup table, which cannot be checked against the tip accounts
    /// without the table. Pass the loaded addresses to
    /// [`Client::send_transaction_with_loaded`](crate::Client::send_transaction_with_loaded)
    /// or use `send_transaction_unchecked` if you know the tip is there.
    #[error(
        "{unresolved_lamports} lamports go to lookup-table addresses that cannot be checked as tips (resolved tip: {resolved_lamports} lamports)"
    )]
    TipUnresolved {
        resolved_lamports: u64,
        unresolved_lamports: u64,
    },

    /// Serialized transaction is larger than the network allows.
    #[error("transaction is {size} bytes, maximum is {max}")]
    TransactionTooLarge { size: usize, max: usize },

    /// Serialized transaction is smaller than any valid transaction.
    #[error("transaction is {size} bytes, minimum is {min}")]
    TransactionTooSmall { size: usize, min: usize },

    /// More transactions than one batch call accepts.
    #[error("batch has {count} transactions, maximum is {max}")]
    BatchTooLarge { count: usize, max: usize },

    /// A batch with no transactions. The server would answer 400; the client
    /// does not send it.
    #[error("batch is empty")]
    EmptyBatch,

    /// Framed batch body exceeds the server limit.
    #[error("batch body is {size} bytes, maximum is {max}")]
    BatchBodyTooLarge { size: usize, max: usize },

    /// HTTP 401: API key missing or invalid.
    #[error("unauthorized: API key missing or invalid")]
    Unauthorized,

    /// HTTP 429: rate limited.
    #[error("rate limited{}", retry_after.map(|s| format!(", retry after {s}s")).unwrap_or_default())]
    RateLimited { retry_after: Option<u64> },

    /// HTTP 400 with the server's message. For batches this can mean a framing
    /// error or an insufficient tip on one transaction; earlier transactions in
    /// the batch may already have been accepted.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// HTTP 5xx.
    #[error("server error {status}: {body}")]
    Server { status: u16, body: String },

    /// Any other non-success HTTP status.
    #[error("unexpected HTTP status {status}: {body}")]
    Http { status: u16, body: String },

    /// JSON-RPC returned an error object.
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },

    /// Transport-level failure (DNS, TLS, connection refused or reset). A
    /// timeout is [`Error::Timeout`], never this.
    ///
    /// If this happens after the request left (a reset mid-response), the
    /// server may still have accepted the transaction. Nozomi retries landing on its side; do not resubmit,
    /// verify by signature with [`Verifier`](crate::Verifier) instead.
    ///
    /// The URL is stripped from the inner error before it is stored, because
    /// Nozomi puts the API key in the query string and `reqwest::Error` would
    /// otherwise print it in both `Display` and `Debug`.
    #[error("transport error: {0}")]
    Transport(#[source] reqwest::Error),

    /// Response body was not what the docs describe.
    #[error("unexpected response: {0}")]
    Decode(String),

    /// Transaction could not be serialized.
    #[error("serialize error: {0}")]
    Serialize(String),

    /// A verifier lookup found no transaction for the signature.
    #[error("transaction not found: {0}")]
    NotFound(String),

    /// The request did not complete within the client's timeout. Raised by
    /// every transport (HTTP, QUIC, websocket). If the request had already
    /// left, the server may still have accepted it; see [`Error::Transport`].
    /// `after` is `None` only for a `Verifier` built on a caller-supplied
    /// `reqwest::Client`, whose timeout the crate does not know.
    #[error("timed out{}", after.map(|d| format!(" after {d:?}")).unwrap_or_default())]
    Timeout { after: Option<std::time::Duration> },

    /// QUIC or HTTP/3 failure (handshake, stream, connection lost). The URL is
    /// never included.
    #[error("quic error: {0}")]
    Quic(String),

    /// Websocket failure on the tip stream. The URL is never included.
    #[error("websocket error: {0}")]
    WebSocket(String),
}

impl Error {
    /// Map a non-success HTTP status and body to the typed error, the same way
    /// for every transport.
    pub(crate) fn from_status(
        status: http::StatusCode,
        retry_after: Option<u64>,
        body: String,
    ) -> Self {
        match status {
            http::StatusCode::UNAUTHORIZED => Error::Unauthorized,
            http::StatusCode::TOO_MANY_REQUESTS => Error::RateLimited { retry_after },
            http::StatusCode::BAD_REQUEST => Error::BadRequest(body),
            s if s.is_server_error() => Error::Server {
                status: s.as_u16(),
                body,
            },
            s => Error::Http {
                status: s.as_u16(),
                body,
            },
        }
    }
}

impl From<reqwest::Error> for Error {
    /// Timeouts become [`Error::Timeout`] with an unknown duration; the
    /// crate's own call sites know the configured timeout and fill it in.
    fn from(e: reqwest::Error) -> Self {
        Error::from_reqwest(e, None)
    }
}

impl Error {
    /// Map a `reqwest` error, stripping the URL and classifying timeouts.
    pub(crate) fn from_reqwest(e: reqwest::Error, timeout: Option<std::time::Duration>) -> Self {
        if e.is_timeout() {
            Error::Timeout { after: timeout }
        } else {
            Error::Transport(e.without_url())
        }
    }

    /// Map a body-decoding failure: a timeout mid-body is still a timeout.
    pub(crate) fn decode_reqwest(e: reqwest::Error, timeout: Option<std::time::Duration>) -> Self {
        if e.is_timeout() {
            Error::Timeout { after: timeout }
        } else {
            Error::Decode(e.without_url().to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transport_error_does_not_leak_url() {
        // Unroutable port on localhost: connection refused, fast.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        let e = client
            .get("http://127.0.0.1:9/api/sendTransaction2?c=SECRETKEY")
            .send()
            .await
            .unwrap_err();
        let e: Error = e.into();
        assert!(matches!(e, Error::Transport(_)));
        assert!(!format!("{e}").contains("SECRETKEY"), "{e}");
        assert!(!format!("{e:?}").contains("SECRETKEY"), "{e:?}");
    }
}
