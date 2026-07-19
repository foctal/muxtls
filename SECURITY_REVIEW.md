# Independent Security Review Package

This document defines the evidence and acceptance criteria for the independent
security review required before muxtls handles sensitive production traffic.
It is not a review report and does not replace review by an independent party.

## Independence requirement

The reviewer must not be the author of the implementation under review and
must be able to report findings without approval from that author. Record the
reviewer's name or organization, review dates, source revision, methodology,
and any conflicts of interest in the final report.

## Review scope

- `muxtls-proto/src`: variable-length integers, frame encoding, length
  accounting, and malformed input handling.
- `muxtls/src/connection.rs` and `muxtls/src/stream.rs`: state transitions,
  concurrency, cancellation, resource accounting, fairness, and shutdown.
- `muxtls/src/config.rs` and `muxtls/src/endpoint.rs`: certificate validation,
  mTLS, ALPN, timeouts, peer identity, and session behavior.
- `muxtls-proto/PROTOCOL.md`, `muxtls-proto/FLOW_CONTROL.md`, and conformance
  vectors: implementation/specification agreement and compatibility.
- Fuzz targets, integration tests, dependency configuration, unsafe-code
  prohibition, CI, and production guidance.

Out of scope items must be listed explicitly in the final report.

## Threat model

Assume an unauthenticated network peer can open TCP connections, complete or
stall TLS handshakes, send arbitrary fragmented and length-prefixed bytes,
create and abandon streams, race terminal operations, and intentionally
consume CPU, memory, permits, and task scheduling capacity. For mTLS, also
consider a peer with a valid certificate that is not authorized for the
requested application operation.

The reviewer should evaluate confidentiality and integrity assumptions at the
TLS boundary, authentication-to-authorization mapping, cross-protocol attacks,
memory and task exhaustion, parser differentials, state-machine violations,
deadlocks, starvation, cancellation safety, and diagnostic data exposure.

## Reproducible evidence

Run from a clean checkout at the reviewed source revision:

```text
cargo fmt --all -- --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo test --manifest-path fuzz/Cargo.toml
```

Run every fuzz target for a reviewer-selected duration and record corpus,
crashes, sanitizer configuration, toolchain, platform, and exact commands.
Perform adversarial integration testing with mismatched limits, slow and
truncated TLS/TCP input, abrupt loss, stream churn, and concurrent close/reset.

## Finding and acceptance policy

Each finding must have a stable identifier, severity, affected revision and
code, exploit scenario, evidence, recommended remediation, and status.
Severity definitions and accepted-risk authority must be agreed before review.

The P1 release gate is satisfied only when:

- the final report covers the scope above;
- all critical and high findings are fixed and independently retested;
- medium findings are fixed or have documented owner-approved mitigations;
- residual risks and excluded areas are listed in release documentation; and
- the reviewer signs off on the exact source revision intended for release.

Add a link to the final report below. Do not mark the corresponding `TODO.md`
item complete until these criteria are met.

## Final report

Pending.

