# muxtls Wire Protocol Version 1

This document defines the `muxtls/1` wire protocol. The key words **MUST**,
**MUST NOT**, **SHOULD**, and **MAY** are normative.

Version 1 multiplexes ordered bidirectional streams over one TLS-protected TCP
connection. TLS provides confidentiality, integrity, authentication according
to the deployment's TLS configuration, and reliable ordered delivery. muxtls
provides stream identification, framing, stream lifecycle signaling, and
connection-level and stream-level receive-credit flow control.

## Version identification

Peers MUST negotiate the TLS ALPN identifier `muxtls/1`. A peer MUST abort the
connection if this identifier is not negotiated.

The `/1` suffix is the wire-protocol major version. A change that makes an
existing version 1 frame or state transition ambiguous or incompatible requires
a new ALPN identifier. Version 1 has no extension negotiation mechanism.

## Integer encoding

All variable-length integers use the QUIC integer representation: the two most
significant bits of the first byte select a total encoded length of 1, 2, 4, or
8 bytes. The remaining bits contain an unsigned big-endian integer.

| Prefix bits | Encoded bytes | Value range |
| --- | ---: | ---: |
| `00` | 1 | 0 to 63 |
| `01` | 2 | 0 to 16,383 |
| `10` | 4 | 0 to 1,073,741,823 |
| `11` | 8 | 0 to 4,611,686,018,427,387,903 |

Senders MUST use the shortest encoding for a value. Version 1 receivers MAY
accept a longer representation. Values larger than `2^62 - 1` cannot be
represented.

## Record framing

Each record consists of:

```text
0                   1                   2                   3
0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+---------------------------------------------------------------+
|              Frame Length (32-bit big-endian)                 |
+---------------------------------------------------------------+
|                     Frame (Frame Length bytes)               ...
+---------------------------------------------------------------+
```

`Frame Length` is an unsigned 32-bit big-endian integer. It counts the complete
inner frame and excludes the four-byte prefix. Empty inner frames are invalid.
Each endpoint advertises its maximum inner frame length in SETTINGS.
Subsequent frames MUST respect the peer's advertised maximum. SETTINGS itself
fits within the minimum supported frame size of 25 bytes.

Exactly one inner frame follows each length prefix. A truncated prefix,
truncated frame, or trailing byte inside an inner frame is a connection error.

## Frame types

The first byte of every inner frame is its frame type.

| Type | Name | Purpose |
| ---: | --- | --- |
| `0x00` | `STREAM` | Carries stream data and optional FIN |
| `0x01` | `RESET_STREAM` | Terminates the sender's stream direction |
| `0x02` | `PING` | Indicates connection activity |
| `0x03` | `CONNECTION_CLOSE` | Terminates the connection |
| `0x04` | `OPEN_STREAM` | Announces a new bidirectional stream |
| `0x05` | `SETTINGS` | Advertises initial receive credit and maximum frame size |
| `0x06` | `MAX_DATA` | Increases the connection receive-credit limit |
| `0x07` | `MAX_STREAM_DATA` | Increases a stream receive-credit limit |
| `0x08` | `STOP_SENDING` | Requests termination of the peer's send direction |

Unknown frame types are connection errors. Frame definitions below are shown
without the four-byte record length.

### STREAM (`0x00`)

```text
Type (0x00) | Stream ID (varint) | Flags (u8) |
Payload Length (varint) | Payload (Payload Length bytes)
```

Bit `0x01` of `Flags` is `FIN`. All other flag bits are reserved and MUST be
zero. `Payload` is opaque application data and MAY be empty.

`FIN=1` terminates the frame sender's direction after the payload. The sender
MUST NOT send another `STREAM` or `RESET_STREAM` for that stream afterward.

### RESET_STREAM (`0x01`)

```text
Type (0x01) | Stream ID (varint) | Error Code (varint)
```

`RESET_STREAM` abruptly terminates the frame sender's direction. The peer's
opposite direction remains usable. The sender MUST NOT send another `STREAM` or
`RESET_STREAM` for that stream afterward.

Stream reset error codes are application-defined. Code zero is used by the Rust
implementation when the last writable stream handle is dropped without FIN.

### PING (`0x02`)

```text
Type (0x02)
```

`PING` has no payload and requires no direct response. It counts as received
connection activity and can therefore be used by periodic keepalive policies.

### CONNECTION_CLOSE (`0x03`)

```text
Type (0x03) | Error Code (varint) | Reason Length (varint) |
Reason (Reason Length bytes)
```

`Reason` MUST be valid UTF-8 and is diagnostic text. It is not intended for
machine parsing. After sending or receiving `CONNECTION_CLOSE`, an endpoint
MUST NOT initiate streams or send additional application frames.

Connection error code zero means a graceful application close. Code one means
the Rust runtime detected a protocol, resource, or idle-timeout failure. Other
codes are reserved for future versions and applications MUST NOT assign them in
version 1.

