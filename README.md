[crates-badge]: https://img.shields.io/crates/v/muxtls.svg
[crates-url]: https://crates.io/crates/muxtls
[doc-url]: https://docs.rs/muxtls/latest/muxtls
[license-badge]: https://img.shields.io/crates/l/muxtls.svg

# muxtls [![Crates.io][crates-badge]][crates-url] ![License][license-badge]

Multiplexed streams over TLS/TCP

> **Status:** The runtime has bounded buffers, strict protocol validation, ALPN,
> and connection/handshake shutdown handling. Review the remaining release
> gates in
> [`TODO.md`](https://github.com/foctal/muxtls/blob/main/TODO.md) before exposing
> it to untrusted Internet peers.

## Features
- TLS-secured client/server transport using `rustls` + `tokio-rustls`
- Protocol isolation through the `muxtls/1` ALPN identifier
- Multiple independent bidirectional logical streams over one TLS/TCP connection
- Bounded-memory runtime with per-frame, per-stream, and per-connection limits
- Optional protocol keepalive and inbound idle timeouts
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
muxtls = "0.2"
```

API documentation is available on [docs.rs][doc-url].  

## Wire Format Overview

`muxtls` uses a four-byte big-endian length prefix followed by a frame encoded
by `muxtls-proto`. Client-initiated stream IDs are even, server-initiated stream
IDs are odd, and each side announces IDs monotonically with `OPEN_STREAM`.

Supported frame types:
- `OPEN_STREAM`
- `STREAM`
- `RESET_STREAM`
- `PING`
- `CONNECTION_CLOSE`

## Examples
- `cargo run -p muxtls --example echo_server`
- `cargo run -p muxtls --example echo_client`

The examples generate and bypass verification of a development certificate.
Use `ServerConfig::from_pem_files` and `ClientConfig::with_native_roots` (or
explicit custom roots) in deployed services.
