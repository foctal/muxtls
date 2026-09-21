# muxtls/1 flow control and cancellation

This document defines the receive-credit accounting, resource limits, and
runtime lifecycle contracts for protocol version 1. The record framing,
integer encoding, frame layouts, and connection state rules are specified in
[PROTOCOL.md](PROTOCOL.md). Peers MUST negotiate `muxtls/1`.
Conformance vectors for all frame types are in `test-vectors/v1.json`.

## Frames

All fields below are unsigned QUIC variable-length integers (at most 2^62-1).
Control frames consume no DATA credit.

| Type | Name | Fields after type |
| --- | --- | --- |
| 0x05 | SETTINGS | Initial Max Data, Initial Max Stream Data, Max Frame Size |
| 0x06 | MAX_DATA | Maximum Data |
| 0x07 | MAX_STREAM_DATA | Stream ID, Maximum Stream Data |
| 0x08 | STOP_SENDING | Stream ID |

SETTINGS MUST be the first frame and MUST occur exactly once in each direction.
Initial credits MAY be zero. Max Frame Size MUST be between 25 and 2^32-1,
including the inner frame header, excluding the four-byte record prefix.
Local default configurations advertise positive windows. Before receiving
SETTINGS, a sender has zero DATA credit. A stream starts with the peer's initial
stream limit, regardless of which endpoint opened it. OPEN MUST precede all
controls and DATA referencing that stream.

MAX_DATA and MAX_STREAM_DATA are absolute lifetime payload limits, not deltas.
Duplicate or decreasing updates MUST be ignored. An update for a future,
unopened stream is a connection error. An update for a retired, previously
issued stream is ignored: delayed updates can legitimately cross FIN/RESET.
Issued IDs are validated by parity and the next-issued counter; no unbounded
retired-stream set is needed. A peer limit never causes allocation or increases
local buffer budgets.

## Accounting and ordering

Each sending direction tracks total serialized payload bytes. Connection
accounting is the sum across every stream, including retired streams. DATA MUST
satisfy both the connection and stream limits. The sole writer spends credit
when taking DATA from a queue for serialization, not when application code
queues a chunk. It splits chunks at the remaining credit and peer frame limit.
No sender may reserve all connection credit while waiting for stream credit.
FIN with no payload requires no credit; it remains ordered after queued DATA.

The receiver MUST check both cumulative end offsets before accepting payload.
Exceeding either limit is a connection error, without wrapping an integer.
Updates saturate at 2^62-1. Reaching this lifetime limit prevents further DATA;
applications must replace that connection. It cannot wrap into new credit.

RESET discards unscheduled local DATA and queues one reset frame. DATA already
handed to the writer remains before RESET on the ordered transport and counts
at both endpoints. There is no final-offset ambiguity because unscheduled DATA
never spent peer credit. A dispatched FIN cannot be replaced by RESET.

Dropping an unfinished receive direction discards its buffered bytes and queues
STOP_SENDING. Receiving STOP_SENDING cancels pending writes, discards unscheduled
DATA, and sends RESET unless FIN or RESET was already dispatched. No further
stream credit is advertised for a discarded receive direction. Connection
credit is returned for all discarded bytes, including already authorized DATA
arriving before the peer observes STOP_SENDING. Duplicate STOP_SENDING is safe;
unknown future stream IDs are errors, and retired IDs are ignored.

## Consumption and bounded updates

Received bytes remain charged while in the inbound queue or an AsyncRead
remainder. Copying bytes to the caller, returning a chunk, or explicit discard
returns the corresponding permits and increments consumed totals exactly once.
A receive window advertises `min(consumed + capacity, 2^62-1)` after at least
half a window has been consumed since the last update. If advertised credit is
exhausted, any newly freed capacity triggers an update, even below the normal
threshold: otherwise one active stream smaller than half the connection window
could deadlock behind stalled streams. Receive and consumption transitions both
wake the writer when this exception applies. This can require a credit frame
per read for a peer making one-byte progress at exhaustion; that traffic is
necessary for progress and still coalesces in one bounded window state.
No timers or per-read frame allocations are needed. The final
saturating update is allowed below the half-window threshold.

