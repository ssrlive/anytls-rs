use anytls::{
    auth::read_auth,
    padding::{DEFAULT_SCHEME, PaddingFactory},
    session::{BoxTransport, Session, Stream},
    stream_io::StreamIo,
};
use clap::Parser;
use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
};
use socks5_impl::protocol::{Address, AsyncStreamOperation};
use std::{
    net::{SocketAddr, ToSocketAddrs},
    sync::Arc,
};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Duration;
use tokio_rustls::TlsAcceptor;

#[derive(Parser, Debug)]
struct Args {
    #[arg(short = 'l', long, default_value = "0.0.0.0:8443")]
    listen: SocketAddr,
    #[arg(short = 'p', long)]
    password: String,
    #[arg(long, default_value_t = 1024)]
    max_streams_per_session: usize,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let listener = TcpListener::bind(args.listen).await?;
    log::info!("AnyTLS server listening on {}", args.listen);
    let acceptor = TlsAcceptor::from(Arc::new(tls_config()?));
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
            log::info!("accepted TLS connection {session_id} from {peer}");
            if let Err(error) = handle_connection(tcp, acceptor, padding, password, session_id, max_streams).await {
                log::warn!("connection {session_id} from {peer} failed: {error}");
            } else {
                log::info!("connection {session_id} from {peer} closed");
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
    log::info!("connection {session_id}: TLS and AnyTLS authentication completed");

    let session = Session::new_server(session_id, Box::new(tls) as BoxTransport, padding, max_streams);
    session.run().await?;
    loop {
        let stream = match session.accept_stream().await {
            Ok(stream) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                log::info!("connection {session_id}: session closed by peer");
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let session_id = session_id;
        tokio::spawn(async move {
            let stream_id = stream.id();
            if let Err(error) = relay_stream(stream).await {
                log::warn!("session {session_id} stream {stream_id} relay failed: {error}");
            }
        });
    }
}

async fn relay_stream(stream: Stream) -> std::io::Result<()> {
    let stream_id = stream.id();
    let started = std::time::Instant::now();
    let mut stream_io = StreamIo::new(stream);
    let destination = read_target(&mut stream_io).await?;
    log::info!("stream {stream_id}: connecting to {destination:?}");
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
    let (stream_to_target, target_to_stream) = tokio::io::copy_bidirectional(&mut stream_io, &mut outbound).await?;
    stream_io.shutdown().await?;
    outbound.shutdown().await?;
    log::info!(
        "stream {stream_id}: relay to {destination:?} closed: stream_to_target={stream_to_target} bytes, target_to_stream={target_to_stream} bytes, elapsed={:?}",
        started.elapsed()
    );
    Ok(())
}

async fn read_target(stream: &mut StreamIo) -> std::io::Result<Address> {
    Address::retrieve_from_async_stream(stream).await
}

fn tls_config() -> std::io::Result<ServerConfig> {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).map_err(std::io::Error::other)?;
    let cert = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()));
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(std::io::Error::other)
}
