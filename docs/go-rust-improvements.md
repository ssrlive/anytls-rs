# Go-to-Rust Behavior Improvements

This document records the behavior that the Rust implementation preserves from the current Go implementation and the deliberate improvements added for usability and protocol safety.

## Search Terms

`max_streams_per_session`, `Session ID`, `stream ID`, `SID`, `idle session pool`, `logical stream capacity`, `FIN`, `SYN`, `SYNACK`, `cmdWaste`, `Go compatibility`, `Rust translation`.

## Session Identity

Each client allocates a unique Session ID with an atomic `AtomicUsize` counter. IDs start at `0` and advance toward `usize::MAX`.

The Session ID is an implementation-level identity used by the client session pool. It is not serialized into the AnyTLS wire protocol.

The Go implementation uses an atomic `uint64` sequence and starts at `1`. The Rust implementation intentionally starts at `0` to follow the repository requirement that Session IDs grow from `0` through `usize::MAX`.

## Logical Stream IDs

A Session allocates logical stream IDs with an atomic `AtomicU32` counter. The first data stream has SID `1`.

- `sid = 0` is reserved for control frames.
- `sid > 0` identifies a data stream.
- Data commands are `SYN`, `PSH`, `FIN`, and `SYNACK`.
- Control commands include `Settings`, `Alert`, padding updates, server settings, and heartbeat commands.

The Rust implementation rejects a frame whose SID does not match its command class. The Go implementation allocates the same non-zero SID sequence but does not enforce every SID class at the receive boundary.

## Maximum Streams Per Session

The Rust implementation exposes `max_streams_per_session` on `Client`, `Session::new_client`, and `Session::new_server`.

Values smaller than `1` are normalized to `1`. This prevents a Session from being created with no usable logical stream capacity.

The limit applies to active logical data streams on one Session:

```text
active_streams < max_streams_per_session
```

When the limit is reached:

- A direct `Session::open_stream` returns `WouldBlock`; `Client::create_stream` treats this as a capacity race and retries Session selection instead of closing the selected Session.
- The Client selects another Session with available capacity, if one exists.
- If no existing Session has capacity, the Client creates a new Session.
- A server receiving an extra `SYN` sends `SYNACK` with `session stream limit reached`.

`max_streams_per_session = 1` prevents multiple concurrent logical streams from sharing one Session, but it does not disable reuse of an idle Session. After its only stream closes, that Session can still return to the idle pool and serve a later stream.

This is a deliberate improvement over the current Go implementation, which has no explicit per-Session stream limit and can keep allocating `uint32` SIDs until resource exhaustion or SID wraparound.

## Session Selection

The Rust Client chooses a Session in this order:

1. Reuse the oldest fully idle Session.
2. Reuse the oldest existing Session with available stream capacity.
3. Create a new Session.

The Session ID is used as the ordering key for this selection; the smallest Session ID is selected first.

The Rust Client also applies a configurable maximum Session age through `Client::new`. An expired Session is excluded from new-stream selection. Existing logical streams are not interrupted; the Session is allowed to drain, and an idle expired Session is closed by the cleanup path.

## Session Reuse Policy

The Rust Client intentionally has no separate `disable_reuse` option. The Go option combines two unrelated policies: it prevents idle Session reuse and also closes the entire Session when the logical stream closes. Rust keeps these responsibilities separate:

- `max_streams_per_session` controls only the number of concurrent logical streams carried by one Session.
- The idle pool controls whether an otherwise healthy Session can be reused after its streams close.
- A value of `max_streams_per_session = 1` prevents concurrent stream sharing, but does not disable reuse of an idle Session.

This is an intentional Rust design difference, not a missing compatibility feature. It avoids forcing a new transport connection merely to limit per-Session concurrency and keeps resource policy independent from stream capacity.

## Idle Session Pool

A Session enters the idle pool only after its active logical stream count becomes zero.

Both closure paths perform the capacity check:

- Local `Stream::close` sends `FIN`, removes the stream, and checks whether the Session has no remaining streams.
- Dropping a `Stream` schedules the same cleanup on the Tokio runtime, removes the stream without waiting for the peer FIN, and then best-effort sends `FIN`.
- Remote `FIN` closes the local read half after queued data drains. The stream remains available for reverse-direction writes until the local side also sends `FIN`; then it is removed and the idle-pool capacity check runs.
- Remote `SYNACK` failure closes the local stream endpoint, removes the stream, and performs the same check.

A Session with one remaining active stream is not placed in the idle pool when another stream closes.

The pool deduplicates Session references, so closing multiple streams cannot enqueue the same Session more than once.

## FIN Half-Close

`FIN` is a one-way EOF marker, matching the protocol and Go implementation. Receiving `FIN` queues EOF behind all previously received `PUSH` data and shuts down only the local read half after that data drains. It does not reply with another `FIN`, and it does not prevent the local application from sending a response in the opposite direction.

Each side sends its own `FIN` when its write half closes. The stream is removed only after both local and remote FINs have been observed and queued inbound data has drained. `StreamIo::poll_shutdown` sends the local FIN while preserving reads, which lets bidirectional relays propagate TCP half-closes correctly.

Because Rust `Drop` cannot await, automatic cleanup requires an active Tokio runtime. Explicit `Stream::close().await` sends FIN and performs deterministic local cleanup.

## Preserved Go Behavior

The Rust translation preserves these important Go behaviors:

- Settings are sent before opening data streams.
- Client Settings and the first `SYN` are buffered briefly, then flushed immediately after `SYN` is queued; they do not require a first data write.
- The first data stream starts at SID `1`.
- `FIN` closes one logical stream direction without closing the whole Session; the stream is released after both FIN directions complete.
- `SYNACK` is sent at most once per stream handshake.
- Padding packet counting is tied to transport writes.
- The Session remains reusable while it is alive and has capacity.

## Compatibility Boundary

`max_streams_per_session` is a local resource-management policy. It does not change the AnyTLS frame format or command values. A Rust endpoint with a limit can still communicate with a Go endpoint; the Rust endpoint may reject additional logical streams when its configured capacity is full.

The Rust implementation also performs stricter SID validation than Go. Valid Go-produced frames remain compatible, while malformed control/data SID combinations are rejected earlier.

## Verification

The behavior is covered by Rust tests for:

- Settings, SYN, PSH, and SYNACK exchange.
- Session reuse.
- Multiple streams on one Session.
- Returning a Session to the idle pool only after the last stream closes.
- Frame SID and command compatibility.

Recommended checks:

```text
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```
