use thiserror::Error;

use crate::VarIntError;

/// Result type used by `muxtls-proto`.
pub type Result<T> = std::result::Result<T, ProtoError>;

/// Errors raised while encoding or decoding protocol frames.
#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("varint error: {0}")]
    VarInt(#[from] VarIntError),

    #[error("unexpected end of input")]
    UnexpectedEof,

    #[error("unknown frame type: {0:#x}")]
    UnknownFrameType(u8),

    #[error("invalid UTF-8 in close reason")]
    InvalidUtf8,

    #[error("trailing bytes in frame: {remaining}")]
    TrailingBytes { remaining: usize },

    #[error("payload length too large for platform: {0}")]
    LengthOverflow(u64),
}
