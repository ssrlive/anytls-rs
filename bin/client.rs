use anytls::{
    BoxTransport, Client, ClientArgs, DEFAULT_SCHEME, Dialer, PaddingFactory, Stream, StreamIo, UotMode, UotRequest, uot_encode_packet,
    uot_get_packet_from_stream, uot_sentinel_destination, write_auth_with_client_id,
};
use clap::Parser;
use rustls::{
    ClientConfig,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use socks_hub_core::{BoxedStream, HttpConnector, UserKey, run_http_service};
use socks5_impl::{
    protocol::{Address, AsyncStreamOperation, Reply},
    server::{AssociatedUdpSocket, ClientConnection, IncomingConnection, UdpAssociate, auth::NoAuth, connection::associate},
};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpStream, UdpSocket};
use tokio_rustls::TlsConnector;
use uuid::Uuid;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    use std::io::{Error, ErrorKind::InvalidInput};
    let args = ClientArgs::parse().resolve()?;
    let default_log_filter = args.log.as_str().to_ascii_lowercase();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&default_log_filter)).init();
    if args.print_url {
        println!("{}", args.format_url()?);
        return Ok(());
    }
    let insecure = insecure_tls_enabled(args.root_cert.as_deref(), args.insecure.unwrap_or(false));
    let client_tls_config = tls_config(args.root_cert.as_deref(), insecure)?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    let server = args.server.clone().expect("server is validated by ClientArgs::resolve");
    log::info!("SOCKS5 + HTTP mixed listener started on {}; AnyTLS server {}", args.listen, server);
    let padding_factory = if let Some(path) = &args.padding_scheme {
        let content = tokio::fs::read(path).await?;
        let factory = PaddingFactory::new(&content)
            .ok_or_else(|| Error::new(InvalidInput, format!("Wrong format padding scheme file: {}", path.display())))?;
        log::info!("Loaded padding scheme file: {}", path.display());
        factory
    } else {
        PaddingFactory::new(DEFAULT_SCHEME).expect("default padding")
    };
    let padding = Arc::new(tokio::sync::RwLock::new(padding_factory));
    let password = Arc::new(args.password.clone().unwrap_or_default());
    let client_id = args.client_id;
    let sni = Arc::new(args.sni);
    let dialer_tls_config = Arc::clone(&client_tls_config);
    let dialer_padding = Arc::clone(&padding);
    let dialer: Dialer = Arc::new(move || {
        let padding = Arc::clone(&dialer_padding);
        let tls_config = Arc::clone(&dialer_tls_config);
        let password = password.clone();
        let sni = sni.clone();
        let server = server.clone();
        Box::pin(async move { dial(&server, sni.as_deref(), &password, padding, tls_config, client_id).await })
    });
    let client = Client::new(
        dialer,
        padding,
        std::time::Duration::from_secs(30),
        args.max_streams_per_session,
        std::time::Duration::from_secs(3600),
    );
    let connector_client = Arc::clone(&client);
    let connector: HttpConnector = Arc::new(move |destination: Address| {
        let client = Arc::clone(&connector_client);
        Box::pin(async move {
            let stream = client.create_stream().await?;
            let mut adapter = HttpStreamIo::new(StreamIo::new(stream));
            destination.write_to_async_stream(&mut adapter).await?;
            Ok(Box::new(adapter) as BoxedStream)
        })
    });

    loop {
        let (stream, _) = listener.accept().await?;
        let client = client.clone();
        let connector = connector.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_listener_stream(stream, client, connector).await {
                log::warn!("Proxy connection failed: {error}");
            }
        });
    }
}

