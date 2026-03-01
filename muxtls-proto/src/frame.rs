use bytes::{Buf, BufMut, Bytes};

use crate::error::{ProtoError, Result};
use crate::varint::VarInt;

const STREAM_FIN_FLAG: u8 = 0x01;

/// Application-visible protocol error code.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ErrorCode(pub VarInt);

impl ErrorCode {
    pub fn from_u64(value: u64) -> Result<Self> {
        Ok(Self(VarInt::from_u64(value)?))
    }

    pub fn into_inner(self) -> u64 {
        self.0.into_inner()
    }
}

/// One protocol frame.
#[derive(Debug, Clone)]
pub enum Frame {
    Stream {
        stream_id: VarInt,
        fin: bool,
        payload: Bytes,
    },
    ResetStream {
        stream_id: VarInt,
        error_code: ErrorCode,
    },
    Ping,
    ConnectionClose {
        error_code: ErrorCode,
        reason: String,
    },
}

impl Frame {
    /// Encodes this frame into a buffer.
    pub fn encode(&self, out: &mut impl BufMut) -> Result<()> {
        match self {
            Frame::Stream {
                stream_id,
                fin,
                payload,
            } => {
                out.put_u8(0x00);
                stream_id.encode(out);
                out.put_u8(if *fin { STREAM_FIN_FLAG } else { 0 });
                VarInt::from_u64(payload.len() as u64)?.encode(out);
                out.put_slice(payload);
            }
            Frame::ResetStream {
                stream_id,
                error_code,
            } => {
                out.put_u8(0x01);
                stream_id.encode(out);
                error_code.0.encode(out);
            }
            Frame::Ping => out.put_u8(0x02),
            Frame::ConnectionClose { error_code, reason } => {
                out.put_u8(0x03);
                error_code.0.encode(out);
                VarInt::from_u64(reason.len() as u64)?.encode(out);
                out.put_slice(reason.as_bytes());
            }
        }
        Ok(())
    }

    /// Decodes one frame from a buffer.
    pub fn decode(src: &mut impl Buf) -> Result<Self> {
        if !src.has_remaining() {
            return Err(ProtoError::UnexpectedEof);
        }

        let frame_type = src.get_u8();
        let frame = match frame_type {
            0x00 => {
                let stream_id = VarInt::decode(src)?;
                if !src.has_remaining() {
                    return Err(ProtoError::UnexpectedEof);
                }
                let flags = src.get_u8();
                let data_len = VarInt::decode(src)?.into_inner();
                let data_len: usize = data_len
                    .try_into()
                    .map_err(|_| ProtoError::LengthOverflow(data_len))?;
                if src.remaining() < data_len {
                    return Err(ProtoError::UnexpectedEof);
                }

                let payload = src.copy_to_bytes(data_len);
                Frame::Stream {
                    stream_id,
                    fin: (flags & STREAM_FIN_FLAG) != 0,
                    payload,
                }
            }
            0x01 => {
                let stream_id = VarInt::decode(src)?;
                let error_code = ErrorCode(VarInt::decode(src)?);
                Frame::ResetStream {
                    stream_id,
                    error_code,
                }
            }
            0x02 => Frame::Ping,
            0x03 => {
                let error_code = ErrorCode(VarInt::decode(src)?);
                let reason_len_u64 = VarInt::decode(src)?.into_inner();
                let reason_len: usize = reason_len_u64
                    .try_into()
                    .map_err(|_| ProtoError::LengthOverflow(reason_len_u64))?;
                if src.remaining() < reason_len {
                    return Err(ProtoError::UnexpectedEof);
                }

                let reason = std::str::from_utf8(&src.copy_to_bytes(reason_len))
                    .map_err(|_| ProtoError::InvalidUtf8)?
                    .to_owned();

                Frame::ConnectionClose { error_code, reason }
            }
            other => return Err(ProtoError::UnknownFrameType(other)),
        };

        if src.has_remaining() {
            return Err(ProtoError::TrailingBytes {
                remaining: src.remaining(),
            });
        }

        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;

    use super::{ErrorCode, Frame};
    use crate::VarInt;

    #[test]
    fn stream_frame_roundtrip() {
        let frame = Frame::Stream {
            stream_id: VarInt::from_u64(4).unwrap(),
            fin: true,
            payload: bytes::Bytes::from_static(b"hello"),
        };
        let mut encoded = BytesMut::new();
        frame.encode(&mut encoded).unwrap();
        let decoded = Frame::decode(&mut encoded.freeze()).unwrap();
        match decoded {
            Frame::Stream {
                stream_id,
                fin,
                payload,
            } => {
                assert_eq!(stream_id.into_inner(), 4);
                assert!(fin);
                assert_eq!(&payload[..], b"hello");
            }
            _ => panic!("decoded unexpected frame type"),
        }
    }

    #[test]
    fn close_frame_roundtrip() {
        let frame = Frame::ConnectionClose {
            error_code: ErrorCode::from_u64(42).unwrap(),
            reason: "closed".to_owned(),
        };
        let mut encoded = BytesMut::new();
        frame.encode(&mut encoded).unwrap();
        let decoded = Frame::decode(&mut encoded.freeze()).unwrap();
        match decoded {
            Frame::ConnectionClose { error_code, reason } => {
                assert_eq!(error_code.into_inner(), 42);
                assert_eq!(reason, "closed");
            }
            _ => panic!("decoded unexpected frame type"),
        }
    }
}
