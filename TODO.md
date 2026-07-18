# Production Readiness Review

This document records the production-readiness review performed on 2026-07-18.
Items are ordered by severity. Checked items are addressed in the current
working tree; unchecked items need additional design or operational work before
the library should be used across an untrusted network boundary.

## P0 — Safety and correctness

- [x] Validate every resource limit before constructing Tokio semaphores or
  converting permit counts to `u32`.
- [x] Wake stream-open, read, and write waiters when a connection terminates.
- [x] Separate "closing" from terminal cleanup so a locally initiated close
  cannot skip stream cleanup.
- [x] Serialize writes, FIN, reset, and last-handle cleanup per stream so frames
  cannot be queued after a terminal frame.
- [x] Reject data after FIN, reused retired stream IDs, invalid stream parity,
  and unsupported frame flag bits.
- [x] Enforce the configured frame limit against the complete encoded frame,
  not only its application payload.
- [x] Prevent one unread stream from blocking the connection reader forever.
  Exceeding an inbound buffer limit now closes the connection instead of
  awaiting permits in the sole reader task.
- [x] Add ALPN (`muxtls/1`) to prevent accidental protocol confusion with
  another TLS application using the same certificate and port.
- [x] Bound client and server TLS handshakes with a configurable timeout.
- [x] Provide `Connection::wait_closed` and make remote close stop the reader.

## P1 — Protocol resilience

- [x] Add malformed/truncated frame tests and boundary tests for frame flags,
  limits, shutdown, reset, and large `AsyncWrite` calls.
- [x] Make `AsyncWrite` accept buffers larger than one frame by reporting a
  valid partial write.
- [x] Validate close reasons before changing connection state.
- [ ] Design protocol-level flow control. The current protocol has no
  window-update frame, so a peer that exceeds advertised local buffering
  cannot be backpressured per stream and is disconnected instead.
- [ ] Expand the wire protocol into a standalone versioned document, including
  the four-byte length prefix, stream ID rules, state transitions, error codes,
  and compatibility policy.
- [ ] Add protocol conformance vectors shared with any non-Rust implementation.
- [ ] Add coverage-guided fuzzing for `VarInt::decode`, `Frame::decode`, and
  connection state transitions.
- [x] Add opt-in keepalive and inbound idle-timeout policies. Endpoints can
  schedule `PING` frames and close connections that receive no frames within a
  configured duration.

## P1 — TLS and deployment

- [x] Report failure when the native certificate store yields no usable roots.
- [x] Document that server accept loops must run TLS handshakes concurrently.
- [ ] Add optional mutual TLS configuration and peer identity accessors.
- [ ] Define certificate rotation and session-resumption behavior.
- [ ] Run an independent security review before handling sensitive production
  traffic.

## P2 — Quality and operations

- [x] Strengthen CI with formatting, Clippy, all-feature tests, documentation,
  protocol package verification, and an MSRV declaration.
- [x] Remove unused direct dependencies.
- [x] Expand public API documentation and production caveats.
- [ ] Add Linux, macOS, and Windows interoperability jobs with real certificate
  stores. The current CI validates the portable custom-root path on Linux.
- [ ] Add long-running soak tests covering churn, cancellation, half-close,
  abrupt TCP loss, and memory ceilings.
- [ ] Establish benchmark baselines and regression thresholds for throughput,
  latency, allocations, and fairness.
- [ ] Add dependency vulnerability/license policy automation (for example,
  `cargo-audit` and `cargo-deny`) after the project selects an advisory policy.
- [ ] Define an observability contract: stable tracing event names, connection
  identifiers, close causes, and metric export guidance.

## Release gate

Do not describe the library as safe for hostile Internet peers until the
unchecked P1 protocol and security-review items are complete. For controlled
production environments, require explicit limits, handshake/idle timeouts at
the service layer, monitoring of protocol-close events, and load/soak testing
against the intended workload.

For a manual release, publish and verify `muxtls-proto` before packaging
`muxtls`; Cargo intentionally resolves packaged path dependencies from the
registry.
