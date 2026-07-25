# Protocol-Level Flow Control Design

This document specifies the flow-control design for a future `muxtls/2` wire
protocol. It is intentionally not added to `muxtls/1`: version 1 rejects
unknown frame types and has no extension negotiation, so adding the required
frames under the existing ALPN identifier would be incompatible.

## Goals

- Bound unread data independently for each stream.
- Bound total unread data for a connection.
- Backpressure one stream without blocking frame processing for other streams.
- Keep control frames sendable when all data credit is exhausted.
- Avoid credit-reclamation ambiguity after reset or stream retirement.
- Permit different receive limits at each endpoint.

Flow control limits application payload bytes only. Frame headers, TLS records,
and implementation queues require separate bounded-memory limits.

## Version negotiation

An implementation of this design MUST negotiate the ALPN identifier
`muxtls/2`. A version 1 endpoint MUST continue to use `muxtls/1` and the
disconnect-on-buffer-exhaustion behavior defined by the version 1 protocol.

Version 2 retains the version 1 record prefix, integer encoding, stream ID
allocation, and frame layouts unless this document changes them.

## Flow-control frames

The following frame type assignments are reserved for version 2.

| Type | Name | Encoding |
| ---: | --- | --- |
| `0x05` | `SETTINGS` | Type, Initial Max Data, Initial Max Stream Data |
| `0x06` | `MAX_DATA` | Type, Maximum Data |
| `0x07` | `MAX_STREAM_DATA` | Type, Stream ID, Maximum Stream Data |

All numeric fields are variable-length integers. Flow-control frames are
control frames and are not themselves subject to flow-control credit.

### SETTINGS

Each endpoint MUST send exactly one `SETTINGS` frame as its first inner frame.
Receiving any other frame before `SETTINGS`, or receiving a second `SETTINGS`,
is a connection error.

`Initial Max Data` is the total number of stream payload bytes the peer may
send across the connection. `Initial Max Stream Data` is the number of payload
bytes the peer may initially send on each stream, regardless of which endpoint
opened it. Both values are absolute offsets and MAY be zero.

An implementation SHOULD send limits no larger than its actual inbound buffer
capacity. Local connection and stream limits continue to apply even if a peer
advertises larger values for the opposite direction.

### MAX_DATA

`Maximum Data` increases the absolute connection-level payload offset the peer
may send. Values lower than or equal to the previously advertised maximum have
no effect and MUST NOT be treated as errors.

### MAX_STREAM_DATA

`Maximum Stream Data` increases the absolute payload offset the peer may send
on the identified stream. The stream MUST already have been announced with
`OPEN_STREAM`. A frame for an unknown or retired stream is a connection error.
Values lower than or equal to the previously advertised maximum have no effect.

## Accounting

Every stream direction has a zero-based sent offset and advertised maximum.
The end offset of a `STREAM` frame is:

```text
previous stream end offset + Payload Length
```

A sender MUST NOT emit a frame whose end offset exceeds the peer's current
stream maximum. Across all streams, the sum of every payload byte ever sent
MUST NOT exceed the peer's current connection maximum.

A receiver MUST validate both limits before making payload available to the
application. Exceeding either advertised maximum is a connection error.

Payload bytes count once when received. They remain counted at connection
level after FIN, reset, discard, or stream retirement. This prevents a peer
from resetting streams to reclaim credit for bytes the receiver did not
consume. Empty `STREAM` frames and FIN consume no credit.

## Credit updates and application reads

Credit is returned only after the application consumes or explicitly discards
buffered payload. Implementations SHOULD use a sliding window:

1. Track consumed connection and stream offsets.
2. When remaining credit falls below an implementation-defined threshold,
   advance the advertised maximum by the amount consumed.
3. Queue `MAX_DATA` or `MAX_STREAM_DATA` without waiting for data-frame credit.

Updates SHOULD be coalesced to avoid one control frame per application read.
An implementation MUST continue processing control frames while a stream has
no send credit. A blocked write MUST wake when relevant credit increases or
the stream or connection becomes terminal.

When the receive handle is discarded, the implementation MAY discard buffered
payload and advertise corresponding connection credit. It MUST NOT advertise
additional stream credit for that discarded direction.

## Concurrency and ordering

Credit updates are monotonically increasing, so reordered writer-queue
operations cannot reduce credit. The sender MUST reserve connection and stream
credit atomically with respect to other writes before enqueueing a `STREAM`
frame. Canceling before enqueue releases the reservation; canceling after
enqueue does not.

Connection close, stream reset, and FIN retain the version 1 terminal ordering
rules. Flow-control waiters MUST be woken on all terminal transitions.

## Required conformance coverage

A version 2 implementation is not complete until tests cover:

- zero initial windows followed by credit updates;
- exact-boundary writes and one-byte connection and stream overruns;
- independent progress on a credited stream while another is blocked;
- duplicate and decreasing updates;
- unknown and retired stream IDs in `MAX_STREAM_DATA`;
- FIN and reset while a write is waiting for credit;
- aggregate connection accounting across stream churn;
- discarded unread data and credit coalescing;
- cancellation before and after credit reservation;
- maximum variable-length integer offsets and overflow handling.

