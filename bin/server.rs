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
use tokio::net::{TcpListener, TcpStream};
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
    let args = Args::parse();
    let listener = TcpListener::bind(args.listen).await?;
    let acceptor = TlsAcceptor::from(Arc::new(tls_config()?));
    let padding = Arc::new(tokio::sync::RwLock::new(
        PaddingFactory::new(DEFAULT_SCHEME).expect("default padding"),
    ));
    let password = Arc::new(args.password);
    let mut session_id = 0usize;
    loop {
        let (tcp, peer) = listener.accept().await?;
        session_id = session_id.wrapping_add(1);
        let acceptor = acceptor.clone();
        let padding = padding.clone();
        let password = password.clone();
        let max_streams = args.max_streams_per_session;
        tokio::spawn(async move {
            if let Err(error) = handle_connection(tcp, acceptor, padding, password, session_id, max_streams).await {
                log::debug!("connection {peer} failed: {error}");
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

    let session = Session::new_server(session_id, Box::new(tls) as BoxTransport, padding, max_streams);
    session.run().await?;
    loop {
        let stream = match session.accept_stream().await {
            Ok(stream) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
            Err(error) => return Err(error),
        };
        tokio::spawn(async move {
            if let Err(error) = relay_stream(stream).await {
                log::debug!("stream relay failed: {error}");
            }
        });
    }
}

async fn relay_stream(stream: Stream) -> std::io::Result<()> {
    let mut stream_io = StreamIo::new(stream);
    let destination = read_target(&mut stream_io).await?;
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
    tokio::io::copy_bidirectional(&mut stream_io, &mut outbound).await?;
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
