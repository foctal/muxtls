#![no_main]

use bytes::{Buf, Bytes, BytesMut};
use libfuzzer_sys::fuzz_target;
use muxtls_proto::VarInt;

fuzz_target!(|data: &[u8]| {
    let mut input = Bytes::copy_from_slice(data);
    let Ok(decoded) = VarInt::decode(&mut input) else {
        return;
    };

    let mut canonical = BytesMut::new();
    decoded.encode(&mut canonical);
    assert_eq!(canonical.len(), decoded.encoded_len());

    let mut canonical = canonical.freeze();
    let roundtrip = VarInt::decode(&mut canonical).expect("canonical varint must decode");
    assert_eq!(roundtrip, decoded);
    assert!(!canonical.has_remaining());
});
