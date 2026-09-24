use anytls::{
    padding::{DEFAULT_SCHEME, PaddingFactory},
    panel_sync::{PanelSyncClient, PanelSyncConfig, TrafficAudit, TrafficAuditPtr},
    session::{BoxTransport, Session, Stream, is_peer_disconnect},
    stream_io::StreamIo,
    uot::{UotMode, uot_encode_packet, uot_get_packet_from_stream, uot_get_request_from_stream, uot_is_sentinel_destination},
};
use clap::Parser;
use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
};
use socks5_impl::protocol::{Address, AsyncStreamOperation};
use std::{
    net::{SocketAddr, ToSocketAddrs},
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::time::Duration;
use tokio_rustls::TlsAcceptor;
use url::Url;
use uuid::Uuid;

#[derive(Parser, serde::Serialize, serde::Deserialize, Debug, Clone)]
#[command(version, author, name = "anytls-server", about = "AnyTLS rust server")]
struct Args {
    /// Server listen port
    #[arg(short = 'l', long, value_name = "IP:PORT", default_value = "0.0.0.0:8443")]
    listen: SocketAddr,

    /// Password for anytls server authentication
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(short = 'p', long)]
    password: Option<String>,

    /// Maximum logical streams accepted on one authenticated TLS session
    #[arg(short = 'm', long, value_name = "N", default_value_t = 1024)]
    max_streams_per_session: usize,

    /// TLS certificate PEM file (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, value_name = "FILE")]
    cert: Option<PathBuf>,

    /// TLS private key PEM file (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, value_name = "FILE")]
    key: Option<PathBuf>,

    /// Panel webapi base url for sync
    #[arg(long, value_name = "URL")]
    #[serde(skip_serializing_if = "Option::is_none")]
    panel_webapi_url: Option<Url>,

    /// Panel webapi token
    #[arg(long, value_name = "TOKEN")]
    #[serde(skip_serializing_if = "Option::is_none")]
    panel_webapi_token: Option<String>,

    /// Panel node id
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, value_name = "ID")]
    panel_node_id: Option<usize>,

    /// Panel API update interval seconds
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, value_name = "SECS")]
    panel_update_interval_secs: Option<u64>,

    /// Log level (off, error, warn, info, debug, trace)
    #[serde(skip, default = "default_log_level")]
    #[arg(long, value_name = "LEVEL", default_value = "info")]
    log: log::LevelFilter,
}

fn default_log_level() -> log::LevelFilter {
    log::LevelFilter::Info
}

