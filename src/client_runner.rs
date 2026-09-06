use crate::AsyncReadWrite;
use crate::core::PaddingFactory;
use crate::parse_url::ClientRuntimeConfig;
use crate::proxy::session::{Client, Stream};
use crate::runtime::DefaultPaddingFactory;
use crate::uot::{UotMode, UotRequest, uot_encode_packet, uot_get_packet_from_stream, uot_sentinel_destination};
use crate::{BoxError, PROGRAM_VERSION_NAME};
use clap::Parser;
use rustls::ClientConfig;
use sha2::{Digest, Sha256};
use socks_hub_core::{BoxedStream, HttpConnector, UserKey, run_http_service};
use socks5_impl::protocol::{Address, ProxyParameters};
use socks5_impl::server::auth::{NoAuth, UserKeyAuth};
use socks5_impl::server::connection::{associate, connect};
use socks5_impl::server::{AssociatedUdpSocket, AuthAdaptor, IncomingConnection, UdpAssociate};
use std::fs::File;
use std::future::Future;
use std::io::BufReader;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader as TokioBufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio_rustls::TlsConnector;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// StreamRw is a lightweight adapter that makes `anytls::proxy::session::Stream` behave
// like a Tokio-compatible async read/write stream. `socks_hub_core::run_http_service`
// expects a boxed stream implementing `AsyncRead + AsyncWrite + Send + Sync` and this
// wrapper forwards reads/writes/close operations to the underlying AnyTLS stream.
struct StreamRw {
    inner: Arc<Stream>,
    #[allow(clippy::type_complexity)]
    read_fut: Option<Pin<Box<dyn Future<Output = std::io::Result<(Vec<u8>, usize)>> + Send + Sync>>>,
    write_fut: Option<Pin<Box<dyn Future<Output = std::io::Result<usize>> + Send + Sync>>>,
    shutdown_fut: Option<Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + Sync>>>,
}

impl StreamRw {
    fn new(inner: Arc<Stream>) -> Self {
        Self {
            inner,
            read_fut: None,
            write_fut: None,
            shutdown_fut: None,
        }
    }
}

impl AsyncRead for StreamRw {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.read_fut.is_none() {
            let remaining = buf.remaining();
            if remaining == 0 {
                return std::task::Poll::Ready(Ok(()));
            }
            let inner = self.inner.clone();
            self.read_fut = Some(Box::pin(async move {
                let mut tmp = vec![0u8; remaining];
                let n = inner.read(&mut tmp).await?;
                Ok((tmp, n))
            }));
        }

