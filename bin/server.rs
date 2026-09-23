use anytls::{
    auth::read_auth,
    padding::{DEFAULT_SCHEME, PaddingFactory},
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
    sync::Arc,
};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::time::Duration;
use tokio_rustls::TlsAcceptor;

#[derive(Parser, serde::Serialize, serde::Deserialize, Debug, Clone)]
#[command(version, author, name = "anytls-server", about = "AnyTLS rust server")]
struct Args {
    /// Server listen port
    #[arg(short = 'l', long, value_name = "IP:PORT", default_value = "0.0.0.0:8443")]
    listen: SocketAddr,

    /// Password for anytls server authentication
    #[arg(short = 'p', long)]
    password: String,

    /// Maximum logical streams accepted on one authenticated TLS session
    #[arg(long, value_name = "N", default_value_t = 1024)]
    max_streams_per_session: usize,

    /// TLS certificate PEM file (optional)
    #[arg(long, value_name = "FILE")]
    cert: Option<PathBuf>,

    /// TLS private key PEM file (optional)
    #[arg(long, value_name = "FILE")]
    key: Option<PathBuf>,

    /// Log level (off, error, warn, info, debug, trace)
    #[arg(long, value_name = "LOG", default_value = "info")]
    log: log::LevelFilter,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let log_level = args.log.to_string().to_ascii_lowercase();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level)).init();
    let acceptor = TlsAcceptor::from(Arc::new(tls_config(args.cert.as_deref(), args.key.as_deref())?));
    let listener = TcpListener::bind(args.listen).await?;
    log::info!("AnyTLS server listening on {}", args.listen);
    let padding = Arc::new(tokio::sync::RwLock::new(
        PaddingFactory::new(DEFAULT_SCHEME).expect("default padding"),
    ));
    let password = Arc::new(args.password);
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
        tokio::spawn(async move {
            log::info!("accepted TLS session {session_id} from {peer}");
            if let Err(error) = handle_connection(tcp, acceptor, padding, password, session_id, max_streams).await {
                log::warn!("session {session_id} from {peer} failed: {error}");
            } else {
                log::info!("session {session_id} from {peer} closed");
            }
        });
    }
}

async fn handle_connection(
    tcp: TcpStream,
    acceptor: TlsAcceptor,
    padding: Arc<tokio::sync::RwLock<PaddingFactory>>,
    password: Arc<String>,
    session_id: usize,
    max_streams: usize,
) -> std::io::Result<()> {
    let mut tls = acceptor.accept(tcp).await?;
    read_auth(&mut tls, &password).await?;
    log::info!("session {session_id}: TLS and AnyTLS authentication completed");

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
        tokio::spawn(async move {
            let stream_id = stream.id();
            if let Err(error) = relay_stream(stream).await {
                if is_peer_disconnect(&error) {
                    log::debug!("session {session_id} stream {stream_id} peer disconnected: {error}");
                } else {
                    log::warn!("session {session_id} stream {stream_id} relay failed: {error}");
                }
            }
        });
    }
}

async fn relay_stream(stream: Stream) -> std::io::Result<()> {
    let session_id = stream.session_id().unwrap_or_default();
    let stream_id = stream.id();
    let started = std::time::Instant::now();
    let mut stream_io = StreamIo::new(stream);
    let destination = read_target(&mut stream_io).await?;
    if uot_is_sentinel_destination(&destination) {
        return relay_uot_datagrams(stream_io).await;
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
    let mut outbound = outbound;
    let relay_result = tokio::io::copy_bidirectional(&mut stream_io, &mut outbound).await;
    let _ = stream_io.shutdown().await;
    let _ = outbound.shutdown().await;
    match relay_result {
        Ok((stream_to_target, target_to_stream)) => {
            log::info!(
                "session {session_id} stream {stream_id}: relay to {destination} closed: stream_to_target={stream_to_target} bytes, target_to_stream={target_to_stream} bytes, elapsed={:?}",
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

async fn relay_uot_datagrams(mut stream_io: StreamIo) -> std::io::Result<()> {
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
            }
            packet = socket.recv_from(&mut outbound_buf) => {
                let (size, source) = packet?;
                let source = Address::from(source);
                let frame = match request.mode {
                    UotMode::Datagram => uot_encode_packet(UotMode::Datagram, Some(&source), &outbound_buf[..size])?,
                    UotMode::Connected => uot_encode_packet(UotMode::Connected, None, &outbound_buf[..size])?,
                };
                write_stream_all(&stream, &frame).await?;
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
    use super::tls_config;
    use std::path::Path;

    #[test]
    fn requires_certificate_and_key_together() {
        let cert_only = tls_config(Some(Path::new("certificate.pem")), None).unwrap_err();
        assert_eq!(cert_only.kind(), std::io::ErrorKind::InvalidInput);

        let key_only = tls_config(None, Some(Path::new("private-key.pem"))).unwrap_err();
        assert_eq!(key_only.kind(), std::io::ErrorKind::InvalidInput);
    }
}
