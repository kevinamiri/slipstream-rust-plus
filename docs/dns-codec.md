# DNS codec and vectors

This document describes the current DNS codec behavior and test vectors.

## Canonical behavior

- QNAME payload encoding uses base32 with inline dots.
- Query decode accepts `TXT`, `A`, and `AAAA` qtypes.
- Unsupported qtypes are rejected with `NXDOMAIN`.
- Decoder supports two payload sources:
  - QNAME subdomain payload
  - EDNS0 OPT RDATA payload
- Response encode/decode supports `TXT`, `A`, and `AAAA` payload carriers.
- `A`/`AAAA` response decoding is reorder-safe via explicit per-chunk sequence indexes.

### Response chunk framing

- `TXT`: raw payload in TXT char-strings.
- `A`: `[len:u16be][payload]` split into 3-byte chunks; each RR uses `[seq:u8][chunk:3]`.
- `AAAA`: `[len:u16be][payload]` split into 14-byte chunks; each RR uses `[seq:u16be][chunk:14]`.

## Vector fixtures

Golden vectors live in `fixtures/vectors/dns-vectors.json`.

Regenerate vectors:

```bash
./scripts/gen_vectors.sh
```

Set `SLIPSTREAM_DIR` if the C repo is not at `../slipstream`.

## Tests

```bash
cargo test -p slipstream-dns
```

This suite validates encode/decode behavior, error mapping, and parser drop cases.
