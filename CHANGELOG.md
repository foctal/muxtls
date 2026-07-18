# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Added

- Explicit `OPEN_STREAM` frames and strict stream ID sequencing.
- The `muxtls/1` ALPN protocol identifier.
- Configurable client and server handshake timeouts.
- Optional keepalive intervals and inbound idle timeouts.
- `Connection::wait_closed` and `Connection::is_closed`.
- Strict resource-limit validation and encoded-frame size accounting.
- Malformed-frame, shutdown, timeout, reset, and large-write tests.
- A standalone version 1 protocol specification and machine-readable
  conformance vectors.
- Coverage-guided fuzz targets for variable-length integers, frames, and
  connection state transitions.

### Changed

- `AsyncWrite` now splits writes at encoded frame boundaries.
- Connection shutdown now wakes blocked stream and connection operations.
- Inbound buffer exhaustion closes the connection instead of blocking the sole
  connection reader.
- The final representable even and odd stream IDs can now be allocated.
- CI now checks formatting, Clippy, all features, documentation, packages, and
  Rust 1.88 compatibility.

### Security

- TLS clients and servers require matching ALPN, preventing cross-protocol use
  of a valid certificate.
- Reserved frame flags, reused stream IDs, invalid limits, and oversized
  encoded frames are rejected.
- Stream frames and resets received after the peer's send direction reaches a
  terminal state are rejected.

## [0.1.0] - 2026-07-17

- Initial release.

[Unreleased]: https://github.com/foctal/muxtls/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/foctal/muxtls/releases/tag/v0.1.0
