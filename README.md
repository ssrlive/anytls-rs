# AnyTLS-RS

[![CI](https://github.com/ssrlive/anytls-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/ssrlive/anytls-rs/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/anytls.svg)](https://crates.io/crates/anytls)
[![docs.rs](https://img.shields.io/docsrs/anytls)](https://docs.rs/anytls)
![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)

A Rust implementation of the [AnyTLS](https://github.com/anytls/anytls-go) proxy protocol that attempts to mitigate the TLS in TLS fingerprinting problem.

AnyTLS-RS provides a proxy solution that disguises proxy traffic as regular TLS connections,
making it harder to detect and block.

## Features

- AnyTLS sessions run over TLS and authenticate with a password.
- The client multiplexes logical streams over reusable sessions and accepts SOCKS5 and HTTP CONNECT traffic on one local listener.
- UDP-over-TCP protocol v2 framing is supported.
- Configurable padding schemes control early TLS record payload sizes; the server can update the client's scheme.
- The server supports custom TLS certificates, SNI probe handling, and optional HTTP(S) forwarding for unauthenticated probes.
- Optional panel integration authorizes clients by UUID and reports traffic.

## Installation

### Linux Installer

The repository includes an interactive Debian/Ubuntu installer. It requires root and installs a system service; review the script before running it.

```bash
sudo ./scripts/anytls-install-2026.sh install
sudo ./scripts/anytls-install-2026.sh uninstall
```

Use `install --use-sspanel` to enable the installer's panel integration prompts.

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

Start the server (default listen address: `0.0.0.0:8443`):

```bash
./anytls-server --listen 0.0.0.0:8443 --password your_password
```

Without `--cert` and `--key`, the server generates a self-signed certificate.
Its default certificate name is `localhost`; provide `--sni` to choose another name.
For a deployed server, pass both a certificate and private key.

### Client

Start a local SOCKS5 + HTTP CONNECT proxy (default listen address: `127.0.0.1:1080`):

```bash
./anytls-client --server 127.0.0.1:8443 --password your_password --sni localhost --insecure
```

`--insecure` is appropriate only for the self-signed local example. For a trusted certificate,
configure the client's system trust or provide `--root-cert FILE` instead.

The server can also be configured with a single AnyTLS URI on the client:

```bash
./anytls-client --url 'anytls://your_password@example.com:443/?sni=example.com#home'
```

The URI accepts `sni`, `insecure=1|0`, and `client_id` query parameters. Its fragment is a display name.
Explicit client options take precedence over values from `--url`.
See [URI Format](./docs/uri_scheme.md) for escaping, IPv6, and more examples.

## Options

### Client Options

- `-u, --url URL`: AnyTLS URI; supplies the server and can carry password, SNI, TLS mode, client UUID, and display name.
- `-s, --server IP:PORT`: Server address. Required unless provided by `--url`; the URI's default port is `443`.
- `-p, --password PASSWORD`: Authentication password. It may instead be placed in the URI authority.
- `-l, --listen IP:PORT`: Local mixed SOCKS5/HTTP listener [default: `127.0.0.1:1080`].
- `    --sni DOMAIN`: TLS server name; defaults to the server host.
- `    --root-cert FILE`: Root certificate PEM file(s) used instead of the system root store to verify the server.
- `    --insecure [true|false]`: Skip normal server certificate verification. Prefer a trusted certificate or `--root-cert` when possible.
- `    --client-id UUID`: Client UUID for panel-managed access.
- `    --padding-scheme FILE`: Load a custom padding scheme.
- `-m, --max-streams-per-session N`: Maximum logical streams per session [default: `16`]. Set to `1` to disable multiplexing.
- `    --print-url`: Print the equivalent AnyTLS URI and exit.
- `    --log LEVEL`: Log level (`off`, `error`, `warn`, `info`, `debug`, `trace`) [default: `info`].

### Server Options

- `-l, --listen IP:PORT`: Listen address [default: `0.0.0.0:8443`].
- `-p, --password PASSWORD`: AnyTLS authentication password.
- `    --cert FILE` and `--key FILE`: TLS certificate and private key PEM files; provide both or neither.
- `    --sni NAME`: Certificate name when using the generated certificate and SNI probe fallback name.
- `    --forward URL`: Forward unauthenticated TLS probes to an `http://` or `https://` target.
  Without it, matching SNI probes may be relayed to the SNI host on port 443.
- `    --padding-scheme FILE`: Load the server's padding scheme.
- `-m, --max-streams-per-session N`: Maximum logical streams per authenticated session [default: `1024`].
- `    --panel-webapi-url URL`, `--panel-webapi-token TOKEN`, `--panel-node-id ID`: Enable panel synchronization; all three are required together.
- `    --panel-update-interval-secs SECS`: Panel update interval [default: `10`; minimum: `5`].
- `    --print-args`: Print server arguments with the detected public IP and exit.
- `    --print-url`: Print a client URI with the detected public IP and exit. This is unavailable when panel sync is enabled.
- `    --log LEVEL`: Log level (`off`, `error`, `warn`, `info`, `debug`, `trace`) [default: `info`].

The server's `--print-args` and `--print-url` options need outbound access to a public-IP service.

## Examples

### Basic Setup

1. Start server:

   ```bash
   ./anytls-server --listen 127.0.0.1:8443 -p mysecret --sni localhost
   ```

2. Start client:

   ```bash
   ./anytls-client -l 127.0.0.1:1080 -s 127.0.0.1:8443 -p mysecret --sni localhost --insecure
   ```

3. Configure your browser or application to use SOCKS5 or HTTP CONNECT at `127.0.0.1:1080`.

### With Custom Certificates

1. Generate certificates (example using OpenSSL):

   ```bash
   # Generate CA
   openssl genrsa -out ca.key 2048
   openssl req -x509 -new -nodes -key ca.key -sha256 -days 365 -out ca.pem -subj "/CN=MyCA" -addext "basicConstraints=critical,CA:TRUE"

   # Generate server cert
   openssl genrsa -out server.key 2048
   openssl req -new -key server.key -out server.csr -subj "/CN=example.com" -addext "subjectAltName=DNS:example.com"
   openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out server.pem -days 365 -sha256 -copy_extensions copy

   # Convert to PKCS#8
   openssl pkcs8 -topk8 -nocrypt -in server.key -out server.pk8
   ```

2. Start server with cert:

   ```bash
   ./anytls-server -l 0.0.0.0:8443 -p mysecret --sni example.com --cert server.pem --key server.pk8
   ```

3. Start client with root CA:
   ```bash
   ./anytls-client -s example.com:8443 -p mysecret --sni example.com --root-cert ca.pem
   ```

### Custom Ports

Server on a custom port:

```bash
./anytls-server -l 0.0.0.0:9443 -p mysecret
```

Client connecting to custom server:

```bash
./anytls-client -s example.com:9443 -p mysecret --insecure
```

## Smoke / Integration Test (local)

Run the end-to-end smoke test. It builds the binaries, starts a local server, client, and HTTP backend,
then fetches a page through the SOCKS5 proxy. Python 3 is required; `curl` is used for the fetch when available,
and OpenSSL is optional for an extra TLS check.

```bash
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

### Cargo Features

The default feature set builds both command-line applications: `client` and `server`.
Features can be disabled and selected explicitly for library consumers:

| Feature   | Enables                                                                    |
| --------- | -------------------------------------------------------------------------- |
| `core`    | Synchronous protocol primitives: frames, padding, hashes, and string maps  |
| `async`   | Async authentication and frame I/O (depends on `core` and Tokio `io-util`) |
| `runtime` | `async` plus session transport and asynchronous stream I/O                 |
| `uot`     | UDP-over-TCP protocol v2 helpers (depends on `async`)                      |
| `client`  | Client library and client CLI argument type                                |
| `server`  | Server CLI, panel synchronization, and `anytls-server` binary              |

For example, depend on the library without its default applications and select only the protocol core:

```toml
anytls = { version = "0.1", default-features = false, features = ["core"] }
```

Implementation modules are private; selected public types and functions are re-exported from the crate root.

## Documentation

- [User FAQ](./docs/faq.md)
- [Protocol Documentation](./docs/protocol.md)
- [URI Format](./docs/uri_scheme.md)
- [Client identification](./docs/client-name.md)
- [Go/Rust implementation notes](./docs/go-rust-improvements.md)

## Compatibility Strategy

The implementation exports `PROTOCOL_VERSION`, currently `2`. Clients advertise their version in session settings;
the server replies with its version, and stream handshake responses are used with version-2 peers.
See [Protocol Documentation](./docs/protocol.md) for the wire format and negotiation details.

## Contributing

Contributions are welcome! Please open issues and pull requests on GitHub.

## License

MIT License.
