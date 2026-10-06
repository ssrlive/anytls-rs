use crate::{
    BoxTransport, Client, ClientArgs, DEFAULT_SCHEME, Dialer, PaddingFactory, Stream, StreamIo, UotMode, UotRequest, relay, traffic_status,
    uot_encode_packet, uot_get_packet_from_stream, uot_sentinel_destination, write_auth_with_client_id,
};
use clap::Parser;
use method_name::method_name_unstable;
use rustls::{
    ClientConfig,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use socks_hub_core::{BoxedStream, HttpConnector, UserKey, run_http_service};
use socks5_impl::{
    protocol::{Address, AsyncStreamOperation, ProxyType, Reply},
    server::{AssociatedUdpSocket, ClientConnection, IncomingConnection, UdpAssociate, auth::NoAuth, connection::associate},
};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpStream, UdpSocket};
use tokio_rustls::TlsConnector;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub async fn run_client(cancel_token: CancellationToken) -> std::io::Result<()> {
    let args = ClientArgs::parse().resolve()?;
    run_client_with_args(cancel_token, args, None).await
}

pub(crate) async fn run_client_with_args(
    cancel_token: CancellationToken,
    args: ClientArgs,
    on_listening: Option<Box<dyn FnOnce(SocketAddr)>>,
) -> std::io::Result<()> {
    let mn = method_name_unstable!();
    use std::io::{Error, ErrorKind::InvalidInput};
    let default_log_filter = args.log.as_str().to_ascii_lowercase();
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&default_log_filter)).try_init();
    if args.print_url {
        println!("{}", args.format_url()?);
        return Ok(());
    }
    let insecure = insecure_tls_enabled(args.root_cert.as_deref(), args.insecure.unwrap_or(false));
    let client_tls_config = tls_config(args.root_cert.as_deref(), insecure)?;
    let listen_addr = args
        .listen
        .addr
        .as_ref()
        .ok_or_else(|| Error::new(InvalidInput, "Local proxy listener address is required"))?
        .to_string();
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    let listen_addr = listener.local_addr()?;
    if let Some(callback) = on_listening {
        callback(listen_addr);
    }
    let proxy_type = args.listen.proxy_type;
    let server = args.server.clone().expect("server is validated by ClientArgs::resolve");
    log::info!("{mn} -- {proxy_type} proxy listener started on {listen_addr}; AnyTLS server {server}");
    let padding_factory = if let Some(path) = &args.padding_scheme {
        let content = tokio::fs::read(path).await?;
        let factory = PaddingFactory::new(&content)
            .ok_or_else(|| Error::new(InvalidInput, format!("Wrong format padding scheme file: {}", path.display())))?;
        log::info!("{mn} -- Loaded padding scheme file: {}", path.display());
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

    let mut connection_tasks = tokio::task::JoinSet::new();
    let accept_result = loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break Ok(()),
            result = listener.accept() => {
                let (stream, _) = match result {
                    Ok(connection) => connection,
                    Err(error) => break Err(error),
                };
                let client = client.clone();
                let connector = connector.clone();
                let context = Arc::new(ProxyConnectionContext::new(stream.peer_addr().ok()));
                connection_tasks.spawn(async move {
                    if let Err(error) = handle_listener_stream(stream, client, connector, proxy_type, Arc::clone(&context)).await {
                        log::warn!("{} -- Proxy connection failed: {}: {error}", method_name_unstable!(), context.label());
                    }
                });
            }
            Some(result) = connection_tasks.join_next(), if !connection_tasks.is_empty() => {
                if let Err(error) = result {
                    log::warn!("{} -- Proxy connection task failed: {error}", method_name_unstable!());
                }
            }
        }
    };

    let cleanup_result = async {
        let shutdown_result = client.shutdown().await;
        let drain = async {
            while let Some(result) = connection_tasks.join_next().await {
                if let Err(error) = result {
                    let mn = method_name_unstable!();
                    log::warn!("{mn} -- Proxy connection task failed during shutdown: {error}");
                }
            }
        };
        if tokio::time::timeout(std::time::Duration::from_secs(5), drain).await.is_err() {
            connection_tasks.abort_all();
            while connection_tasks.join_next().await.is_some() {}
        }
        shutdown_result
    }
    .await;

    accept_result?;
    cleanup_result
}