        let Some(read_fut) = self.read_fut.as_mut() else {
            return std::task::Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "read future missing")));
        };
        match read_fut.as_mut().poll(cx) {
            std::task::Poll::Ready(Ok((tmp, n))) => {
                self.read_fut = None;
                buf.put_slice(&tmp[..n]);
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Ready(Err(e)) => {
                self.read_fut = None;
                std::task::Poll::Ready(Err(e))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl AsyncWrite for StreamRw {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        if self.write_fut.is_none() {
            let inner = self.inner.clone();
            let data = buf.to_vec();
            self.write_fut = Some(Box::pin(async move { inner.write(&data).await }));
        }

        let Some(write_fut) = self.write_fut.as_mut() else {
            return std::task::Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "write future missing")));
        };
        match write_fut.as_mut().poll(cx) {
            std::task::Poll::Ready(Ok(n)) => {
                self.write_fut = None;
                std::task::Poll::Ready(Ok(n))
            }
            std::task::Poll::Ready(Err(e)) => {
                self.write_fut = None;
                std::task::Poll::Ready(Err(e))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        if self.shutdown_fut.is_none() {
            let inner = self.inner.clone();
            self.shutdown_fut = Some(Box::pin(async move { inner.shutdown_write().await }));
        }

        let Some(shutdown_fut) = self.shutdown_fut.as_mut() else {
            return std::task::Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "shutdown future missing")));
        };
        match shutdown_fut.as_mut().poll(cx) {
            std::task::Poll::Ready(Ok(())) => {
                self.shutdown_fut = None;
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Ready(Err(e)) => {
                self.shutdown_fut = None;
                std::task::Poll::Ready(Err(e))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

const MAX_UDP_RELAY_PACKET_SIZE: usize = 65_535;

#[derive(Parser, serde::Serialize, serde::Deserialize, Debug, Clone)]
#[command(version, author, name = "anytls-client", about = "AnyTLS Client")]
pub struct ClientArgs {
    /// AnyTLS URI in the format anytls://[auth@]hostname[:port]/?[key=value]&[key=value]...#fragment
    #[arg(short, long, value_name = "URL")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<url::Url>,

    /// URL of SOCKS5 and HTTP CONNECT listen parameters (e.g. "socks5://[user[:password]@]127.0.0.1:1080")
    #[arg(short = 'l', long, value_name = "URL", default_value = "socks5://127.0.0.1:1080")]
    pub listen: ProxyParameters,

    /// Optional IP address advertised to SOCKS5 UDP associate clients
    #[arg(long, value_name = "IP")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advertise_ip: Option<IpAddr>,

    #[arg(short = 's', long, value_name = "IP:PORT", help = "Server address")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<SocketAddr>,

    #[arg(long, value_name = "SNI", help = "TLS server name indication (SNI)")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sni: Option<String>,

    /// Optional man in the middle (MITM) HTTP CONNECT proxy used for the client's outbound connection to the AnyTLS server
    #[arg(long, value_name = "IP:PORT")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mitm: Option<SocketAddr>,

    #[arg(short = 'p', long, help = "Password for anytls server authentication")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,

    #[arg(long, value_name = "UUID", value_parser = clap::value_parser!(Uuid), help = "Client UUID")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<Uuid>,

    /// Allow an insecure TLS connection
    #[arg(long, value_name = "BOOL", num_args(0..=1), value_parser = clap::value_parser!(bool))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insecure: Option<bool>,

    #[arg(long, value_name = "FILE", help = "Padding scheme file")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding_scheme: Option<PathBuf>,

    #[arg(long, value_name = "FILE", help = "Root CA certificate PEM file to verify server (optional)")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_cert: Option<PathBuf>,

    /// Maximum logical streams per AnyTLS session, if is 1 then multiplexing is disabled
    #[arg(short, long, default_value_t = 5, value_name = "N")]
    #[serde(skip)]
    pub max_streams_per_session: usize,

    #[arg(long, help = "Print the equivalent AnyTLS URI and exit")]
    #[serde(skip)]
    pub print_url: bool,

    /// Log level (off, error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    #[serde(skip, default = "default_log_level")]
    pub log: log::LevelFilter,
}

fn default_log_level() -> log::LevelFilter {
    log::LevelFilter::Info
}

pub fn resolve_client_config(args: &ClientArgs) -> std::io::Result<ClientRuntimeConfig> {
    let mut config = if let Some(url) = &args.url {
        url.try_into()?
    } else {
        ClientRuntimeConfig {
            server: Address::DomainAddress(Box::<str>::from(""), 443),
            ..Default::default()
        }
    };

    if let Some(server) = args.server {
        config.server = server.into();
    }

    if let Some(password) = &args.password {
        config.password = password.clone();
    }

    if let Some(sni) = &args.sni {
        config.sni = Some(sni.clone());
    }

    if let Some(client_id) = args.client_id {
        config.client_id = Some(client_id);
    }

    if let Some(insecure) = args.insecure {
        config.insecure = insecure;
    }

    use std::io::{Error, ErrorKind::InvalidInput};
    if config.server.host().is_empty() {
        return Err(Error::new(InvalidInput, "Server address is required (use --server or --url)"));
    }

    Ok(config)
}

struct StreamReader {
    inner: Arc<Stream>,
    #[allow(clippy::type_complexity)]
    read_fut: Option<std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<(Vec<u8>, usize)>> + Send>>>,
}

impl StreamReader {
    fn new(inner: Arc<Stream>) -> Self {
        Self { inner, read_fut: None }
    }
}

impl AsyncRead for StreamReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        loop {
            if let Some(fut) = self.read_fut.as_mut() {
                match fut.as_mut().poll(cx) {
                    std::task::Poll::Ready(Ok((v, n))) => {
                        self.read_fut = None;
                        buf.put_slice(&v[..n]);
                        return std::task::Poll::Ready(Ok(()));
                    }
                    std::task::Poll::Ready(Err(e)) => {
                        self.read_fut = None;
                        return std::task::Poll::Ready(Err(e));
                    }
                    std::task::Poll::Pending => return std::task::Poll::Pending,
                }
            }

            let remaining = buf.remaining();
            if remaining == 0 {
                return std::task::Poll::Ready(Ok(()));
            }

            let inner = self.inner.clone();
            self.read_fut = Some(Box::pin(async move {
                let mut v = vec![0_u8; remaining];
                let n = inner.read(&mut v).await?;
                Ok::<(Vec<u8>, usize), std::io::Error>((v, n))
            }));
        }
    }
}

pub async fn runner_execute(cancel_token: CancellationToken, args: ClientArgs) -> Result<(), BoxError> {
    use std::io::{Error, ErrorKind::InvalidInput};
    let config = resolve_client_config(&args)?;

    if let Some(padding_scheme) = &args.padding_scheme {
        let content = tokio::fs::read(padding_scheme).await?;
        if !DefaultPaddingFactory::update(&content).await {
            return Err(Error::new(
                InvalidInput,
                format!("Wrong format padding scheme file: {}", padding_scheme.display()),
            )
            .into());
        }
        log::info!("Loaded padding scheme file: {}", padding_scheme.display());
    }

    let password_sha256: [u8; 32] = Sha256::digest(config.password.as_bytes()).into();
    let client_id = config.client_id;

    log::info!("[Client] {}", PROGRAM_VERSION_NAME);
    log::info!("[Client] SOCKS5 or HTTP CONNECT {} => {}", args.listen.addr, config.authority());

    let tls_config = create_tls_config(args.root_cert.as_deref(), config.insecure)?;
    let padding = DefaultPaddingFactory::load();
    let max_streams_per_session = args.max_streams_per_session.max(1);

    let padding_clone = padding.clone();
    let server = config.server.clone();
    let server_sni = config.sni.clone();
    let client = Arc::new(Client::new(
        Box::new(move || {
            Box::pin(dail_out_callback(
                server.clone(),
                server_sni.clone(),
                args.mitm,
                tls_config.clone(),
                padding_clone.clone(),
                password_sha256,
                client_id,
            ))
        }),
        padding,
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(30),
        5,
        max_streams_per_session,
    ));

    let listen: SocketAddr = args.listen.addr.try_into()?;

    let auth: AuthAdaptor = match &args.listen.credentials {
        Some(creds) => Arc::new(UserKeyAuth::from(creds)),
        None => Arc::new(NoAuth),
    };
    let credentials: UserKey = args.listen.credentials.clone().unwrap_or_default();

    let listener = TcpListener::bind(listen).await?;
    log::info!("[Client] Listening on {} (SOCKS5 + HTTP mixed)", listen);

    let connector_client = client.clone();
    let connector: HttpConnector = Arc::new(move |dst: Address| {
        let client = connector_client.clone();
        Box::pin(async move {
            let proxy_stream = client.create_stream().await?;
            let addr_data: Vec<u8> = dst.into();
            // Opening a stream is non-blocking in v2: queue the target
            // address immediately and let the core SYNACK watchdog handle a
            // missing or failed remote handshake.
            let handshake_stream = proxy_stream.clone();
            let mut adapter = StreamRw::new(proxy_stream);
            adapter.write_all(&addr_data).await?;
            let handshake_result = tokio::time::timeout(std::time::Duration::from_secs(10), handshake_stream.wait_for_handshake())
                .await
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "stream handshake timed out"))
                .and_then(|result| result);
            if let Err(error) = handshake_result {
                let _ = handshake_stream.terminate().await;
                return Err(error);
            }
            let boxed: BoxedStream = Box::new(adapter);
            Ok(boxed)
        })
    });

    loop {
        let cancel_token = cancel_token.clone();
        let auth = auth.clone();
        let connector = connector.clone();
        let client = client.clone();
        let credentials = credentials.clone();
        let advertise_ip = args.advertise_ip;

        let (stream, addr) = tokio::select! {
            _ = cancel_token.cancelled() => {
                log::info!("Shutting down client...");
                client.close().await?;
                break Ok(());
            }
            res = listener.accept() => res?,
        };

        tokio::spawn(async move {
            if let Err(e) = handle_listener_stream(stream, auth, connector, client, advertise_ip, credentials).await {
                log::error!("Connection from {addr} error: {e}");
            }
        });
    }
}

