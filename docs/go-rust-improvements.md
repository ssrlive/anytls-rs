# Go-to-Rust Behavior Improvements

This document records the behavior that the Rust implementation preserves from the current Go implementation and the deliberate improvements added for usability and protocol safety.

## Search Terms

`Session ID`, `stream ID`, `SID`, `single-stream Session`, `idle session pool`, `FIN`, `SYN`, `SYNACK`, `cmdWaste`, `Go compatibility`, `Rust translation`.

## Session Identity

Each client allocates a local Session ID with an atomic `AtomicUsize` counter. IDs start at `0` and advance toward `usize::MAX`.

Rust includes its local Session ID in `cmdSettings` and returns its local ID in `cmdServerSettings`. The IDs let each endpoint correlate logs using `local_session` and `peer_session`; they are not global identities and do not identify a session across reconnects. The fields are optional for compatibility with older peers.

The Go implementation uses an atomic `uint64` sequence and starts at `1`. The Rust implementation intentionally starts at `0` to follow the repository requirement that Session IDs grow from `0` through `usize::MAX`.

## Client ID and Panel Authorization

The Rust client optionally carries a client UUID in the first 36 bytes of the authentication padding. This is an AnyTLS-RS extension; it does not change password authentication, and clients that omit the UUID remain compatible when server panel synchronization is disabled.

When panel synchronization is enabled, the Rust server requires the UUID to identify an enabled client on the configured panel node. It checks the client after authentication and again before relaying each logical stream. TCP and UOT traffic is counted against that client for panel synchronization. Without panel synchronization, the UUID does not gate access.

## Logical Stream IDs

The Rust process uses one atomic `AtomicU32` counter to allocate logical stream IDs. Allocation increments the `u32` sequence, wraps after `u32::MAX`, and skips `0`, so the first ID is `1` and no data stream is assigned the control-frame SID.

- `sid = 0` is reserved for control frames.
- `sid > 0` identifies a data stream.
- Data commands are `SYN`, `PSH`, `FIN`, and `SYNACK`.
- Control commands include `Settings`, `Alert`, padding updates, server settings, and heartbeat commands.

The allocator is an implementation detail; the wire protocol associates each data SID with a stream in its Session. IDs are eventually reused after the `u32` sequence wraps.

The Rust implementation rejects a frame whose SID does not match its command class. The Go implementation also uses non-zero SIDs, but does not enforce every SID class at the receive boundary.

## Session Stream Policy

The Rust implementation permits one active logical data stream per Session. A client creates an independent Session for each concurrently active flow; it does not scan active Sessions for spare stream capacity. A server rejects an additional `SYN` while its Session already has an active stream.

When a stream closes, its Session remains open and can return to the idle pool. A later flow can reuse that Session, so single-stream operation does not imply one new transport connection per flow.

The wire protocol's SID format is unchanged. Stream IDs still come from one process-global atomic counter, increment with wraparound, and skip `0`, which remains reserved for control frames.

## Session Selection

The Rust Client chooses a Session in this order:

1. Reuse the oldest fully idle Session.
2. If no Session is idle, create a new Session for this flow.

The Session ID is used as the ordering key for this selection; the smallest Session ID is selected first.

The Rust Client also applies a configurable maximum Session age through `Client::new`. An expired Session is excluded from new-stream selection. Existing logical streams are not interrupted; the Session is allowed to drain, and an idle expired Session is closed by the cleanup path.

`Session::new_client` and `Session::new_server` require an explicit maximum age; `DEFAULT_MAX_SESSION_AGE` is one hour, and zero disables age expiration. Once expired, a Session rejects new outgoing and incoming streams, drains active streams, and shuts down when idle.

## Stalled Connection Recovery

Each flow reserves an idle Session or dials independently; there is no client-wide lock serializing stream allocation or Session creation. A stalled SYN write therefore cannot hold an allocation lock, and a stalled dial does not prevent another request from reusing an idle Session or starting its own dial. Each new dial has a 15-second deadline.

Transport write requests have a 15-second deadline. A timeout closes the whole Session because a partially written frame cannot safely be resumed on that transport. Session shutdown cancels an in-flight write and releases both transport halves, even when a closed Session remains referenced by the pool.

After a client receives version-2 server settings, it sends a heartbeat every 15 seconds. Each probe has a 10-second deadline covering both transmission and the response. Missing responses close the Session; version-1 peers are not proactively probed. Control replies are queued without waiting for transport writes in the receive loop. A full control queue closes the Session instead of blocking that loop or silently dropping a required reply.

The command-line client and server resolve proxy addresses asynchronously. The server gives destination resolution and TCP connection establishment a combined 15-second deadline and reports failure through SYNACK when supported.

Closing a failed Session interrupts its existing streams. Later requests can establish a new Session; existing TCP streams are not replayed or migrated automatically.

## Session Reuse Policy

The Rust Client intentionally has no separate `disable_reuse` option. The Go option combines two unrelated policies: it prevents idle Session reuse and also closes the entire Session when the logical stream closes. Rust keeps these responsibilities separate:

- Each Session carries at most one active logical stream.
- The idle pool controls whether an otherwise healthy Session can be reused after its stream closes.

This is an intentional Rust implementation policy. It avoids multiplex-specific active-session selection and coordination while preserving transport reuse between flows.

## Idle Session Pool

A Session enters the idle pool after its only active logical stream closes.

Both closure paths check whether the Session can return to the pool:

- Local `Stream::close` sends `FIN`, removes the stream, and checks whether the Session has no remaining streams.
- Dropping a `Stream` schedules the same cleanup on the Tokio runtime, removes the stream without waiting for the peer FIN, and then best-effort sends `FIN`.
- Remote `FIN` closes the local read half after queued data drains. The stream remains available for reverse-direction writes until the local side also sends `FIN`; then it is removed and the Session may return to the idle pool.
- Remote `SYNACK` failure closes the local stream endpoint, removes the stream, and performs the same check.

The idle pool deduplicates Session references, so a Session is not enqueued more than once.

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
- A Session is reused only while idle; each active Session carries one data stream.

## Compatibility Boundary

One-stream-per-Session is a local implementation policy; it does not change the AnyTLS frame format, command values, or global SID allocation. A Rust endpoint can communicate with a Go endpoint, but a Rust server rejects an additional logical stream on a Session that is already active.

The Rust implementation also performs stricter SID validation than Go. Valid Go-produced frames remain compatible, while malformed control/data SID combinations are rejected earlier.

## Verification

The behavior is covered by Rust tests for:

- Settings, SYN, PSH, and SYNACK exchange.
- Session reuse.
- Rejection of a second active stream on one Session.
- Returning a Session to the idle pool after its stream closes.
- Frame SID and command compatibility.

Recommended checks:

```text
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```
