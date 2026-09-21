use bytes::{Buf, Bytes, BytesMut};
use muxtls_proto::{ErrorCode, Frame, ProtoError, VarInt, VarIntError};
use serde::Deserialize;

#[derive(Deserialize)]
struct Vectors {
    schema_version: u64,
    protocol: String,
    varints: Vec<VarIntVector>,
    frames: Vec<FrameVector>,
    invalid_varints: Vec<InvalidVector>,
    invalid_frames: Vec<InvalidVector>,
}

#[derive(Deserialize)]
struct VarIntVector {
    name: String,
    value: u64,
    encoded: String,
}

#[derive(Deserialize)]
struct FrameVector {
    name: String,
    encoded: String,
    length_prefixed: String,
    frame: ExpectedFrame,
}

#[derive(Deserialize)]
struct InvalidVector {
    name: String,
    encoded: String,
    error: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ExpectedFrame {
    Stream {
        stream_id: u64,
        fin: bool,
        payload: String,
    },
    ResetStream {
        stream_id: u64,
        error_code: u64,
    },
    OpenStream {
        stream_id: u64,
    },
    Settings {
        max_data: u64,
        max_stream_data: u64,
        max_frame_size: u64,
    },
    MaxData {
        maximum: u64,
    },
    MaxStreamData {
        stream_id: u64,
        maximum: u64,
    },
    StopSending {
        stream_id: u64,
    },
    Ping,
    ConnectionClose {
        error_code: u64,
        reason: String,
    },
}

#[test]
fn version_one_conformance_vectors_match_codec() {
    let vectors: Vectors = serde_json::from_str(include_str!("../test-vectors/v1.json")).unwrap();
    assert_eq!(vectors.protocol, "muxtls/1");
    check_vectors(vectors);
}

fn check_vectors(vectors: Vectors) {
    assert_eq!(vectors.schema_version, 1);
    for vector in vectors.varints {
        let expected = decode_hex(&vector.encoded, &vector.name);
        let value = VarInt::from_u64(vector.value).expect("vector value must fit");
        let mut encoded = BytesMut::new();
        value.encode(&mut encoded);
        assert_eq!(encoded.as_ref(), expected, "encode: {}", vector.name);

        let mut input = Bytes::from(expected);
        let decoded = VarInt::decode(&mut input).expect("valid varint vector");
        assert_eq!(decoded, value, "decode: {}", vector.name);
        assert!(!input.has_remaining(), "unread bytes: {}", vector.name);
    }

    for vector in vectors.frames {
        let expected = expected_frame(vector.frame, &vector.name);
        let encoded = decode_hex(&vector.encoded, &vector.name);

        let mut output = BytesMut::new();
        expected.encode(&mut output).expect("encode valid frame");
        assert_eq!(output.as_ref(), encoded, "encode: {}", vector.name);
        assert_eq!(
            expected.encoded_len().expect("encoded length"),
            encoded.len(),
            "encoded length: {}",
            vector.name
        );

        let decoded = Frame::decode(&mut Bytes::from(encoded.clone())).expect("decode valid frame");
        assert_eq!(decoded, expected, "decode: {}", vector.name);

        let length_prefixed = decode_hex(&vector.length_prefixed, &vector.name);
        assert!(length_prefixed.len() >= 4, "length prefix: {}", vector.name);
        let declared =
            u32::from_be_bytes(length_prefixed[..4].try_into().expect("four-byte prefix")) as usize;
        assert_eq!(declared, encoded.len(), "declared length: {}", vector.name);
        assert_eq!(
            &length_prefixed[4..],
            encoded,
            "record body: {}",
            vector.name
        );
    }

    for vector in vectors.invalid_varints {
        let mut input = Bytes::from(decode_hex(&vector.encoded, &vector.name));
        let error = VarInt::decode(&mut input).expect_err("invalid varint vector must fail");
        assert_eq!(
            varint_error_name(&error),
            vector.error,
            "error: {}",
            vector.name
        );
    }

    for vector in vectors.invalid_frames {
        let mut input = Bytes::from(decode_hex(&vector.encoded, &vector.name));
        let error = Frame::decode(&mut input).expect_err("invalid frame vector must fail");
        assert_eq!(
            frame_error_name(&error),
            vector.error,
            "error: {}",
            vector.name
        );
    }
}

fn expected_frame(frame: ExpectedFrame, name: &str) -> Frame {
    match frame {
        ExpectedFrame::Stream {
            stream_id,
            fin,
            payload,
        } => Frame::Stream {
            stream_id: VarInt::from_u64(stream_id).expect("stream id must fit"),
            fin,
            payload: Bytes::from(decode_hex(&payload, name)),
        },
        ExpectedFrame::ResetStream {
            stream_id,
            error_code,
        } => Frame::ResetStream {
            stream_id: VarInt::from_u64(stream_id).expect("stream id must fit"),
            error_code: ErrorCode::from_u64(error_code).expect("error code must fit"),
        },
        ExpectedFrame::OpenStream { stream_id } => Frame::OpenStream {
            stream_id: VarInt::from_u64(stream_id).expect("stream id must fit"),
        },
        ExpectedFrame::Settings {
            max_data,
            max_stream_data,
            max_frame_size,
        } => Frame::Settings {
            max_data: VarInt::from_u64(max_data).unwrap(),
            max_stream_data: VarInt::from_u64(max_stream_data).unwrap(),
            max_frame_size: VarInt::from_u64(max_frame_size).unwrap(),
        },
        ExpectedFrame::MaxData { maximum } => Frame::MaxData {
            maximum: VarInt::from_u64(maximum).unwrap(),
        },
        ExpectedFrame::MaxStreamData { stream_id, maximum } => Frame::MaxStreamData {
            stream_id: VarInt::from_u64(stream_id).unwrap(),
            maximum: VarInt::from_u64(maximum).unwrap(),
        },
        ExpectedFrame::StopSending { stream_id } => Frame::StopSending {
            stream_id: VarInt::from_u64(stream_id).unwrap(),
        },
        ExpectedFrame::Ping => Frame::Ping,
        ExpectedFrame::ConnectionClose { error_code, reason } => Frame::ConnectionClose {
            error_code: ErrorCode::from_u64(error_code).expect("error code must fit"),
            reason,
        },
    }
}

fn decode_hex(encoded: &str, name: &str) -> Vec<u8> {
    assert_eq!(encoded.len() % 2, 0, "odd hex length: {name}");
    encoded
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("hex must be ASCII");
            u8::from_str_radix(pair, 16).unwrap_or_else(|_| panic!("invalid hex in {name}"))
        })
        .collect()
}

fn varint_error_name(error: &VarIntError) -> &'static str {
    match error {
        VarIntError::UnexpectedEof => "unexpected_eof",
        VarIntError::ValueTooLarge(_) => "value_too_large",
    }
}

fn frame_error_name(error: &ProtoError) -> &'static str {
    match error {
        ProtoError::VarInt(VarIntError::UnexpectedEof) => "varint_unexpected_eof",
        ProtoError::VarInt(VarIntError::ValueTooLarge(_)) => "varint_value_too_large",
        ProtoError::UnexpectedEof => "unexpected_eof",
        ProtoError::UnknownFrameType(_) => "unknown_frame_type",
        ProtoError::UnsupportedFlags { .. } => "unsupported_flags",
        ProtoError::InvalidUtf8 => "invalid_utf8",
        ProtoError::TrailingBytes { .. } => "trailing_bytes",
        ProtoError::LengthOverflow(_) => "length_overflow",
    }
}