async fn handle_listener_stream(mut stream: TcpStream, client: Arc<Client>, connector: HttpConnector) -> std::io::Result<()> {
    let peer_addr = stream.peer_addr().ok();
    let protocol = match tokio::time::timeout(std::time::Duration::from_secs(5), detect_listener_protocol(&stream)).await {
        Ok(protocol) => protocol?,
        Err(_) => {
            log::debug!("Timed out detecting proxy protocol from {peer_addr:?}");
            stream.shutdown().await?;
            return Ok(());
        }
    };
    match protocol {
        Some(ListenerProtocol::Socks5) => {
            log::trace!("SOCKS5 client detected from {peer_addr:?}");
            let incoming = IncomingConnection::new(stream, Arc::new(NoAuth));
            handle_socks5(incoming, client).await
        }
        Some(ListenerProtocol::Socks4) => {
            log::warn!("SOCKS4 is unsupported on mixed SOCKS5/HTTP listener from {peer_addr:?}");
            stream.shutdown().await
        }
        Some(ListenerProtocol::Http) => {
            log::trace!("HTTP proxy client detected from {peer_addr:?}");
            run_http_service(stream, connector, UserKey::default()).await
        }
        None => {
            let mut first_byte = [0u8; 1];
            let _ = stream.peek(&mut first_byte).await?;
            log::warn!("Unknown proxy protocol from {peer_addr:?}, first byte: 0x{:02x}", first_byte[0]);
            stream.shutdown().await
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ListenerProtocol {
    Socks5,
    Socks4,
    Http,
}

async fn detect_listener_protocol(stream: &TcpStream) -> std::io::Result<Option<ListenerProtocol>> {
    let mut peek_buf = [0u8; 16];
    loop {
        let size = stream.peek(&mut peek_buf).await?;
        if size == 0 {
            return Ok(None);
        }
        let bytes = &peek_buf[..size];
        match bytes[0] {
            0x05 => return Ok(Some(ListenerProtocol::Socks5)),
            0x04 => return Ok(Some(ListenerProtocol::Socks4)),
            _ if is_http_request(bytes) => return Ok(Some(ListenerProtocol::Http)),
            _ if could_be_http_request_prefix(bytes) => {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            _ => return Ok(None),
        }
    }
}

fn is_http_request(bytes: &[u8]) -> bool {
    http_methods().iter().any(|method| {
        bytes.get(..method.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(method)) && bytes.get(method.len()) == Some(&b' ')
    })
}

fn could_be_http_request_prefix(bytes: &[u8]) -> bool {
    http_methods().iter().any(|method| {
        let compared_len = bytes.len().min(method.len());
        bytes
            .get(..compared_len)
            .zip(method.get(..compared_len))
            .is_some_and(|(prefix, expected)| prefix.eq_ignore_ascii_case(expected))
            && (bytes.len() <= method.len() || bytes.get(method.len()) == Some(&b' '))
    })
}

fn http_methods() -> &'static [&'static [u8]] {
    const METHODS: &[&[u8]] = &[
        b"CONNECT",
        b"GET",
        b"POST",
        b"HEAD",
        b"PUT",
        b"OPTIONS",
        b"DELETE",
        b"TRACE",
        b"PATCH",
        b"LOCK",
        b"UNLOCK",
        b"PROPFIND",
        b"MKCOL",
        b"COPY",
        b"MOVE",
    ];
    METHODS
}

#[cfg(test)]
mod listener_tests {
    use super::{could_be_http_request_prefix, insecure_tls_enabled, is_http_request};
    use std::path::Path;

    #[test]
    fn root_certificate_forces_secure_tls_even_when_insecure_is_true() {
        assert!(!insecure_tls_enabled(None, false));
        assert!(insecure_tls_enabled(None, true));
        assert!(!insecure_tls_enabled(Some(Path::new("root.pem")), true));
        assert!(!insecure_tls_enabled(Some(Path::new("root.pem")), false));
    }

    #[test]
    fn recognizes_complete_http_methods_case_insensitively() {
        assert!(is_http_request(b"CONNECT example.com:443 HTTP/1.1\r\n"));
        assert!(is_http_request(b"get http://example.com/ HTTP/1.1\r\n"));
        assert!(!is_http_request(b"GARBAGE"));
    }

    #[test]
    fn retains_partial_http_method_prefixes_for_more_peeking() {
        assert!(could_be_http_request_prefix(b"CON"));
        assert!(could_be_http_request_prefix(b"GET"));
        assert!(!could_be_http_request_prefix(b"GARBAGE"));
    }
}

struct HttpStreamIo {
    inner: Mutex<StreamIo>,
}

impl HttpStreamIo {
    fn new(inner: StreamIo) -> Self {
        Self { inner: Mutex::new(inner) }
    }
}

impl AsyncRead for HttpStreamIo {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return Poll::Ready(Err(std::io::Error::other("HTTP stream lock poisoned"))),
        };
        Pin::new(&mut *inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for HttpStreamIo {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return Poll::Ready(Err(std::io::Error::other("HTTP stream lock poisoned"))),
        };
        Pin::new(&mut *inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return Poll::Ready(Err(std::io::Error::other("HTTP stream lock poisoned"))),
        };
        Pin::new(&mut *inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return Poll::Ready(Err(std::io::Error::other("HTTP stream lock poisoned"))),
        };
        Pin::new(&mut *inner).poll_shutdown(cx)
    }
}

async fn dial(
    server: &Address,
    sni: Option<&str>,
    password: &str,
    padding: Arc<tokio::sync::RwLock<PaddingFactory>>,
    tls_config: Arc<ClientConfig>,
    client_id: Option<Uuid>,
) -> std::io::Result<BoxTransport> {
    let addr = server
        .to_socket_addrs()?
        .next()
        .ok_or(std::io::Error::other("No socket addresses found"))?;
    let tcp = TcpStream::connect(addr).await?;
    log::info!("connecting to AnyTLS server {server}");
    let name = match sni {
        Some(sni) => ServerName::try_from(sni.to_owned()).map_err(std::io::Error::other)?,
        None => ServerName::try_from(server.host()).map_err(std::io::Error::other)?,
    };
    let connector = TlsConnector::from(tls_config);
    let mut tls = connector.connect(name, tcp).await?;
    log::info!("TLS connection to AnyTLS server {server} established");
    let padding = padding.read().await;
    write_auth_with_client_id(&mut tls, password, &padding, client_id).await?;
    log::info!("AnyTLS authentication to {server} completed");
    Ok(Box::new(tls))
}

async fn handle_socks5(incoming: socks5_impl::server::IncomingConnection, client: Arc<Client>) -> std::io::Result<()> {
    let authenticated = incoming.authenticate().await.map_err(std::io::Error::other)?;
    let request = authenticated.wait_request().await.map_err(std::io::Error::other)?;
    let (connect, target) = match request {
        ClientConnection::Connect(connect, target) => (connect, target),
        ClientConnection::Bind(mut bind, target) => {
            let _ = bind.shutdown().await;
            return Err(std::io::Error::other(format!("SOCKS5 BIND is unsupported: {target}")));
        }
        ClientConnection::UdpAssociate(associate, _) => {
            return handle_udp_associate(associate, client).await;
        }
    };
    let started = std::time::Instant::now();
    log::info!("opening SOCKS5 CONNECT to {target}");
    let stream = client.create_stream().await?;
    let mut remote = StreamIo::new(stream);
    target.write_to_async_stream(&mut remote).await?;
    let mut ready = connect.reply(Reply::Succeeded, Address::unspecified()).await?;
    let (client_to_proxy, proxy_to_client) = tokio::io::copy_bidirectional(&mut ready, &mut remote).await?;
    ready.shutdown().await?;
    remote.shutdown().await?;
    log::info!(
        "SOCKS5 relay to {target} closed: client_to_proxy={client_to_proxy} bytes, proxy_to_client={proxy_to_client} bytes, elapsed={:?}",
        started.elapsed()
    );
    Ok(())
}

const MAX_UDP_RELAY_PACKET_SIZE: usize = 65_535;

async fn handle_udp_associate(associate: UdpAssociate<associate::NeedReply>, client: Arc<Client>) -> std::io::Result<()> {
    let (tcp_local_addr, udp_listener, listen_addr) = match async {
        let tcp_local_addr = associate.local_addr()?;
        let udp_listener = UdpSocket::bind(SocketAddr::from((tcp_local_addr.ip(), 0))).await?;
        let listen_addr = udp_listener.local_addr()?;
        Ok::<_, std::io::Error>((tcp_local_addr, udp_listener, listen_addr))
    }
    .await
    {
        Ok(result) => result,
        Err(error) => {
            let mut reply = associate.reply(Reply::GeneralFailure, Address::unspecified()).await?;
            reply.shutdown().await?;
            return Err(error);
        }
    };
    let mut proxy_reader = match client.create_stream().await {
        Ok(stream) => StreamIo::new(stream),
        Err(error) => {
            let mut reply = associate.reply(Reply::GeneralFailure, Address::unspecified()).await?;
            reply.shutdown().await?;
            return Err(error);
        }
    };
    let proxy_stream = proxy_reader.stream();

    let outer_address: Vec<u8> = uot_sentinel_destination().into();
    let request: Vec<u8> = UotRequest::new(UotMode::Datagram, Address::unspecified()).into();
    if let Err(error) = write_stream_all(&proxy_stream, &outer_address).await {
        let _ = proxy_stream.shutdown_write().await;
        let mut reply = associate.reply(Reply::GeneralFailure, Address::unspecified()).await?;
        reply.shutdown().await?;
        return Err(error);
    }
    if let Err(error) = write_stream_all(&proxy_stream, &request).await {
        let _ = proxy_stream.shutdown_write().await;
        let mut reply = associate.reply(Reply::GeneralFailure, Address::unspecified()).await?;
        reply.shutdown().await?;
        return Err(error);
    }

    let advertised_addr = SocketAddr::new(tcp_local_addr.ip(), listen_addr.port());
    let mut control = associate.reply(Reply::Succeeded, Address::from(advertised_addr)).await?;
    let listen_udp = Arc::new(AssociatedUdpSocket::from((udp_listener, MAX_UDP_RELAY_PACKET_SIZE)));
    let zero_ip = match listen_addr {
        SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    };
    let incoming_addr = Arc::new(tokio::sync::Mutex::new(SocketAddr::from((zero_ip, 0))));
    let (packet_sender, mut packet_receiver) = tokio::sync::mpsc::channel(32);
    let reader_task = tokio::spawn(async move {
        loop {
            let packet = uot_get_packet_from_stream(UotMode::Datagram, &mut proxy_reader).await;
            let failed = packet.is_err();
            if packet_sender.send(packet).await.is_err() || failed {
                break;
            }
        }
    });

    let result: std::io::Result<()> = async {
        loop {
            tokio::select! {
            packet = listen_udp.recv_from() => {
                let (payload, fragment, destination, source) = packet?;
                if fragment != 0 {
                    break Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "UDP fragmentation is not supported"));
                }
                *incoming_addr.lock().await = source;
                let frame = uot_encode_packet(UotMode::Datagram, Some(&destination), &payload)?;
                write_stream_all(&proxy_stream, &frame).await?;
            }
            packet = packet_receiver.recv() => {
                let packet = packet.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "UOT reader stopped"))??;
                let (source, payload) = packet;
                let incoming = *incoming_addr.lock().await;
                if incoming.port() == 0 {
                    continue;
                }
                let source = source.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "UOT response has no source address"))?;
                listen_udp.send_to(&payload, 0, source, incoming).await?;
            }
            closed = control.wait_until_closed() => {
                closed?;
                break Ok(());
            }
            }
        }
    }
    .await;

    reader_task.abort();
    let _ = proxy_stream.shutdown_write().await;
    let _ = control.shutdown().await;
    result
}

