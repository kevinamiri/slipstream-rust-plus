# Design notes

This document summarizes the major design goals and architecture choices for the
Rust implementation.

## Goals

- Preserve external behavior and wire compatibility where feasible.
- Improve safety by minimizing unsafe code and isolating FFI boundaries.
- Maintain or improve performance relative to the C implementation.

## Architecture (Rust workspace)

Crates are organized so core logic and performance-sensitive code are isolated:

- slipstream-core: shared types, parsing, and TCP helpers.
- slipstream-dns: DNS codec, base32 QNAME encoding, and transport RR framing (`TXT`/`A`/`AAAA`).
- slipstream-ffi: picoquic FFI bindings and runtime helpers.
- slipstream-client: CLI and client runtime.
- slipstream-server: CLI and server runtime.

## QUIC and multipath

Multipath QUIC support is provided by picoquic. The Rust code uses an FFI wrapper
so the application logic is in Rust while keeping multipath behavior intact.
Unsafe code is constrained to the FFI layer, and higher-level APIs avoid raw
pointer exposure where possible.

## DNS codec

The DNS codec is intentionally minimal and treats parsing as an attack surface:

- Strict bounds checks on message length and label lengths.
- Hard caps on decoded payload sizes.
- Explicit error handling with drop vs reply behavior.
- Reorder-safe `A`/`AAAA` response reassembly via explicit sequence indexes.

Golden vectors (`fixtures/vectors/dns-vectors.json`) are treated as the source of
truth for DNS behavior.

## Event loop and concurrency

The runtime centers around a connection manager that owns QUIC state, timers, and
per-connection queues. UDP receive/send and TCP accept/read/write are handled by
separate tasks, with bounded channels used to limit memory growth under load.

## Flow control strategy

Slipstream needs to satisfy two competing cases:

- Slow, long-lived single-stream transfers should behave like TCP. We want
  application backpressure to propagate to the sender so large uploads don't get
  aborted just because the target is slow.
- If a second stream appears, a stalled or blackholed stream must not be able to
  exhaust the connection-level window and block new streams. This showed up in
  practice as "new TCP connections hang" even though the QUIC connection is
  still alive.

To cover both:

- In single-stream mode, we rely on TCP-style backpressure (consume after TCP
  writes) but keep a small reserve window (`SLIPSTREAM_CONN_RESERVE_BYTES`) so
  a new stream can always send its first bytes and trigger mode switch.
- Once multiple streams are active, we switch to consume-on-receive and enforce
  per-stream caps (`SLIPSTREAM_STREAM_QUEUE_MAX_BYTES`). If a stream exceeds its
  cap, we send `STOP_SENDING` and discard further data for that stream while
  continuing to consume, which prevents connection-wide stalls.

## Rust vs C behavior notes

- Client path mode affects DNS transport type in Rust runtime:
  - recursive: `A`
  - authoritative: `AAAA` (and EDNS0 for larger packets)
- Authoritative polling follows picoquic pacing rate (bytes/sec) converted to
  queries per second using DNS payload size; cwnd remains a fallback.
- When QUIC has ready stream data queued, the Rust client suppresses extra polls
  to prioritize data-bearing queries unless flow control blocks progress.
- When server has no QUIC payload ready for a poll, Rust server returns empty
  `NOERROR` to clear poll backlog instead of `NXDOMAIN`.

## Safety and shutdown

- CLI validation enforces required flags and valid host:port parsing.
- Backpressure uses connection-level max_data with a single-stream reserve and
  per-stream caps in multi-stream mode to avoid global stalls.
- Shutdown follows explicit states (drain, close, force terminate) to avoid hangs
  and minimize data loss.

## Performance strategy

- Measure first with benchmark harnesses (see docs/benchmarks.md).
- Reuse buffers and avoid per-packet allocations in the hot path.
- Keep the DNS codec simple and predictable.
- Make logging configurable and avoid hot-path overhead by default.

## Testing and interop

- DNS codec behavior is validated against golden vectors.
- Interop harnesses ensure Rust <-> C compatibility.
- Integration tests cover local loopback and shutdown behavior.
