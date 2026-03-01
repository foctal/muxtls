//! Wire protocol primitives for muxtls.
//!
//! This crate contains transport-independent protocol definitions:
//!
//! - QUIC-style [`VarInt`]
//! - frame definitions ([`Frame`])
//! - frame encode/decode routines
//! - protocol error types

mod error;
mod frame;
mod varint;

pub use error::{ProtoError, Result};
pub use frame::{ErrorCode, Frame};
pub use varint::{VarInt, VarIntError};
