# Protocol

Slipstream encapsulates QUIC packets inside DNS traffic. Payloads are carried in QNAME labels or, for some authoritative-path packets, in EDNS0 OPT RDATA.

## Domain suffix matching

- Server is configured with one or more domains.
- QNAME must end with a configured suffix.
- If multiple configured domains match, the longest matching suffix is selected.

## QNAME payload encoding

- Encoding is base32 RFC4648 (uppercase alphabet, no padding).
- Decoder is case-insensitive and removes inline dots before decode.
- Dots are inserted every 57 characters to keep labels within DNS limits.

## Query format (client -> server)

- `QDCOUNT = 1`
- `QCLASS = IN`
- `RD = 1`
- `ARCOUNT = 1` with an OPT RR is always emitted by encoder
- Transport query type is mode-dependent in current client runtime:
  - Recursive resolver mode: `A`
  - Authoritative resolver mode: `AAAA`

### Payload location

- QNAME mode:
  - `QNAME = <base32(payload with inline dots)>.<domain>.`
- EDNS0 mode (authoritative path, large packet):
  - `QNAME = <domain>.`
  - payload bytes are stored in OPT RDATA

## Response format (server -> client)

- Mirrors query ID.
- `QR = 1`, `AA = 1`.
- `RD` and `CD` are copied from query.
- Same question echoed in Question section.
- `ARCOUNT = 1` OPT RR.
- Response RR type follows incoming query type.

### Payload framing by RR type

If payload is present and `RCODE = NOERROR`:

- `TXT`:
  - one TXT answer RR
  - payload split across DNS TXT character-strings
- `A`:
  - payload is framed as `[len:u16be][payload]`
  - split into 3-byte chunks
  - each A RR stores `[seq:u8][chunk:3]` (4 bytes total)
- `AAAA`:
  - payload is framed as `[len:u16be][payload]`
  - split into 14-byte chunks
  - each AAAA RR stores `[seq:u16be][chunk:14]` (16 bytes total)

When no payload is available, server runtime may return `NOERROR` with `ANCOUNT=0` to clear polls.

## Server decode rules

- If packet is a response (`QR=1`) or `QDCOUNT != 1`: reply `FORMERR`.
- If `QTYPE` is not one of `TXT`, `A`, `AAAA`: reply `NXDOMAIN`.
- If domain/suffix does not match configured domains: reply `NXDOMAIN`.
- If subdomain payload is empty: reply `NXDOMAIN`.
- If base32 decode fails: reply `SERVFAIL`.
- Parse failures may be dropped without reply.

## Client decode rules

Client accepts payload only when:

- `QR=1`
- `RCODE=NOERROR`
- `ANCOUNT > 0`
- all answers share one supported type (`TXT`, `A`, or `AAAA`)

For `A` and `AAAA` answers, chunks are sorted by sequence index and contiguity is validated before reassembly.

## Limits and sizing

- EDNS UDP payload advertisement: `1232`.
- Client MTU is derived from `max_payload_len_for_domain(domain)`.
- Server QUIC MTU is computed as the minimum `max_payload_len_for_domain` across configured domains.
- EDNS0 payload helper limit: `1232` bytes.

## References

- DNS codec implementation: `crates/slipstream-dns/src/codec.rs`
- DNS sizing helpers: `crates/slipstream-dns/src/lib.rs`
- Vectors: `fixtures/vectors/dns-vectors.json`