struct ProxyConnectionContext {
    peer_addr: Option<SocketAddr>,
    protocol: Mutex<ProxyType>,
    target: Mutex<Option<String>>,
}

impl ProxyConnectionContext {
    fn new(peer_addr: Option<SocketAddr>) -> Self {
        Self {
            peer_addr,
            protocol: Mutex::new(ProxyType::None),
            target: Mutex::new(None),
        }
    }

    fn set_protocol(&self, protocol: ProxyType) {
        *self.protocol.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = protocol;
    }

    fn set_target(&self, target: String) {
        *self.target.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(target);
    }

    fn label(&self) -> String {
        let protocol = *self.protocol.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let target = self.target.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let target = target.as_deref().unwrap_or("<unknown>");
        format!("peer={:?} protocol={protocol} target={target}", self.peer_addr)
    }
}

async fn handle_listener_stream(
    mut stream: TcpStream,
    client: Arc<Client>,
    connector: HttpConnector,
    proxy_type: ProxyType,
    context: Arc<ProxyConnectionContext>,
) -> std::io::Result<()> {
    let mn = method_name_unstable!();
    let c_l = context.label();
    let peer_addr = context.peer_addr;
    let protocol = match tokio::time::timeout(std::time::Duration::from_secs(5), detect_listener_protocol(&stream)).await {
        Ok(Ok(protocol)) => protocol,
        Ok(Err(error)) if relay::is_peer_disconnect(&error) => {
            log::debug!("{mn} -- Proxy peer disconnected during protocol detection: {c_l}: {error}");
            return Ok(());
        }
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            log::debug!("{mn} -- Timed out detecting proxy protocol from {peer_addr:?}");
            stream.shutdown().await?;
            return Ok(());
        }
    };
    if let Some(protocol) = protocol {
        context.set_protocol(match protocol {
            ListenerProtocol::Socks5 => ProxyType::Socks5,
            ListenerProtocol::Socks4 => ProxyType::Socks4,
            ListenerProtocol::Http => ProxyType::Http,
        });
        if !listener_supports_protocol(&proxy_type, protocol) {
            log::warn!("{mn} -- {protocol:?} is not enabled on the {proxy_type} proxy listener: {c_l}");
            return stream.shutdown().await;
        }
    }

    match protocol {
        Some(ListenerProtocol::Socks5) => {
            log::trace!("{mn} -- SOCKS5 client detected from {peer_addr:?}");
            let incoming = IncomingConnection::new(stream, Arc::new(NoAuth));
            handle_socks5(incoming, client, context).await
        }
        Some(ListenerProtocol::Socks4) => {
            log::warn!("{mn} -- SOCKS4 is unsupported on the mixed SOCKS5/HTTP listener: {c_l}");
            stream.shutdown().await
        }
        Some(ListenerProtocol::Http) => {
            log::trace!("{mn} -- HTTP proxy client detected from {peer_addr:?}");
            let request_context = Arc::clone(&context);
            let contextual_connector: HttpConnector = Arc::new(move |destination: Address| {
                request_context.set_target(destination.to_string());
                connector(destination)
            });
            run_http_service(stream, contextual_connector, UserKey::default()).await
        }
        None => {
            context.set_protocol(ProxyType::None);
            let mut first_byte = [0u8; 1];
            match stream.peek(&mut first_byte).await {
                Ok(0) => {
                    log::debug!("{mn} -- Proxy peer closed before sending a request: {c_l}");
                    return Ok(());
                }
                Ok(_) => log::warn!("{mn} -- Unknown proxy protocol: {c_l}, first byte: 0x{:02x}", first_byte[0]),
                Err(error) if relay::is_peer_disconnect(&error) => {
                    log::debug!("{mn} -- Proxy peer disconnected before sending a request: {c_l}: {error}");
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
            stream.shutdown().await
        }
    }
}

fn listener_supports_protocol(proxy_type: &ProxyType, protocol: ListenerProtocol) -> bool {
    matches!(
        (proxy_type, protocol),
        (ProxyType::Socks5, ListenerProtocol::Socks5)
            | (ProxyType::Http, ListenerProtocol::Http)
            | (ProxyType::Mixed, ListenerProtocol::Socks5 | ListenerProtocol::Http)
    )
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
    use super::{
        ListenerProtocol, could_be_http_request_prefix, insecure_tls_enabled, is_http_request, listener_supports_protocol,
        negotiate_socks5_request,
    };
    use socks5_impl::protocol::ProxyType;
    use socks5_impl::server::IncomingConnection;
    use std::{path::Path, sync::Arc, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

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

    #[test]
    fn listener_scheme_limits_accepted_protocols() {
        assert!(listener_supports_protocol(&ProxyType::Socks5, ListenerProtocol::Socks5));
        assert!(!listener_supports_protocol(&ProxyType::Socks5, ListenerProtocol::Http));
        assert!(listener_supports_protocol(&ProxyType::Http, ListenerProtocol::Http));
        assert!(!listener_supports_protocol(&ProxyType::Http, ListenerProtocol::Socks5));
        assert!(listener_supports_protocol(&ProxyType::Mixed, ListenerProtocol::Socks5));
        assert!(listener_supports_protocol(&ProxyType::Mixed, ListenerProtocol::Http));
        assert!(!listener_supports_protocol(&ProxyType::Mixed, ListenerProtocol::Socks4));
    }

    #[tokio::test]
    async fn stalled_socks5_request_times_out_and_closes_the_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let incoming = IncomingConnection::new(server, Arc::new(super::NoAuth));
        let negotiation = tokio::spawn(negotiate_socks5_request(incoming, Duration::from_millis(10)));

        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut response = [0u8; 2];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(response, [0x05, 0x00]);

        let error = negotiation.await.unwrap().err().unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);

        let mut byte = [0u8; 1];
        assert_eq!(client.read(&mut byte).await.unwrap(), 0);
    }
}

