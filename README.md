# AnyTLS-RS

[![CI](https://github.com/ssrlive/anytls-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/ssrlive/anytls-rs/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/anytls.svg)](https://crates.io/crates/anytls)
[![docs.rs](https://img.shields.io/docsrs/anytls)](https://docs.rs/anytls)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A Rust implementation of the [AnyTLS](https://github.com/anytls/anytls-go) proxy protocol that attempts to mitigate the TLS in TLS fingerprinting problem.

AnyTLS-RS provides a proxy solution that disguises proxy traffic as regular TLS connections,
making it harder to detect and block.

Implementation and review conventions for multiplexed sessions are documented in
[docs/coding_conventions.md](docs/coding_conventions.md).

## Features

- **TLS Obfuscation**: Masks proxy traffic as standard TLS connections
- **Flexible Padding**: Configurable packet splitting and padding strategies
- **Connection Reuse**: Reduces latency by reusing connections
- **Cross-Platform**: Supports Linux, macOS, and Windows
- **Certificate Support**: Optional custom TLS certificates for server and root CA for client
- **SOCKS5 + HTTP CONNECT**: Client can listen on a mixed SOCKS5 and HTTP CONNECT endpoint

## Installation

### From script (Linux)

For server

```bash
apt-get update && apt-get install -y curl openssl tar
curl -sSL https://raw.githubusercontent.com/ssrlive/anytls-rs/main/scripts/installer.sh -o installer.sh
bash installer.sh install cn.bing.com 54321 password
```

Running client with

```bash
anytls-client -l socks5://127.0.0.1:3080 -s 123.45.67.89:54321 -p password --sni cn.bing.com --root-cert /etc/anytls/ca.crt
```

Uninstall server with

```bash
bash installer.sh uninstall
```

### From Source

Ensure you have Rust installed (https://rustup.rs/), then:

```bash
git clone https://github.com/ssrlive/anytls-rs.git
cd anytls-rs
cargo build --release
```

The binaries will be in `target/release/`.

### Pre-built Binaries

Download from the [releases page](https://github.com/ssrlive/anytls-rs/releases).

## Usage

### Server

Start the AnyTLS server:

```bash
./anytls-server --password your_password
```

The server listens on `0.0.0.0:443` by default.

### Client

Start the AnyTLS client as a SOCKS5 proxy:

```bash
./anytls-client --password your_password --server 127.0.0.1:443
```

You can also use a single AnyTLS URI:

```bash
./anytls-client --url 'anytls://your_password@example.com?sni=example.com&insecure=1'
```

The client listens on `socks5://127.0.0.1:1080` by default, it's enable mixed SOCKS5 + HTTP CONNECT on the same port automatically.

## Options

### Server Options

- `-l, --listen <LISTEN>`: Server listen port [default: `0.0.0.0:443`]
- `-p, --password <PASSWORD>`: Password
- `    --padding-scheme <PADDING_SCHEME>`: Padding scheme file
- `    --sni <SNI>`: TLS server name indication (SNI)
- `    --cert <CERT>`: TLS certificate PEM file (optional)
- `    --key <KEY>`: TLS private key PEM file (optional)
- `    --log <LOG>`: Log level (off, error, warn, info, debug, trace) [default: info]
- `-h, --help`: Print help
- `-V, --version`: Print version

### Client Options

- `-l, --listen <LISTEN>`: URL of SOCKS5 and HTTP CONNECT listen parameters (default: `socks5://127.0.0.1:1080`)
- `--url <URL>`: AnyTLS URI in the form `anytls://[auth@]hostname[:port]/?[key=value]&[key=value]...` with `sni` and `insecure`
- `-s, --server <SERVER>`: Server address (default: `127.0.0.1:443`)
- `-p, --password <PASSWORD>`: Authentication password (required)
- `--sni <SNI>`: Server Name Indication for TLS
- `--padding-scheme <FILE>`: Padding scheme file
- `--log <LOG>`: Log level (off, error, warn, info, debug, trace)
- `--root-cert <FILE>`: Path to root CA certificate PEM file for server verification (optional)
- `--mitm <IP:PORT>`: Optional man in the middle (MITM) HTTP CONNECT proxy used for the client's outbound connection to the AnyTLS server
- `--max-streams-per-session <N>`: Maximum logical streams per AnyTLS session [default: `1`]; The `1` means multiplexing is disabled

## Examples

### Basic Setup

1. Start server:

   ```bash
   ./anytls-server -p mysecret
   ```

2. Start client:

   ```bash
   ./anytls-client -l socks5://127.0.0.1:1080 -p mysecret
   ```

3. Configure your browser or application to use SOCKS5 or HTTP CONNECT proxy at `127.0.0.1:1080`.

### With an Intermediate MITM Proxy

This setup lets you put a local HTTP CONNECT MITM proxy between the client and the real server.

First start the `trojan-killer` example and bind it to a local address:

```bash
cargo run --example trojan-killer -- 127.0.0.1:12345
```

Then point the client at that MITM proxy with `--mitm`:

```bash
./anytls-client -p mysecret -s 123.45.67.89:443 --mitm 127.0.0.1:12345
```

In this mode, the client first opens `CONNECT 123.45.67.89:443` to `127.0.0.1:12345`,
and `trojan-killer` forwards that connection to the real server while printing the traffic it sees.

### With Custom Certificates

1. Generate certificates (example using OpenSSL):

   ```bash
   # Generate CA
   openssl genrsa -out ca.key 2048
   openssl req -x509 -new -nodes -key ca.key -sha256 -days 365 -out ca.pem -subj "/CN=MyCA"

   # Generate server cert
   openssl genrsa -out server.key 2048
   openssl req -new -key server.key -out server.csr -subj "/CN=localhost"
   openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out server.pem -days 365 -sha256

   # Convert to PKCS#8
   openssl pkcs8 -topk8 -nocrypt -in server.key -out server.pk8
   ```

2. Start server with cert:

   ```bash
   ./anytls-server -p mysecret --cert server.pem --key server.pk8
   ```

3. Start client with root CA:
   ```bash
   ./anytls-client -p mysecret --root-cert ca.pem
   ```

### Generate Test Certificate (Python)

If you need a quick self-signed cert for local testing, use the included Python helper:

```bash
python scripts/gen_cert.py
# Produces scripts/selfsigned.crt and scripts/selfsigned.key (10 year validity)
```

`smoke_test.py` will also invoke this helper automatically if certificates are missing.

### Custom Ports

Server on port 443:

```bash
./anytls-server -l 0.0.0.0:443 -p mysecret
```

Client connecting to custom server:

```bash
./anytls-client -s example.com:443 -p mysecret
```

## Smoke / Integration Test (local)

Run the end-to-end smoke/integration test (builds binaries, starts a test server and client, then fetches a backend page through the SOCKS5 proxy):

```bash
# Requires Python 3 and curl (on Windows, run in PowerShell or Git Bash)
python scripts/smoke_test.py
```

## Building

```bash
cargo build --release
```

For development:

```bash
cargo build
cargo test
```

## Documentation

- [User FAQ](./docs/faq.md)
- [Protocol Documentation](./docs/protocol.md)
- [URI Format](./docs/uri_scheme.md)
- [Code Documentation](./docs/code.md)

## Compatibility Strategy

This project exposes two protocol version symbols used during session negotiation: `PROTOCOL_VERSION` (current implementation version)
and `MIN_PROTOCOL_VERSION` (minimum accepted version for compatibility). See [docs/protocol.md](./docs/protocol.md) for details. In short:

- Clients advertise `v=<n>` in `cmdSettings`; servers record and echo back a compatible version (>= `MIN_PROTOCOL_VERSION`).
- Feature gates (such as `cmdSYNACK` and heartbeats) are enabled only when the negotiated version supports them.
- Keep `MIN_PROTOCOL_VERSION` at a previous stable value when bumping `PROTOCOL_VERSION` to allow staged rollouts and interoperability.

## Contributing

Contributions are welcome! Please open issues and pull requests on GitHub.

## License

MIT License - see [LICENSE](LICENSE) file for details.