async fn write_stream_all(stream: &Stream, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let written = stream.write(bytes).await?;
        if written == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "failed to write UOT stream"));
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

fn tls_config(root_cert: Option<&Path>, insecure: bool) -> std::io::Result<Arc<ClientConfig>> {
    if insecure {
        let mut config = ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        config.dangerous().set_certificate_verifier(Arc::new(AnyCertificate));
        return Ok(Arc::new(config));
    }

    let mut root_store = rustls::RootCertStore::empty();
    if let Some(path) = root_cert {
        let file = std::fs::File::open(path)?;
        let mut reader = std::io::BufReader::new(file);
        for cert in rustls_pemfile::certs(&mut reader) {
            root_store.add(cert?).map_err(std::io::Error::other)?;
        }
    } else {
        let cert_result = rustls_native_certs::load_native_certs();
        if !cert_result.errors.is_empty() {
            log::warn!("Failed to load some native certificates: {:?}", cert_result.errors);
        }
        for cert in cert_result.certs {
            root_store.add(cert).map_err(std::io::Error::other)?;
        }
    }

    if root_store.roots.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "No root certificates available for TLS verification",
        ));
    }

    let config = ClientConfig::builder().with_root_certificates(root_store).with_no_client_auth();
    Ok(Arc::new(config))
}

fn insecure_tls_enabled(root_cert: Option<&Path>, insecure: bool) -> bool {
    root_cert.is_none() && insecure
}

#[derive(Debug)]
struct AnyCertificate;

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
        ]
    }
}
