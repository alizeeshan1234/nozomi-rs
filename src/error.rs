use thiserror::Error;

/// Every way a Nozomi call can fail, split so callers can branch on it.
#[derive(Debug, Error)]
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

    /// Serialized transaction is larger than the network allows.
    #[error("transaction is {size} bytes, maximum is {max}")]
    TransactionTooLarge { size: usize, max: usize },

    /// Serialized transaction is smaller than any valid transaction.
    #[error("transaction is {size} bytes, minimum is {min}")]
    TransactionTooSmall { size: usize, min: usize },

    /// More transactions than one batch call accepts.
    #[error("batch has {count} transactions, maximum is {max}")]
    BatchTooLarge { count: usize, max: usize },

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

    /// Transport-level failure (DNS, TLS, timeout, connection reset).
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// Response body was not what the docs describe.
    #[error("unexpected response: {0}")]
    Decode(String),

    /// Transaction could not be serialized.
    #[error("serialize error: {0}")]
    Serialize(String),

    /// A verifier lookup found no transaction for the signature.
    #[error("transaction not found: {0}")]
    NotFound(String),
}