async fn handle_listener_stream(
    mut stream: TcpStream,
    auth: AuthAdaptor,
    connector: HttpConnector,
    client: Arc<Client>,
    advertise_ip: Option<IpAddr>,
    credentials: UserKey,
) -> Result<(), BoxError> {
    let mut peek_buf = [0u8; 10];
    let n = stream.peek(&mut peek_buf).await?;
    if n == 0 {
        return Ok(());
    }

    let peer_addr = stream.peer_addr().ok();

    match peek_buf[0] {
        0x05 => {
            log::trace!("SOCKS5 client detected from {peer_addr:?}");
            let incoming = IncomingConnection::new(stream, auth);
            handle_connection(incoming, client, advertise_ip).await?;
            log::trace!("SOCKS5 client from {peer_addr:?} disconnected");
            Ok(())
        }
        0x04 => {
            log::warn!("socks4 client detected from {peer_addr:?}, but only SOCKS5/HTTP mixed mode is supported",);
            let _ = stream.shutdown().await;
            Ok(())
        }
        _ => {
            let first_bytes = &peek_buf[..n];
            let is_http = if let Ok(text) = std::str::from_utf8(first_bytes) {
                let methods = [
                    "CONNECT", "GET", "POST", "HEAD", "PUT", "OPTIONS", "DELETE", "TRACE", "PATCH", "LOCK", "UNLOCK", "PROPFIND", "MKCOL",
                    "COPY", "MOVE",
                ];
                methods.iter().any(|method| {
                    method.len() <= text.len()
                        && text[..method.len()].eq_ignore_ascii_case(method)
                        && text.as_bytes().get(method.len()) == Some(&b' ')
                })
            } else {
                false
            };

            if !is_http {
                let fb = first_bytes[0];
                log::warn!("unknown client type detected from {peer_addr:?}, first byte: 0x{fb:02x}",);
                let _ = stream.shutdown().await;
                return Ok(());
            }

            log::trace!("HTTP client detected from {peer_addr:?}");
            run_http_service(stream, connector, credentials).await?;
            log::trace!("HTTP client from {peer_addr:?} disconnected");
            Ok(())
        }
    }
}

