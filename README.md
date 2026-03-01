[crates-badge]: https://img.shields.io/crates/v/muxtls.svg
[crates-url]: https://crates.io/crates/muxtls
[doc-url]: https://docs.rs/muxtls/latest/muxtls
[license-badge]: https://img.shields.io/crates/l/muxtls.svg

# muxtls [![Crates.io][crates-badge]][crates-url] ![License][license-badge]

Multiplexed streams over TLS/TCP

## Features
- TLS-secured client/server transport using `rustls` + `tokio-rustls`
- Multiple independent bidirectional logical streams over one TLS/TCP connection
- Bounded-memory runtime with per-frame, per-stream, and per-connection limits
- Stream-oriented API with async backpressure
- `SendStream` implements `tokio::io::AsyncWrite`
- `RecvStream` implements `tokio::io::AsyncRead`

## Crates
- `muxtls-proto`: Transport-agnostic wire protocol
  - QUIC-style `VarInt`
  - `Frame` definitions and encode/decode routines
  - Protocol error types (`ProtoError`, `ErrorCode`)
- `muxtls`: Async transport implementation
  - TLS over TCP endpoint/connection/stream runtime
  - Stream multiplexing and bounded-memory backpressure
  - Depends on `muxtls-proto` for wire format

## Quick start

```toml
[dependencies]
muxtls = "0.1"
```

API documentation is available on [docs.rs][doc-url].  

## Wire Format Overview
`muxtls` uses length-delimited frames, and each frame payload is encoded by `muxtls-proto`.

Supported frame types:
- `STREAM`
- `RESET_STREAM`
- `PING`
- `CONNECTION_CLOSE`

## Examples
- `cargo run -p muxtls --example echo_server`
- `cargo run -p muxtls --example echo_client`
