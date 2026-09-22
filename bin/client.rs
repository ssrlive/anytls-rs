use anytls::{
    auth::write_auth,
    client::{Client, Dialer},
    padding::{DEFAULT_SCHEME, PaddingFactory},
    session::BoxTransport,
    stream_io::StreamIo,
};
use clap::Parser;
use rustls::{
    ClientConfig,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use socks5_impl::{
    protocol::{Address, AsyncStreamOperation, Reply},
    server::{ClientConnection, Server, auth::NoAuth},
};
use std::{net::SocketAddr, sync::Arc};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
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
    #[arg(long, default_value_t = 128)]
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
    let dialer: Dialer = Arc::new(move || {
        let padding = padding.clone();
        let password = password.clone();
        let sni = sni.clone();
        Box::pin(async move { dial(server, &sni, &password, padding).await })
    });
    let client = Client::new(
        dialer,
        Arc::new(tokio::sync::RwLock::new(
            PaddingFactory::new(DEFAULT_SCHEME).expect("default padding"),
        )),
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
        ClientConnection::UdpAssociate(mut associate, _) => {
            let _ = associate.shutdown().await;
            return Err(std::io::Error::other("SOCKS5 UDP ASSOCIATE is unsupported"));
        }
    };
    let started = std::time::Instant::now();
    log::info!("opening SOCKS5 CONNECT to {target:?}");
    let stream = client.create_stream().await?;
    let mut remote = StreamIo::new(stream);
    target.write_to_async_stream(&mut remote).await?;
    let mut ready = connect.reply(Reply::Succeeded, Address::unspecified()).await?;
    let (client_to_proxy, proxy_to_client) = tokio::io::copy_bidirectional(&mut ready, &mut remote).await?;
    ready.shutdown().await?;
    remote.shutdown().await?;
    log::info!(
        "SOCKS5 relay to {target:?} closed: client_to_proxy={client_to_proxy} bytes, proxy_to_client={proxy_to_client} bytes, elapsed={:?}",
        started.elapsed()
    );
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