async fn dail_out_callback(
    server: Address,
    sni: Option<String>,
    mitm: Option<SocketAddr>,
    tls_config: Arc<ClientConfig>,
    padding: Arc<tokio::sync::RwLock<PaddingFactory>>,
    password_sha256: [u8; 32],
    client_id: Option<Uuid>,
) -> std::io::Result<Box<dyn AsyncReadWrite>> {
    let sni = sni.clone();
    let server_authority = crate::parse_url::format_authority(&server);
    let stream = if let Some(proxy_addr) = mitm {
        connect_via_mitm_proxy(proxy_addr, &server_authority).await?
    } else {
        TcpStream::connect(&server_authority).await?
    };
    stream.set_nodelay(true)?;
    let ka = socket2::TcpKeepalive::new()
        .with_time(std::time::Duration::from_secs(60))
        .with_interval(std::time::Duration::from_secs(10));
    socket2::SockRef::from(&stream).set_tcp_keepalive(&ka)?;

    use rustls::pki_types::ServerName;
    let server_name = if let Some(sni) = sni {
        if let Ok(ip) = sni.parse::<std::net::IpAddr>() {
            ServerName::IpAddress(ip.into())
        } else {
            // For domain, use owned string
            use std::io::{Error, ErrorKind::InvalidInput};
            ServerName::try_from(sni).map_err(|e| Error::new(InvalidInput, e))?
        }
    } else {
        match &server {
            Address::SocketAddress(socket_addr) => ServerName::IpAddress(socket_addr.ip().into()),
            Address::DomainAddress(domain, _) => {
                if let Ok(ip) = domain.parse::<std::net::IpAddr>() {
                    ServerName::IpAddress(ip.into())
                } else {
                    let domain = domain.as_ref().to_owned();
                    use std::io::{Error, ErrorKind::InvalidInput};
                    ServerName::try_from(domain).map_err(|e| Error::new(InvalidInput, e))?
                }
            }
        }
    };

    let connector = TlsConnector::from(tls_config);
    let mut tls_stream = connector.connect(server_name, stream).await?;

    // Send authentication. Password hash is always sent first, followed by the padding length
    // field and padding bytes. We embed the optional client_id string in the padding area.
    let client_id_bytes = client_id.as_ref().map(|id| id.to_string().into_bytes()).unwrap_or_default();

    let padding_factory = padding.read().await;
    let padding_sizes = padding_factory.generate_record_payload_sizes(0);
    let mut padding_len = if !padding_sizes.is_empty() { padding_sizes[0] as u16 } else { 0 };
    if !client_id_bytes.is_empty() {
        padding_len = padding_len.max(client_id_bytes.len() as u16);
    }

    let mut auth_data = Vec::with_capacity(34 + padding_len as usize);
    auth_data.extend_from_slice(&password_sha256);
    auth_data.extend_from_slice(&padding_len.to_be_bytes());

    if padding_len > 0 {
        let mut padding_data = vec![0u8; padding_len as usize];
        if !client_id_bytes.is_empty() {
            padding_data[..client_id_bytes.len()].copy_from_slice(&client_id_bytes);
        }
        auth_data.extend_from_slice(&padding_data);
    }

    // Send auth data
    tls_stream.write_all(&auth_data).await?;

    Ok(Box::new(tls_stream) as Box<dyn AsyncReadWrite>)
}

