# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Added

- Explicit `OPEN_STREAM` frames and strict stream ID sequencing.
- The `muxtls/1` ALPN protocol identifier.
- Configurable client and server handshake timeouts.
- `Connection::wait_closed` and `Connection::is_closed`.
- Strict resource-limit validation and encoded-frame size accounting.
- Malformed-frame, shutdown, timeout, reset, and large-write tests.

### Changed

- `AsyncWrite` now splits writes at encoded frame boundaries.
- Connection shutdown now wakes blocked stream and connection operations.
- Inbound buffer exhaustion closes the connection instead of blocking the sole
  connection reader.
- CI now checks formatting, Clippy, all features, documentation, packages, and
  Rust 1.88 compatibility.

### Security

- TLS clients and servers require matching ALPN, preventing cross-protocol use
  of a valid certificate.
- Reserved frame flags, reused stream IDs, invalid limits, and oversized
  encoded frames are rejected.

## [0.1.0] - 2026-07-17

- Initial release.

[Unreleased]: https://github.com/foctal/muxtls/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/foctal/muxtls/releases/tag/v0.1.0
