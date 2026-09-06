# Protocol Documentation

> This document describes the current behavior of the [Go implementation](https://github.com/anytls/anytls-go).

## Client

### Authentication

This protocol is based on the TLS protocol. After the TLS handshake is completed, the client immediately sends an authentication request:

| sha256(password) | padding0 length   | padding0        |
| ---------------- | ----------------- | --------------- |
| 32 Bytes         | Big-Endian uint16 | Variable length |

After successful authentication, the server enters the session loop. After authentication failure, the server closes the connection (or falls back to HTTP service).

### Session

After authentication is completed, the client & server start a session layer event loop on top of the TLS protocol. The session layer frame format is as follows:

| command | streamId          | data length       | data            |
| ------- | ----------------- | ----------------- | --------------- |
| uint8   | Big-Endian uint32 | Big-Endian uint16 | Variable length |

**The client must immediately send `cmdSettings` when starting a new session.**

#### command

```
// Since version 1

cmdWaste               = 0 // Paddings
cmdSYN                 = 1 // stream open
cmdPSH                 = 2 // data push
cmdFIN                 = 3 // stream close, a.k.a EOF mark
cmdSettings            = 4 // Settings（client sends to server）
cmdAlert               = 5 // Alert（server sends to client）
cmdUpdatePaddingScheme = 6 // update padding scheme（server sends to client）

// Since version 2

cmdSYNACK         = 7  // Server reports to the client that the stream has been opened
cmdHeartRequest   = 8  // Keep alive command
cmdHeartResponse  = 9  // Keep alive command
cmdServerSettings = 10 // Settings (Server send to client)
```

For different types of commands, unless mentioned below, this type of command should not and cannot carry data.

#### cmdWaste

After receiving cmdWaste, either party should read its data completely and silently discard it.

#### cmdHeartRequest

After receiving cmdHeartRequest, either party should send cmdHeartResponse to the other party.

#### cmdSYN

The client notifies the server to open a new Stream. The client should generate a monotonically increasing streamId within the Session for each Stream.

#### cmdSYNACK

If the client reports version `v` >= 2, after the server receives cmdSYN, it should send a cmdSYNACK packet with the corresponding streamId after the proxy outbound connection TCP handshake is completed.

If your server software architecture does not support reporting outbound connection status, you can also directly send cmdSYNACK after receiving cmdSYN.

If cmdSYNACK does not carry data, it means the proxy stream handshake was successful. If it carries data, the data represents error information. After the client receives the error information, it must close the corresponding stream.

#### cmdPSH

The data of this command carries the transmission data of the Stream.

#### cmdFIN

Notifies the other party to close the Stream corresponding to the streamId.

#### cmdSettings

Its data is currently:

```
v=2
client=anytls-rs/0.0.8
padding-md5=(md5)
```

> Uses UTF-8 encoding, key and value are connected with `=`, both are string types. Different items are separated by `\n`.

- `v` is the protocol version number implemented by the client (currently `2`)
- `client` is the client software name and version number (third-party implementations should fill in the real software name and version number, there is no point in disguising)
- `padding-md5` is the md5 of the client's current `paddingScheme` (lowercase hex encoding)

#### cmdServerSettings

Its data is currently:

```
v=2
```

- `v` is the protocol version number implemented by the server (currently `2`)

#### cmdAlert

The data is the warning text information sent by the server. The client needs to read it and print it to the log, then both parties close the session.

#### cmdUpdatePaddingScheme

When the server receives a `padding-md5` from the client that is different from the server, it will send `cmdUpdatePaddingScheme` to request the client to update. The data format is currently as follows:

> Default Padding Scheme

```
stop=8
0=30-30
1=100-400
2=400-500,c,500-1000,c,500-1000,c,500-1000,c,500-1000
3=9-9,500-1000
4=500-1000
5=500-1000
6=500-1000
7=500-1000
```

- The client should store `paddingScheme` in the Client object, that is, the `paddingScheme` issued by the server only acts on the Client connected to the server
- The client uses the default `paddingScheme` for the first session connection. If `cmdUpdatePaddingScheme` is received, subsequent new sessions must use the `paddingScheme` issued by the server

Current Go implementation note:
The Go implementation currently updates a process-wide default `paddingScheme` after receiving `cmdUpdatePaddingScheme`,
and subsequent new sessions use that updated global default.

> With this design, when the traffic characteristics generated by the default paddingScheme are blacklisted by GFW, theoretically each client only needs to send a small amount of data when starting (ideally only the first connected pkt 0~2), and can update to the characteristics specified by the server after receiving the first `cmdUpdatePaddingScheme` from the server. Therefore, theoretically the proportion of connections with known characteristics that can be captured by GFW will be very low.

#### paddingScheme specific meaning and implementation

> stop

`stop` indicates which packet to stop processing padding, for example: `stop=8` means only processing packets `0~7`.

> padding0

`padding0` is the `0`th packet, which is in the authentication part and does not support packet splitting. The client should send the padding of this length together with sha256(password).

Note: The overhead of the authentication part is 34 bytes.

> padding1 starts

- Starting from padding1, it is in the session part, using strategy packet splitting and/or padding: if there is still remaining user data after packet splitting is completed, the remaining data is sent directly. If the user data is exhausted before packet splitting is completed, send `cmdWaste` with data (suggested to use 0) for padding.
- Strategy example: The above paddingScheme will split packet `2` into 5 packets with sizes between 400-500 / 500-1000 (the size here refers to the size of TLS PlainText, not including TLS encryption and other overhead).
- `c` in the strategy is a check symbol, meaning: if the user data has no remaining after the previous packet splitting is completed, directly return to this Write TLS, and no longer send subsequent padding packets.
- The packet counter is based on the number of Write TLS times. Packet `1` should include: `cmdSettings` and the first Stream's `cmdSYN + cmdPSH(proxy target address)`
- Packet `2` should be the first data packet proxied from the user, such as TLS ClientHello.
- If the sending strategy of a certain packet before stop is not defined by PaddingScheme, send the packet directly.

Reference processing logic in `func (s *Session) writeConn()`

### Reuse

**The client must implement session layer reuse functionality.** The overall architecture is:

> TCP Proxy -> Stream -> Session -> TLS -> TCP

Specific reuse logic:

Before creating a new session layer, you must check if there is an "idle" session. If there is, take the session with the largest `Seq`, and open a Stream on this Session to carry the user proxy request.

If there is no idle session, create a new session. The sequence number `Seq` of the Session should be monotonically increasing within a Client.

When the Stream is closed after the proxy relay is completed, if the event loop of the corresponding Session does not encounter an error, the Session is put into the "idle session pool", and the idle start time of the Session is set to now.

Regularly (such as 30s) check the session pool, close and delete sessions that have been idle for more than a certain time (such as 60s).

> The above reuse strategy is highly summarized: prioritize reusing the latest session, prioritize cleaning up the oldest session.

### Proxy

For TCP, after each Stream is opened, the client sends the target address of the proxy request in [SocksAddr](https://tools.ietf.org/html/rfc1928#section-5) format to the server, and then starts bidirectional proxy relay.

For UDP, sing-box's [udp-over-tcp 2](https://sing-box.sagernet.org/configuration/shared/udp-over-tcp/#protocol-version-2) protocol is now used, which is equivalent to proxying the TCP request `sp.v2.udp-over-tcp.arpa`.

## Server

### Authentication

The server runs based on TLS Server. For each Accpted TLS Connection, the authentication method is:

Read the first data packet, verify the authentication request (including completely reading padding0). If it matches, start the session loop. If it doesn't match, close the connection directly or "[fallback](https://trojan-gfw.github.io/trojan/protocol.html#:~:text=Anti%2Ddetection-,Active%2DDetection,-All%20connection%20without)" to any "legal" L7 application.

### Session

The session layer format and commands are the same as the client.

For a new Session, if the server receives `cmdSYN` before receiving the client's `cmdSettings`, it must reject this session.

The server has the right to reject client connections that do not correctly implement this protocol (including but not limited to `cmdUpdatePaddingScheme` and connection reuse) and have outdated versions (with known issues).

When the server rejects such clients, it must send `cmdAlert` to explain the reason, then close the Session.

When the client reports version `v` >= 2, the server should immediately send cmdServerSettings after receiving cmdSettings.

### Proxy

After the proxy relay is completed, the server closes the Stream but does not close the Session.

The server can regularly clean up Sessions that have been idle for a long time.

For requests with target address `sp.v2.udp-over-tcp.arpa`, the sing-box udp-over-tcp protocol should be used for processing.

## Protocol Parameters

The anytls protocol parameters do not include TLS parameters. TLS parameters should be specified in another configuration section.

### Client

- `password` Required, string type, password for protocol authentication.
- `idleSessionCheckInterval` Optional, time.Duration type, interval for checking idle sessions.
- `idleSessionTimeout` Optional, time.Duration type, in the check, close sessions that have been idle for longer than this duration.
- `minIdleSession` Optional, int type, in the check, keep at least the first n idle sessions from being closed, that is, reserve a certain number of "prepared sessions" for subsequent proxies.

### Server

- `paddingScheme` Optional, string type, padding scheme.

## Update Records

### Protocol Version 2

> `anytls-rs` v0.0.8+

> `sing-anytls` v0.0.7+ ( `sing-box` 1.12.0-alpha.21+ )

> `mihomo` Prerelease-Alpha 2025.3.27+

This protocol update is mainly to deal with the problem of tunnel connection stuck, and implement better timeout handling.

The following features should only be enabled when both your server and client support version 2. Otherwise, both ends will run according to version 1.

- Can use cmdSYNACK to report server outbound connection status, and detect and recover stuck tunnel connections
- `cmdHeartRequest` and `cmdHeartResponse` are used for carrier keep-alive. This implementation sends periodic requests,
  expects a response within the heartbeat timeout, and terminates the carrier when the peer stops responding.
- Server can send negotiation information to client (cmdServerSettings)

Version negotiation principle:

- v2 server + v1 client: Since the client sends version 1, the server directly disables version 2 features.
- v1 server + v2 client: Since the client sends version 2, the server doesn't recognize it and won't send cmdServerSettings to the client. The client doesn't receive the version prompt from cmdServerSettings, defaults to version 1, and doesn't enable version 2 features.

#### cmdSYNACK

When the tunnel connection is unexpectedly disconnected and the client does not receive RST, the behavior of protocol version 1 may cause very long timeouts in extreme cases (depending on system settings).

Since version 2 clients can expect a reply from the server when opening a stream, if no reply is received for a long time, it means there may be a network problem, and the client can close the stuck connection in advance.

Current Go implementation note:
The Go implementation starts this timeout only when `peerVersion >= 2` and the opened `streamId >= 2`, and the timeout is currently fixed at 3 seconds.
