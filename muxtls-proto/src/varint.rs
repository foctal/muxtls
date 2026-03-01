use std::fmt;

use bytes::{Buf, BufMut};
use thiserror::Error;

/// QUIC-style variable-length integer.
#[derive(Default, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct VarInt(u64);

impl VarInt {
    /// Maximum representable value in QUIC varint format.
    pub const MAX: u64 = (1u64 << 62) - 1;

    /// Creates a varint from `u64`.
    pub fn from_u64(v: u64) -> Result<Self, VarIntError> {
        if v > Self::MAX {
            return Err(VarIntError::ValueTooLarge(v));
        }
        Ok(Self(v))
    }

    /// Returns the underlying integer value.
    pub fn into_inner(self) -> u64 {
        self.0
    }

    /// Encodes this value to the destination buffer.
    pub fn encode(self, out: &mut impl BufMut) {
        let v = self.0;
        if v <= 0x3f {
            out.put_u8(v as u8);
        } else if v <= 0x3fff {
            out.put_u16(0x4000 | v as u16);
        } else if v <= 0x3fff_ffff {
            out.put_u32(0x8000_0000 | v as u32);
        } else {
            out.put_u64(0xc000_0000_0000_0000 | v);
        }
    }

    /// Decodes one varint from the source buffer.
    pub fn decode(src: &mut impl Buf) -> Result<Self, VarIntError> {
        if !src.has_remaining() {
            return Err(VarIntError::UnexpectedEof);
        }

        let first = src.chunk()[0];
        let tag = first >> 6;
        let value = match tag {
            0 => (src.get_u8() & 0x3f) as u64,
            1 => {
                if src.remaining() < 2 {
                    return Err(VarIntError::UnexpectedEof);
                }
                (src.get_u16() & 0x3fff) as u64
            }
            2 => {
                if src.remaining() < 4 {
                    return Err(VarIntError::UnexpectedEof);
                }
                (src.get_u32() & 0x3fff_ffff) as u64
            }
            _ => {
                if src.remaining() < 8 {
                    return Err(VarIntError::UnexpectedEof);
                }
                src.get_u64() & 0x3fff_ffff_ffff_ffff
            }
        };

        Self::from_u64(value)
    }
}

impl fmt::Debug for VarInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VarInt({})", self.0)
    }
}

impl fmt::Display for VarInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<u64> for VarInt {
    type Error = VarIntError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::from_u64(value)
    }
}

impl From<VarInt> for u64 {
    fn from(value: VarInt) -> Self {
        value.into_inner()
    }
}

/// Errors produced by [`VarInt`].
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VarIntError {
    #[error("varint value too large: {0}")]
    ValueTooLarge(u64),

    #[error("unexpected end of input while decoding varint")]
    UnexpectedEof,
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;

    use super::{VarInt, VarIntError};

    #[test]
    fn boundary_values_roundtrip() {
        let values = [
            0,
            63,
            64,
            16_383,
            16_384,
            1_073_741_823,
            1_073_741_824,
            VarInt::MAX,
        ];

        for value in values {
            let var = VarInt::from_u64(value).expect("boundary value must be valid");
            let mut buf = BytesMut::new();
            var.encode(&mut buf);

            let decoded = VarInt::decode(&mut buf.freeze()).expect("decode must succeed");
            assert_eq!(decoded.into_inner(), value);
        }
    }

    #[test]
    fn representative_roundtrip() {
        let values = [1, 7, 15293, 65535, 999_999, 1_000_000_000];

        for value in values {
            let var = VarInt::try_from(value).expect("value must be valid");
            let mut buf = BytesMut::new();
            var.encode(&mut buf);
            let decoded = VarInt::decode(&mut buf.freeze()).expect("decode must succeed");
            assert_eq!(decoded, var);
        }
    }

    #[test]
    fn overflow_is_error() {
        let err = VarInt::from_u64(VarInt::MAX + 1).expect_err("overflow must fail");
        assert_eq!(err, VarIntError::ValueTooLarge(VarInt::MAX + 1));
    }
}