### OPEN_STREAM (`0x04`)

```text
Type (0x04) | Stream ID (varint)
```

`OPEN_STREAM` creates one bidirectional stream. It MUST precede every `STREAM`
or `RESET_STREAM` frame carrying that stream ID.

### SETTINGS (`0x05`)

```text
Type (0x05) | Initial Max Data (varint) |
Initial Max Stream Data (varint) | Max Frame Size (varint)
```

SETTINGS MUST be the first frame and MUST occur exactly once in each direction.
Initial credit MAY be zero. Max Frame Size counts the inner frame, excluding the
record prefix, and MUST be between 25 and `2^32 - 1`. No DATA credit is available
before receiving SETTINGS.

### MAX_DATA (`0x06`)

```text
Type (0x06) | Maximum Data (varint)
```

Maximum Data is the absolute cumulative payload limit for the sending direction
of the entire connection, including retired streams.

### MAX_STREAM_DATA (`0x07`)

```text
Type (0x07) | Stream ID (varint) | Maximum Stream Data (varint)
```

Maximum Stream Data is the absolute cumulative payload limit for the peer's
send direction of that stream. Duplicate or decreasing credit updates are
ignored. Updates for unopened streams are errors; updates for retired,
previously issued streams are ignored. See [FLOW_CONTROL.md](FLOW_CONTROL.md)
for accounting and bounded update rules.

### STOP_SENDING (`0x08`)

```text
Type (0x08) | Stream ID (varint)
```

STOP_SENDING requests that the peer stop its send direction. The peer discards
unscheduled payload and sends RESET_STREAM unless FIN or RESET_STREAM was
already dispatched. Duplicate requests and requests for retired streams are
ignored; requests for unopened streams are errors.

## Stream identifiers

Client-initiated stream IDs are even and begin at 0. Server-initiated stream IDs
are odd and begin at 1. Each endpoint increments its own stream ID by 2.

An endpoint MUST announce its stream IDs in exact ascending sequence. Skipped,
repeated, wrong-parity, and previously retired IDs are connection errors. The
largest representable even ID is `2^62 - 2`; the largest representable odd ID
is `2^62 - 1`. Once its identifier space is exhausted, an endpoint cannot open
another stream on that connection.

The initiating endpoint sends `OPEN_STREAM`; both endpoints may then send
`STREAM` frames for the stream. Each direction has an independent lifecycle:

```text
                  STREAM
                 +------+
                 |      v
OPEN_STREAM -> Open ---------> Finished
                 |    FIN
                 |
                 +------------> Reset
                    RESET_STREAM
```

`Finished` and `Reset` are terminal for that sender's direction. A stream is
fully retired after both directions are terminal. Receiving stream data or
another terminal frame for a terminal direction is a connection error.

## Connection state and error handling

After TLS and ALPN negotiation, each endpoint sends SETTINGS as its first
frame. A peer frame before SETTINGS or duplicate SETTINGS is a connection
error. Application payload is sent only after peer settings have arrived and
both stream and connection credit permit it. Sending or receiving
`CONNECTION_CLOSE`, a framing failure, a protocol violation, a configured
resource-limit violation, transport EOF, or a local idle timeout moves the
connection to `Closed`.

For a locally detected version 1 protocol or resource violation, an endpoint
SHOULD send `CONNECTION_CLOSE` with code one when transport state permits, then
close the TLS/TCP connection. A peer MUST NOT depend on receiving the diagnostic
close frame because abrupt transport loss is always possible.

## Flow control and resource limits

Every payload byte MUST fit within both the stream's and the connection's
advertised absolute limits. Consumption or explicit discard returns receive
capacity; the endpoint advertises additional credit through MAX_DATA and
MAX_STREAM_DATA. Credit arithmetic MUST NOT wrap. Empty FIN and control frames
do not consume DATA credit. A legitimate temporary lack of credit blocks DATA
transmission rather than terminating the connection.

[FLOW_CONTROL.md](FLOW_CONTROL.md) defines accounting, bounded update rules,
scheduling fairness, configurable resource limits, and the runtime's finite
graceful shutdown and cancellation contracts. It forms part of this version 1
specification.

## Compatibility policy

Version 1 receivers reject unknown frame types and nonzero reserved flags.
Consequently, new frame types or flag meanings cannot be added compatibly
without a future negotiation mechanism. Implementations using `muxtls/1` MUST
follow the frame layouts and state rules in this document.

Editorial clarifications and additional conformance vectors may be added
without changing the version when they do not alter accepted wire behavior.

## Conformance vectors

Machine-readable canonical encodings and rejection cases are stored in
[`test-vectors/v1.json`](test-vectors/v1.json). Hexadecimal strings contain no
separators and use network byte order. The Rust codec verifies these vectors in
its integration test suite.