async fn connect_via_mitm_proxy(proxy_addr: SocketAddr, target_authority: &str) -> std::io::Result<TcpStream> {
    let mut stream = TcpStream::connect(proxy_addr).await?;
    stream.set_nodelay(true)?;

    let connect_request =
        format!("CONNECT {target_authority} HTTP/1.1\r\nHost: {target_authority}\r\nProxy-Connection: Keep-Alive\r\n\r\n");
    stream.write_all(connect_request.as_bytes()).await?;

    let mut reader = TokioBufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).await?;
    if !status_line.starts_with("HTTP/1.1 200") && !status_line.starts_with("HTTP/1.0 200") {
        use std::io::Error;
        return Err(Error::other(format!("HTTP proxy CONNECT failed: {}", status_line.trim_end())));
    }

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let stream = reader.into_inner();
    stream.set_nodelay(true)?;
    Ok(stream)
}

fn create_tls_config(root_cert: Option<&Path>, insecure: bool) -> Result<Arc<ClientConfig>, BoxError> {
    if !insecure {
        let mut root_store = rustls::RootCertStore::empty();

        if let Some(path) = root_cert {
            let file = File::open(path)?;
            let mut reader = BufReader::new(file);
            let certs_iter = rustls_pemfile::certs(&mut reader);
            let certs: Vec<rustls::pki_types::CertificateDer<'static>> = certs_iter.collect::<Result<_, _>>()?;

            for cert in certs {
                root_store.add(cert)?;
            }
        } else {
            let cert_result = rustls_native_certs::load_native_certs();
            if !cert_result.errors.is_empty() {
                log::warn!("Failed to load some native certs: {:?}", cert_result.errors);
            }
            for cert in cert_result.certs {
                root_store.add(cert)?;
            }
        }

        if root_store.roots.is_empty() {
            use std::io::{Error, ErrorKind::InvalidInput};
            return Err(Error::new(InvalidInput, "No root certificates available for TLS verification").into());
        }

        let config = ClientConfig::builder().with_root_certificates(root_store).with_no_client_auth();

        return Ok(Arc::new(config));
    }

    // Insecure mode: accept any certificate.
    let mut config = ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();

    config.dangerous().set_certificate_verifier(Arc::new(AllowAnyCertVerifier));

    Ok(Arc::new(config))
}