const SOCKS_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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
        let filled_before = buf.filled().len();
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return Poll::Ready(Err(std::io::Error::other("HTTP stream lock poisoned"))),
        };
        let result = Pin::new(&mut *inner).poll_read(cx, buf);
        drop(inner);
        if matches!(result, Poll::Ready(Ok(()))) {
            traffic_status::record(0, buf.filled().len() - filled_before);
        }
        result
    }
}

async fn negotiate_socks5_request(incoming: IncomingConnection, timeout: std::time::Duration) -> std::io::Result<ClientConnection> {
    tokio::time::timeout(timeout, async {
        let authenticated = incoming.authenticate().await.map_err(std::io::Error::other)?;
        authenticated.wait_request().await.map_err(std::io::Error::other)
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "SOCKS5 handshake timed out"))?
}

impl AsyncWrite for HttpStreamIo {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return Poll::Ready(Err(std::io::Error::other("HTTP stream lock poisoned"))),
        };
        let result = Pin::new(&mut *inner).poll_write(cx, buf);
        drop(inner);
        if let Poll::Ready(Ok(written)) = result {
            traffic_status::record(written, 0);
            Poll::Ready(Ok(written))
        } else {
            result
        }
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

#[derive(Clone, Copy)]
enum TrafficDirection {
    Tx,
    Rx,
}

struct TrafficCounter<S> {
    inner: S,
    direction: TrafficDirection,
}

