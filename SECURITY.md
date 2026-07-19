# Security and TLS Operations

## Supported authentication modes

Servers authenticate with a certificate in every deployment. Clients verify
servers with native roots, the optional platform verifier, or explicit custom
roots.

Mutual TLS is opt-in:

- Construct a client with
  `ClientConfig::with_native_roots_and_client_auth` or
  `ClientConfig::with_custom_roots_and_client_auth`.
- Construct a server with `ServerConfig::from_der_with_client_auth`.
- Read the verified handshake certificate chain through
  `Connection::peer_identity`.

The first certificate returned by `PeerIdentity::certificates` is the
end-entity certificate. The accessor deliberately exposes DER certificates
rather than assigning an application identity. Applications must define how a
certificate maps to an account, tenant, authorization policy, or audit
principal. A certificate accepted by the testing-only insecure verifier must
not be treated as authenticated.

## Certificate rotation

`ClientConfig`, `ServerConfig`, and `Endpoint` are immutable snapshots. A
configuration change affects only TLS handshakes made with a newly constructed
endpoint. Existing connections retain the certificate and authenticated peer
identity from their completed handshake until those connections close.

Use the following rotation procedure:

1. Issue the replacement certificate before the current certificate expires.
   Preserve the same DNS names or application identity unless an identity
   migration is intended.
2. Ensure clients trust both the old and new issuing chains during an overlap
   period. For mTLS, ensure servers likewise trust both client issuing chains.
3. Validate the replacement chain, private-key match, validity period, ALPN,
   and mTLS policy in a pre-production handshake.
4. Create a new config and endpoint, then move new connections to it. A process
   restart or rolling listener replacement is required by the current API.
5. Drain existing connections with an operational deadline. Use
   `Connection::close` for graceful shutdown and observe `wait_closed`.
6. Remove the old trust anchor only after the maximum connection lifetime and
   deployment rollback window have elapsed.

Private keys must be loaded from access-controlled storage and must never be
written to logs. Compromise response should skip graceful overlap: revoke or
remove the affected identity, replace the endpoint, and terminate existing
connections.

## TLS session resumption

The crate uses rustls defaults and does not expose early application data.
Application streams are available only after the TLS handshake and required
`muxtls/1` ALPN negotiation complete, so 0-RTT replay is not part of the
muxtls API.

A reused `ClientConfig` may retain client-side resumption state. A
`ServerConfig` created by this crate owns its rustls server-side resumption
state. Creating a replacement config for certificate or client-auth rotation
also creates a new resumption context, so sessions from the old endpoint are
not intentionally shared with the new endpoint.

Deployments MUST NOT add shared rustls session storage or ticket keys across
configs with different server identities, client certificate verifiers, trust
roots, or authorization policies. Doing so can resume a session under security
requirements different from those of the original handshake. Rotating an mTLS
trust policy therefore requires a fresh config and resumption context.

Peer certificates returned for a connection describe the identity rustls
associated with that completed or resumed session. Authorization code should
evaluate the identity once per connection and apply an explicit maximum
connection lifetime when prompt certificate or policy revocation is required.

## Protocol resource behavior

`muxtls/1` has bounded local queues but no advertised per-stream flow-control
window. A peer that exceeds a receiver's configured inbound capacity is
disconnected. Operators must configure compatible limits at both endpoints.
The wire design for protocol-level backpressure is documented in
`muxtls-proto/FLOW_CONTROL.md` and requires a future `muxtls/2` implementation.