impl Args {
    fn panel_sync_config(&self) -> std::io::Result<Option<PanelSyncConfig>> {
        let has_required_value = self.panel_webapi_url.is_some() || self.panel_webapi_token.is_some() || self.panel_node_id.is_some();
        if !has_required_value && self.panel_update_interval_secs.is_none() {
            return Ok(None);
        }

        match (&self.panel_webapi_url, &self.panel_webapi_token, self.panel_node_id) {
            (Some(webapi_url), Some(webapi_token), Some(node_id)) => Ok(Some(PanelSyncConfig {
                webapi_url: webapi_url.clone(),
                webapi_token: webapi_token.clone(),
                node_id,
                update_interval_secs: self.panel_update_interval_secs.unwrap_or(10).max(5),
            })),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--panel-webapi-url, --panel-webapi-token, and --panel-node-id must be provided together",
            )),
        }
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let log_level = args.log.to_string().to_ascii_lowercase();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level)).init();
    let acceptor = TlsAcceptor::from(Arc::new(tls_config(args.cert.as_deref(), args.key.as_deref())?));
    let panel_config = args.panel_sync_config()?;
    let panel_sync_enabled = panel_config.is_some();
    let traffic_audit: TrafficAuditPtr = Arc::new(tokio::sync::Mutex::new(TrafficAudit::new()));
    if let Some(config) = panel_config {
        let mut panel_client = PanelSyncClient::new(config);
        panel_client.sync_initial(&traffic_audit).await?;
        let audit = Arc::clone(&traffic_audit);
        tokio::spawn(async move { panel_client.run(audit).await });
    }
    let listener = TcpListener::bind(args.listen).await?;
    log::info!("AnyTLS server listening on {}", args.listen);
    let padding = Arc::new(tokio::sync::RwLock::new(
        PaddingFactory::new(DEFAULT_SCHEME).expect("default padding"),
    ));
    let password = Arc::new(args.password.clone().unwrap_or_default());
    let mut session_id = 0usize;
    let mut last_emfile_warning = tokio::time::Instant::now() - Duration::from_secs(5);
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock || error.raw_os_error() == Some(24) => {
                if last_emfile_warning.elapsed() >= Duration::from_secs(5) {
                    log::warn!("accept temporarily failed due to exhausted descriptors: {error}");
                    last_emfile_warning = tokio::time::Instant::now();
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        session_id = session_id.wrapping_add(1);
        let acceptor = acceptor.clone();
        let padding = padding.clone();
        let password = password.clone();
        let max_streams = args.max_streams_per_session;
        let traffic_audit = Arc::clone(&traffic_audit);
        tokio::spawn(async move {
            log::info!("accepted TLS session {session_id} from {peer}");
            if let Err(error) = handle_connection(
                tcp,
                acceptor,
                padding,
                password,
                session_id,
                max_streams,
                traffic_audit,
                panel_sync_enabled,
            )
            .await
            {
                log::warn!("session {session_id} from {peer} failed: {error}");
            } else {
                log::info!("session {session_id} from {peer} closed");
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    tcp: TcpStream,
    acceptor: TlsAcceptor,
    padding: Arc<tokio::sync::RwLock<PaddingFactory>>,
    password: Arc<String>,
    session_id: usize,
    max_streams: usize,
    traffic_audit: TrafficAuditPtr,
    panel_sync_enabled: bool,
) -> std::io::Result<()> {
    let mut tls = acceptor.accept(tcp).await?;
    let client_id = anytls::auth::read_auth_with_client_id(&mut tls, &password).await?;
    if panel_sync_enabled {
        let approved = match client_id {
            Some(client_id) => traffic_audit.lock().await.is_enabled(&client_id),
            None => false,
        };
        if !approved {
            log::info!("session {session_id}: denied panel-managed client {client_id:?}");
            return Ok(());
        }
    }
    log::info!("session {session_id}: TLS and AnyTLS authentication completed for {client_id:?}");

    let session = Session::new_server(session_id, Box::new(tls) as BoxTransport, padding, max_streams);
    session.run().await?;
    loop {
        let stream = match session.accept_stream().await {
            Ok(stream) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                log::info!("session {session_id}: session closed by peer");
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let traffic_audit = Arc::clone(&traffic_audit);
        tokio::spawn(async move {
            let stream_id = stream.id();
            if let Err(error) = relay_stream(stream, traffic_audit, client_id, panel_sync_enabled).await {
                if is_peer_disconnect(&error) {
                    log::debug!("session {session_id} stream {stream_id} peer disconnected: {error}");
                } else {
                    log::warn!("session {session_id} stream {stream_id} relay failed: {error}");
                }
            }
        });
    }
}

async fn relay_stream(
    stream: Stream,
    traffic_audit: TrafficAuditPtr,
    client_id: Option<Uuid>,
    panel_sync_enabled: bool,
) -> std::io::Result<()> {
    let session_id = stream.session_id().unwrap_or_default();
    let stream_id = stream.id();
    let started = std::time::Instant::now();
    let mut stream_io = StreamIo::new(stream);
    let destination = read_target(&mut stream_io).await?;
    if panel_sync_enabled {
        let enabled = match client_id {
            Some(client_id) => traffic_audit.lock().await.is_enabled(&client_id),
            None => false,
        };
        if !enabled {
            let message = "panel-managed client is disabled";
            stream_io.handshake_failure(message).await?;
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, message));
        }
    }
    if uot_is_sentinel_destination(&destination) {
        return relay_uot_datagrams(stream_io, traffic_audit, client_id).await;
    }
    log::debug!("session {session_id} stream {stream_id}: connecting to {destination}");
    let addresses: Vec<SocketAddr> = destination.to_socket_addrs()?.collect();
    let outbound = match TcpStream::connect(&addresses[..]).await {
        Ok(outbound) => outbound,
        Err(error) => {
            let _ = stream_io.handshake_failure(&error.to_string()).await;
            return Err(error);
        }
    };
    stream_io.handshake_success().await?;
    let upstream_bytes = Arc::new(AtomicU64::new(0));
    let downstream_bytes = Arc::new(AtomicU64::new(0));
    let mut stream_io = CountedIo::new(stream_io, Arc::clone(&downstream_bytes));
    let mut outbound = CountedIo::new(outbound, Arc::clone(&upstream_bytes));
    let relay_result = tokio::io::copy_bidirectional(&mut stream_io, &mut outbound).await;
    let upstream = upstream_bytes.load(Ordering::Relaxed);
    let downstream = downstream_bytes.load(Ordering::Relaxed);
    if let Some(client_id) = client_id {
        let mut audit = traffic_audit.lock().await;
        audit.add_upstream(&client_id, upstream);
        audit.add_downstream(&client_id, downstream);
    }
    let _ = stream_io.shutdown().await;
    let _ = outbound.shutdown().await;
    match relay_result {
        Ok(_) => {
            log::info!(
                "session {session_id} stream {stream_id}: relay to {destination} closed: stream_to_target={upstream} bytes, target_to_stream={downstream} bytes, elapsed={:?}",
                started.elapsed()
            );
            Ok(())
        }
        Err(error) if is_peer_disconnect(&error) => {
            log::debug!("session {session_id} stream {stream_id}: peer reset/closed relay: {error}");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

struct CountedIo<T> {
    inner: T,
    written: Arc<AtomicU64>,
}

impl<T> CountedIo<T> {
    fn new(inner: T, written: Arc<AtomicU64>) -> Self {
        Self { inner, written }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for CountedIo<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for CountedIo<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = result {
            self.written.fetch_add(written as u64, Ordering::Relaxed);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

async fn relay_uot_datagrams(mut stream_io: StreamIo, traffic_audit: TrafficAuditPtr, client_id: Option<Uuid>) -> std::io::Result<()> {
    let stream = stream_io.stream();
    let request = uot_get_request_from_stream(&mut stream_io).await?;
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    if request.mode == UotMode::Connected {
        socket.connect(request.destination.to_string()).await?;
    }
    stream_io.handshake_success().await?;
    let mut outbound_buf = vec![0u8; 65_535];
    let (packet_sender, mut packet_receiver) = tokio::sync::mpsc::channel(32);
    let mode = request.mode;
    let reader_task = tokio::spawn(async move {
        loop {
            let packet = uot_get_packet_from_stream(mode, &mut stream_io).await;
            let failed = packet.is_err();
            if packet_sender.send(packet).await.is_err() || failed {
                break;
            }
        }
    });
    let result: std::io::Result<()> = async {
        loop {
            tokio::select! {
            packet = packet_receiver.recv() => {
                let packet = packet.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "UOT reader stopped"))?;
                let (destination, payload) = match packet {
                    Ok(packet) => packet,
                    Err(error) if anytls::session::is_peer_disconnect(&error) => break Ok(()),
                    Err(error) => break Err(error),
                };
                match request.mode {
                    UotMode::Datagram => {
                        let destination = destination.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "UOT datagram has no destination"))?;
                        socket.send_to(&payload, destination.to_string()).await?;
                    }
                    UotMode::Connected => {
                        socket.send(&payload).await?;
                    }
                }
                if let Some(client_id) = client_id {
                    traffic_audit.lock().await.add_upstream(&client_id, payload.len() as u64);
                }
            }
            packet = socket.recv_from(&mut outbound_buf) => {
                let (size, source) = packet?;
                let source = Address::from(source);
                let frame = match request.mode {
                    UotMode::Datagram => uot_encode_packet(UotMode::Datagram, Some(&source), &outbound_buf[..size])?,
                    UotMode::Connected => uot_encode_packet(UotMode::Connected, None, &outbound_buf[..size])?,
                };
                write_stream_all(&stream, &frame).await?;
                if let Some(client_id) = client_id {
                    traffic_audit.lock().await.add_downstream(&client_id, size as u64);
                }
            }
            }
        }
    }
    .await;
    reader_task.abort();
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

async fn read_target(stream: &mut StreamIo) -> std::io::Result<Address> {
    Address::retrieve_from_async_stream(stream).await
}

fn tls_config(cert_path: Option<&std::path::Path>, key_path: Option<&std::path::Path>) -> std::io::Result<ServerConfig> {
    use std::io::{Error, ErrorKind::InvalidData, ErrorKind::InvalidInput};
    if let (Some(cert_path), Some(key_path)) = (cert_path, key_path) {
        let cert_file = std::fs::File::open(cert_path)?;
        let mut cert_reader = std::io::BufReader::new(cert_file);
        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader).collect::<Result<_, _>>()?;
        if certs.is_empty() {
            return Err(Error::new(InvalidData, "failed to parse certificate PEM"));
        }

        let key_file = std::fs::File::open(key_path)?;
        let mut key_reader = std::io::BufReader::new(key_file);
        let key = rustls_pemfile::private_key(&mut key_reader)?
            .ok_or_else(|| Error::new(InvalidData, "failed to parse a supported private key"))?;

        return ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(std::io::Error::other);
    }

    if cert_path.is_some() || key_path.is_some() {
        return Err(Error::new(InvalidInput, "both --cert and --key must be provided"));
    }

    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).map_err(Error::other)?;
    let cert = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()));
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tls_config_tests {
    use super::{Args, tls_config};
    use clap::Parser;
    use std::path::Path;

    #[test]
    fn requires_certificate_and_key_together() {
        let cert_only = tls_config(Some(Path::new("certificate.pem")), None).unwrap_err();
        assert_eq!(cert_only.kind(), std::io::ErrorKind::InvalidInput);

        let key_only = tls_config(None, Some(Path::new("private-key.pem"))).unwrap_err();
        assert_eq!(key_only.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn panel_sync_requires_all_connection_parameters_and_clamps_interval() {
        let args = Args::try_parse_from([
            "anytls-server",
            "--password",
            "secret",
            "--panel-webapi-url",
            "https://panel.example/api",
            "--panel-webapi-token",
            "token",
            "--panel-node-id",
            "42",
            "--panel-update-interval-secs",
            "2",
        ])
        .unwrap();
        assert_eq!(args.panel_sync_config().unwrap().unwrap().update_interval_secs, 5);

        let incomplete = Args::try_parse_from([
            "anytls-server",
            "--password",
            "secret",
            "--panel-webapi-url",
            "https://panel.example/api",
        ])
        .unwrap();
        assert_eq!(incomplete.panel_sync_config().unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    }
}
