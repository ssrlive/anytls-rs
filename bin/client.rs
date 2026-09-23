use anytls::{
    auth::write_auth,
    client::{Client, Dialer},
    padding::{DEFAULT_SCHEME, PaddingFactory},
    session::BoxTransport,
    stream_io::StreamIo,
    uot::{UotMode, UotRequest, uot_encode_packet, uot_get_packet_from_stream, uot_sentinel_destination},
};
use clap::Parser;
use rustls::{
    ClientConfig,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use socks5_impl::{
    protocol::{Address, AsyncStreamOperation, Reply},
    server::{AssociatedUdpSocket, ClientConnection, Server, UdpAssociate, auth::NoAuth, connection::associate},
};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio_rustls::TlsConnector;

#[derive(Parser, Debug)]
struct Args {
    #[arg(short = 'l', long, default_value = "127.0.0.1:1080")]
    listen: SocketAddr,
    #[arg(short = 's', long)]
    server: SocketAddr,
    #[arg(short = 'p', long)]
    password: String,
    #[arg(long, default_value = "localhost")]
    sni: String,
    #[arg(long, default_value_t = 16)]
    max_streams_per_session: usize,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let listener = Server::bind(args.listen, Arc::new(NoAuth)).await?;
    log::info!("SOCKS5 listener started on {}; AnyTLS server {}", args.listen, args.server);
    let padding = Arc::new(tokio::sync::RwLock::new(
        PaddingFactory::new(DEFAULT_SCHEME).expect("default padding"),
    ));
    let password = Arc::new(args.password);
    let sni = Arc::new(args.sni);
    let server = args.server;
    let dialer_padding = Arc::clone(&padding);
    let dialer: Dialer = Arc::new(move || {
        let padding = Arc::clone(&dialer_padding);
        let password = password.clone();
        let sni = sni.clone();
        Box::pin(async move { dial(server, &sni, &password, padding).await })
    });
    let client = Client::new(
        dialer,
        padding,
        std::time::Duration::from_secs(30),
        args.max_streams_per_session,
        std::time::Duration::from_secs(3600),
    );

    loop {
        let (stream, _) = listener.accept().await?;
        let client = client.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_socks5(stream, client).await {
                log::warn!("SOCKS5 connection failed: {error}");
            }
        });
    }
}

async fn dial(
    server: SocketAddr,
    sni: &str,
    password: &str,
    padding: Arc<tokio::sync::RwLock<PaddingFactory>>,
) -> std::io::Result<BoxTransport> {
    let tcp = TcpStream::connect(server).await?;
    log::info!("connecting to AnyTLS server {server}");
    let name = ServerName::try_from(sni.to_owned()).map_err(std::io::Error::other)?;
    let connector = TlsConnector::from(tls_config());
    let mut tls = connector.connect(name, tcp).await?;
    log::info!("TLS connection to AnyTLS server {server} established");
    let padding = padding.read().await;
    write_auth(&mut tls, password, &padding).await?;
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

async fn write_stream_all(stream: &anytls::session::Stream, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let written = stream.write(bytes).await?;
        if written == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "failed to write UOT stream"));
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

fn tls_config() -> Arc<ClientConfig> {
    let mut config = ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    config.dangerous().set_certificate_verifier(Arc::new(AnyCertificate));
    Arc::new(config)
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
