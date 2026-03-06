# Slipstream Rust Plus

Slipstream Rust Plus is a DNS tunnel that carries QUIC traffic over DNS queries and responses.

## Build

```bash
git clone https://github.com/Fox-Fig/slipstream-rust-plus.git
cd slipstream-rust-plus
git submodule update --init --recursive
cargo build -p slipstream-client -p slipstream-server --release
```

## Quick Local End-to-End Test

1. Start a local backend:

```bash
python3 -m http.server 18080 --bind 127.0.0.1
```

2. Start tunnel server:

```bash
./target/release/slipstream-server \
  --dns-listen-host 127.0.0.1 \
  --dns-listen-port 8853 \
  --target-address 127.0.0.1:18080 \
  --domain ns13.maila.ai \
  --cert fixtures/certs/cert.pem \
  --key fixtures/certs/key.pem \
  --reset-seed .interop/reset-seed
```

3. Start tunnel client:

```bash
./target/release/slipstream-client \
  --tcp-listen-host 127.0.0.1 \
  --tcp-listen-port 7000 \
  --authoritative 127.0.0.1:8853 \
  --domain ns13.maila.ai \
  --cert fixtures/certs/cert.pem
```

4. Verify tunnel traffic:

```bash
curl http://127.0.0.1:7000/
```

## DNS Transport Behavior (Current)

- QNAME payload encoding is `base32` (not base64 yet).
- Recursive paths (`--resolver`) send DNS `A` queries.
- Authoritative paths (`--authoritative`) send DNS `AAAA` queries.
- Server accepts `A`, `AAAA`, and `TXT` query types for transport decoding.
- Server replies using the same RR type as the query.
- `A` and `AAAA` responses use sequence-framed chunking so clients can decode correctly even if resolvers reorder answer RRs.
- For authoritative mode only, large payloads may switch to EDNS0 OPT payload mode.
- Server QUIC MTU is computed from configured DNS domain capacity (no longer fixed to 900).

## Public Recursive Resolver Testing Notes

When testing with public resolvers (`--resolver 8.8.8.8`, etc.), make sure all of the following are true:

- Delegation is correct for the zone that contains your tunnel domain.
- Authoritative DNS service is reachable on UDP/53 from the resolver.
- Firewall/NAT rules forward UDP/53 traffic to your tunnel DNS listener (for example, local redirect from 53 to 5300).
- `--domain` exactly matches what is delegated and what your server is configured to serve.

## Documentation

- [docs/usage.md](docs/usage.md)
- [docs/protocol.md](docs/protocol.md)
- [docs/dns-codec.md](docs/dns-codec.md)
- [docs/config.md](docs/config.md)
- [docs/interop.md](docs/interop.md)

## License

This project is licensed under GPLv3. See `LICENSE`.

4.2.2.1
4.2.2.2
4.2.2.3
4.2.2.4
4.2.2.5
4.2.2.6