// 允许任何证书的验证器
#[derive(Debug)]
struct AllowAnyCertVerifier;

impl rustls::client::danger::ServerCertVerifier for AllowAnyCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA1,
            rustls::SignatureScheme::ECDSA_SHA1_Legacy,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::ED448,
        ]
    }
}

async fn handle_connection(incoming: IncomingConnection, client: Arc<Client>, advertise_ip: Option<IpAddr>) -> Result<(), BoxError> {
    // perform handshake/authentication
    let authenticated = incoming.authenticate().await?;
    let client_conn = authenticated.wait_request().await?;

    use socks5_impl::protocol::Reply;
    use socks5_impl::server::connection::ClientConnection;

    match client_conn {
        ClientConnection::Connect(conn_need_reply, addr) => {
            // Reply to client with success and upgrade to Ready
            let conn_ready = conn_need_reply.reply(Reply::Succeeded, addr.clone()).await?;
            s5_connect(conn_ready, addr, client).await?;
        }
        ClientConnection::UdpAssociate(associate, _) => {
            handle_udp_associate(associate, client, advertise_ip).await?;
        }
        ClientConnection::Bind(_, _) => {
            log::warn!("Bind command is not supported");
            return Err("Bind command is not supported".into());
        }
    };
    Ok(())
}

async fn s5_connect(conn_ready: connect::Connect<connect::Ready>, target_addr: Address, client: Arc<Client>) -> std::io::Result<()> {
    log::info!("Connecting to target via proxy: {}", target_addr);

    // 创建到代理服务器的连接
    let proxy_stream = client.create_stream().await?;
    let sid = proxy_stream.id();
    {
        // Debug: check is_terminated first, then take pointer (as integer) and log in a short scope
        let is_terminated = proxy_stream.is_terminated().await;
        let session_ptr_val = Arc::as_ptr(&proxy_stream) as usize;
        log::debug!("Session #{sid}: acquired proxy session ptr=0x{session_ptr_val:x} is_terminated={is_terminated}",);
    }

    // 发送目标地址给代理服务器
    let addr_data: Vec<u8> = target_addr.into();
    let written = proxy_stream.write(&addr_data).await?;
    log::debug!(
        "Session #{sid}: wrote target addr {} bytes to proxy (expected {})",
        written,
        addr_data.len()
    );

    // 开始数据转发
    let (mut client_read, mut client_write) = conn_ready.into_split();
    let proxy_stream_read = proxy_stream.clone();
    let proxy_stream_write = proxy_stream.clone();

    // Client -> Proxy
    let c2p = tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        let mut err = None;
        let mut local_eof = false;
        loop {
            match client_read.read(&mut buf).await {
                Ok(0) => {
                    local_eof = true;
                    break;
                }
                Ok(n) => {
                    log::trace!("s5_connect: client->proxy forwarding {} bytes", n);
                    if let Err(e) = proxy_stream_write.write(&buf[..n]).await {
                        err = Some(e);
                        break;
                    }
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = err {
            let _ = proxy_stream_write.terminate().await;
            log::debug!("Session #{sid}: client to proxy error: {e}");
        } else if local_eof {
            log::debug!("Session #{sid}: local EOF, sending FIN");
            let _ = proxy_stream_write.shutdown_write().await;
        }
    });

    // Proxy -> Client
    let p2c = tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        let mut err = None;
        loop {
            match proxy_stream_read.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    log::trace!("s5_connect: proxy->client forwarding {} bytes", n);
                    match client_write.write_all(&buf[..n]).await {
                        Ok(()) => log::trace!("s5_connect: proxy->client wrote {} bytes", n),
                        Err(e) => {
                            log::debug!("Session #{sid}: proxy to client write failed after {} bytes: {e}", n);
                            err = Some(e);
                            break;
                        }
                    }
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let _ = client_write.shutdown().await;
        if let Some(e) = err {
            log::debug!("Session #{sid}: proxy to client error: {e}");
        }
    });

    let _ = tokio::join!(c2p, p2c);

    Ok(())
}