Credit updates are synthesized from window state, rather than appended to an
unbounded queue. There is one connection window and at most one registered
window per admitted stream. Ordinary controls have an explicit finite capacity;
PING coalesces. At least `3 * max_open_streams + 2` control slots are required
for OPEN, RESET, STOP_SENDING and SETTINGS/PING. Arithmetic is checked.

The writer round-robins eligible streams and skips uncredited streams. Control
work alternates with DATA; queued OPEN and SETTINGS precede dependent frames.
A blocked stream cannot monopolize scheduling or reserve connection credit.
Isolation requires connection capacity larger than the stalled stream's window;
no protocol can provide progress after all shared capacity is genuinely occupied.
TCP packet loss and a peer that stops reading the entire socket still cause
connection-wide head-of-line blocking.

Payload semaphores and explicit connection/stream outbound frame-count
semaphores bound queues. Each stream gets at most
`max_queued_outbound_frames / max_open_streams` slots; validation requires at
least one slot per stream. A stalled stream cannot occupy the entire metadata
budget with one-byte writes.
Small inbound frames coalesce into pages of at most 16 KiB (or one configured
stream window when smaller); frames of at least half a page retain zero-copy
ownership. Queue metadata is therefore bounded by page count plus active
streams, rather than one entry per peer-controlled byte. An unfinished page
can reserve at most one page of spare capacity per stream. Read-chunk boundaries
are implementation-defined byte boundaries, not preserved message boundaries. Empty non-FIN DATA is not queued. Incoming stream handles are
bounded by `max_open_streams`. Decoded/sending frames add at most one configured
frame per I/O direction beyond queued byte budgets. Application-owned returned
buffers and retained stream handles remain the application's responsibility.

## Local lifecycle and API contracts

`close` publishes graceful shutdown atomically, rejecting new operations and
waking admission waiters. The connection-owned supervisor applies
`Limits::drain_timeout` (default five seconds; configurable in (0, one hour]).
It drains queued DATA/controls, then sends CLOSE. If the peer will not read or
will not provide credit, the deadline aborts I/O. Cancellation of `close` before
publication does nothing; after publication shutdown belongs to the supervisor.
Dropping the last connection handle invokes the same finite drain policy.
`abort` requests immediate termination. `wait_closed` waits for the joined
reader, writer, keepalive and stream-cleanup task group and transport release.
It does not wait for application-owned stream handles to be dropped.

Stream cleanup uses one connection-owned worker, not one spawned task per drop.
Accept does not hold a queue lock while awaiting arrival; a suspended accept
future cannot block shutdown. No dequeue-to-return cancellation point exists.
Open acquires admission/queue resources before assigning an ID and publishing
OPEN. Cancellation before publication cannot leak IDs or permits.

Each stream direction selects chunk operations or AsyncRead/AsyncWrite at first
use. Mixing them returns an explicit error. Stream halves are not Clone: one
AsyncWrite buffer and one AsyncRead remainder have unique owners, preventing
another clone from overtaking accepted data with FIN or abandoning a remainder. Flushing
does not change that selection. AsyncWrite owns and acknowledges at most one
bounded chunk per handle; a subsequent call drains that chunk before accepting
new bytes and never reports an old buffer's length. Flush publishes accepted
bytes to the bounded connection queue; it does not promise peer consumption.
Use AsyncWrite shutdown after AsyncWrite, and `finish` after chunk writes.
Dropped unfinished send handles reset the direction. A successfully queued FIN
survives handle drop. Explicit reset can cancel a queued, undispatched FIN.
Received bytes and FIN remain readable through a retained receive handle after
connection shutdown; clearing connection bookkeeping must not discard bytes
already accepted for an application. EOF and reset remain terminal. Cancelling a pending read consumes no data.
EOF/terminal completion and protocol offsets do not depend on application
futures being repolled after shutdown.

## Verification

The runtime regressions cover stalled-stream fairness, smaller peer windows,
partial-read accounting, reset ordering, STOP_SENDING, duplicate/extreme credit,
cancelled publication, blocked-operation release, simultaneous close and joined
transport teardown. Window tests cover coalescing, exact limits and saturation.
The state-machine fuzzer includes flow-control frames and raw malformed frames. These
bounded tests are evidence, not a proof of universal correctness.
