#![forbid(unsafe_code)]

//! Wire protocol primitives for muxtls.
//!
//! This crate contains transport-independent protocol definitions:
//!
//! - QUIC-style [`VarInt`]
//! - frame definitions ([`Frame`])
//! - frame encode/decode routines
//! - protocol error types
//!
//! The complete version 1 wire format and state rules are specified in the
//! crate package's `PROTOCOL.md`. Language-independent examples are available
//! in `test-vectors/v1.json`.

mod error;
mod frame;
mod varint;

pub use error::{ProtoError, Result};
pub use frame::{ErrorCode, Frame};
pub use varint::{VarInt, VarIntError};
