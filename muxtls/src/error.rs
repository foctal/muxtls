use std::io;

use thiserror::Error;

/// Result type used by this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by muxtls APIs and runtime.
#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("tls error: {0}")]
    Tls(#[from] rustls::Error),

    #[error("TLS handshake failed: {0}")]
    TlsHandshake(String),

    #[error("invalid dns name: {0}")]
    InvalidDnsName(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("wire protocol error: {0}")]
    Wire(#[from] muxtls_proto::ProtoError),

    #[error("limit exceeded: {0}")]
    LimitExceeded(String),

    #[error("invalid limit `{field}`: {reason}")]
    InvalidLimit {
        /// Name of the invalid [`crate::Limits`] field.
        field: &'static str,
        /// Explanation of the accepted range or relationship.
        reason: String,
    },

    #[error("{0} timed out")]
    Timeout(&'static str),

    #[error("stream identifier space exhausted")]
    StreamIdExhausted,

    #[error("connection closed")]
    ConnectionClosed,

    #[error("stream reset with code {0}")]
    StreamReset(u64),

    #[error("endpoint role mismatch: {0}")]
    EndpointRole(&'static str),
}
