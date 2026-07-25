#![no_main]

use bytes::{Bytes, BytesMut};
use libfuzzer_sys::fuzz_target;
use muxtls_proto::Frame;

fuzz_target!(|data: &[u8]| {
    let mut input = Bytes::copy_from_slice(data);
    let Ok(decoded) = Frame::decode(&mut input) else {
        return;
    };

    let mut canonical = BytesMut::new();
    decoded
        .encode(&mut canonical)
        .expect("decoded frame must re-encode");
    assert_eq!(
        canonical.len(),
        decoded.encoded_len().expect("decoded frame length")
    );

    let roundtrip = Frame::decode(&mut canonical.freeze()).expect("canonical frame must decode");
    assert_eq!(roundtrip, decoded);
});