async fn handle_udp_associate(
    associate: UdpAssociate<associate::NeedReply>,
    client: Arc<Client>,
    advertise_ip: Option<IpAddr>,
) -> Result<(), BoxError> {
    use socks5_impl::protocol::Reply;

    let tcp_local_addr = associate.local_addr()?;
    let udp_bind_ip = tcp_local_addr.ip();
    let udp_listener = UdpSocket::bind(SocketAddr::from((udp_bind_ip, 0))).await;

    let (udp_listener, listen_addr) = match udp_listener.and_then(|socket| socket.local_addr().map(|addr| (socket, addr))) {
        Ok(v) => v,
        Err(err) => {
            let mut reply_listener = associate.reply(Reply::GeneralFailure, Address::unspecified()).await?;
            reply_listener.shutdown().await?;
            return Err(err.into());
        }
    };

    let proxy_stream = match client.create_stream().await {
        Ok(stream) => stream,
        Err(err) => {
            let mut reply_listener = associate.reply(Reply::GeneralFailure, Address::unspecified()).await?;
            reply_listener.shutdown().await?;
            return Err(err.into());
        }
    };

    if let Err(err) = async {
        log::debug!("Session #{}: starting UDP associate", proxy_stream.id());
        let outer_addr: Vec<u8> = uot_sentinel_destination().into();
        proxy_stream.write(&outer_addr).await?;

        let request_bytes: Vec<u8> = UotRequest::new(UotMode::Datagram, Address::unspecified()).into();
        proxy_stream.write(&request_bytes).await?;

        Ok::<(), BoxError>(())
    }
    .await
    {
        let _ = proxy_stream.terminate().await;
        let mut reply_listener = associate.reply(Reply::GeneralFailure, Address::unspecified()).await?;
        reply_listener.shutdown().await?;
        return Err(err);
    }

    let advertised_addr = SocketAddr::new(advertise_ip.unwrap_or(tcp_local_addr.ip()), listen_addr.port());
    let mut reply_listener = associate.reply(Reply::Succeeded, Address::from(advertised_addr)).await?;
    let listen_udp = Arc::new(AssociatedUdpSocket::from((udp_listener, MAX_UDP_RELAY_PACKET_SIZE)));
    let zero_ip = match listen_addr {
        SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    };
    let incoming_addr = Arc::new(tokio::sync::Mutex::new(SocketAddr::from((zero_ip, 0))));
    let proxy_writer = proxy_stream.clone();
    let mut proxy_reader = StreamReader::new(proxy_stream.clone());

    let result: Result<(), BoxError> = loop {
        tokio::select! {
            res = listen_udp.recv_from() => {
                let (pkt, frag, destination, src_addr) = res?;
                if frag != 0 {
                    break Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "UDP fragmentation is not supported").into());
                }

                *incoming_addr.lock().await = src_addr;
                let frame = uot_encode_packet(UotMode::Datagram, Some(&destination), &pkt)?;
                proxy_writer.write(&frame).await?;
            }
            res = uot_get_packet_from_stream(UotMode::Datagram, &mut proxy_reader) => {
                let (source, payload) = res?;
                let incoming = *incoming_addr.lock().await;
                if incoming.port() == 0 {
                    continue;
                }

                let Some(source) = source else {
                    break Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "UOT datagram response missing source address").into());
                };
                listen_udp.send_to(&payload, 0, source, incoming).await?;
            }
            res = reply_listener.wait_until_closed() => {
                res?;
                break Ok(());
            }
        }
    };

    if result.is_ok() {
        let _ = proxy_stream.close().await;
    } else {
        let _ = proxy_stream.terminate().await;
    }
    let _ = reply_listener.shutdown().await;
    result
}