impl<S> TrafficCounter<S> {
    fn new(inner: S, direction: TrafficDirection) -> Self {
        Self { inner, direction }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for TrafficCounter<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for TrafficCounter<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = result {
            match self.direction {
                TrafficDirection::Tx => traffic_status::record(written, 0),
                TrafficDirection::Rx => traffic_status::record(0, written),
            }
            Poll::Ready(Ok(written))
        } else {
            result
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
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
    let mn = method_name_unstable!();
    use std::io::Error;
    let addresses = tokio::net::lookup_host(server.to_string()).await?.collect::<Vec<_>>();
    let tcp = TcpStream::connect(&addresses[..]).await?;
    log::trace!("{mn} -- connecting to AnyTLS server {server}");
    let name = match sni {
        Some(sni) => ServerName::try_from(sni.to_owned()).map_err(Error::other)?,
        None => ServerName::try_from(server.host()).map_err(Error::other)?,
    };
    let connector = TlsConnector::from(tls_config);
    let mut tls = connector.connect(name, tcp).await?;
    log::trace!("{mn} -- TLS connection to AnyTLS server {server} established");
    let padding = padding.read().await.clone();
    write_auth_with_client_id(&mut tls, password, &padding, client_id).await?;
    log::trace!("{mn} -- AnyTLS authentication to {server} completed");
    Ok(Box::new(tls))
}

async fn handle_socks5(incoming: IncomingConnection, client: Arc<Client>, context: Arc<ProxyConnectionContext>) -> std::io::Result<()> {
    let mn = method_name_unstable!();
    let request = negotiate_socks5_request(incoming, SOCKS_HANDSHAKE_TIMEOUT).await?;
    let (connect, target) = match request {
        ClientConnection::Connect(connect, target) => {
            context.set_target(target.to_string());
            (connect, target)
        }
        ClientConnection::Bind(mut bind, target) => {
            context.set_target(target.to_string());
            let _ = bind.shutdown().await;
            return Err(std::io::Error::other(format!("SOCKS5 BIND is unsupported: {target}")));
        }
        ClientConnection::UdpAssociate(associate, target) => {
            context.set_target(target.to_string());
            return handle_udp_associate(associate, client).await;
        }
    };
    let started = std::time::Instant::now();
    log::trace!("{mn} -- opening SOCKS5 CONNECT to {target}");
    let stream = client.create_stream().await?;
    let mut remote = TrafficCounter::new(StreamIo::new(stream), TrafficDirection::Tx);
    target.write_to_async_stream(&mut remote).await?;
    let ready = match connect.reply(Reply::Succeeded, Address::unspecified()).await {
        Ok(ready) => ready,
        Err(error) if relay::is_peer_disconnect(&error) => {
            log::debug!("{mn} -- Proxy peer disconnected before SOCKS5 reply: {}: {error}", context.label());
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let mut ready = TrafficCounter::new(ready, TrafficDirection::Rx);
    let (client_to_proxy, proxy_to_client) = match relay::copy_bidirectional(&mut ready, &mut remote).await {
        Ok(counts) => counts,
        Err(error) if error.is_peer_disconnect() => {
            log::debug!("{mn} -- Proxy peer disconnected during SOCKS5 relay: {}: {error}", context.label());
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    ready.shutdown().await?;
    remote.shutdown().await?;
    let elapsed = started.elapsed().as_secs();
    log::trace!("{mn} -- SOCKS5 relay to {target} closed: up={client_to_proxy}, down={proxy_to_client}, elapsed={elapsed}");
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
        let _ = proxy_stream.shutdown_write_by_send_fin_to_remote().await;
        let mut reply = associate.reply(Reply::GeneralFailure, Address::unspecified()).await?;
        reply.shutdown().await?;
        return Err(error);
    }
    if let Err(error) = write_stream_all(&proxy_stream, &request).await {
        let _ = proxy_stream.shutdown_write_by_send_fin_to_remote().await;
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
                traffic_status::record(payload.len(), 0);
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
                traffic_status::record(0, payload.len());
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
    let _ = proxy_stream.shutdown_write_by_send_fin_to_remote().await;
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
    let mn = method_name_unstable!();
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
            log::warn!("{mn} -- Failed to load some native certificates: {:?}", cert_result.errors);
        }
        for cert in cert_result.certs {
            root_store.add(cert).map_err(std::io::Error::other)?;
        }
    }

    if root_store.roots.is_empty() {
        use std::io::{Error, ErrorKind::InvalidInput};
        return Err(Error::new(InvalidInput, "No root certificates available for TLS verification"));
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
