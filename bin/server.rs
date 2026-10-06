use anytls::{
    AUTH_HEADER_SIZE, BoxTransport, DEFAULT_MAX_SESSION_AGE, DEFAULT_SCHEME, PASSWORD_DIGEST_SIZE, PaddingFactory, PanelSyncClient,
    ServerArgs, Session, Stream, StreamIo, TrafficAudit, TrafficAuditPtr, UotMode, extract_client_id_from_padding, is_peer_disconnect,
    password_digest, print_args, print_url, relay, uot_encode_packet, uot_get_packet_from_stream, uot_get_request_from_stream,
    uot_is_sentinel_destination,
};
use clap::Parser;
use method_name::method_name_unstable;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
};
use socks5_impl::protocol::{Address, AsyncStreamOperation};
use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Duration;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::client::TlsConnector;
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;
use x509_parser::extensions::{GeneralName, ParsedExtension};

#[derive(Clone)]
struct TlsState {
    acceptor: TlsAcceptor,
    probe_sni_allowlist: Arc<Vec<String>>,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cancel_token = CancellationToken::new();
    let cancel_token_clone = cancel_token.clone();
    let ctrlc = ctrlc2::AsyncCtrlC::new(move || {
        cancel_token_clone.cancel();
        true
    })?;

    let server = run_server(cancel_token);
    tokio::pin!(server);
    let res = tokio::select! {
        result = &mut server => result,
        result = ctrlc => {
            result?;
            server.await
        }
    };
    log::info!("{} -- Server exited with result: {:?}", method_name_unstable!(), res);
    res
}

async fn run_server(cancel_token: CancellationToken) -> std::io::Result<()> {
    let args = ServerArgs::parse();
    let log_level = args.log.to_string().to_ascii_lowercase();
    if let Err(e) = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level)).try_init() {
        eprintln!("Failed to initialize logger: {e}");
    }
    if args.print_args {
        println!("\n{}\n", print_args(&args, args.listen.port()).await?);
        return Ok(());
    }
    if args.print_url {
        let port = args.listen.port();
        let password = args.password.as_deref().unwrap_or_default();
        let enable = args.panel_sync_enabled();
        println!("{}", print_url(port, password, args.sni.as_deref(), enable).await?);
        return Ok(());
    }

    let forward_target = validate_forward_url(args.forward.clone())?;
    let (tls_file_watcher, tls_events, cert_path, key_path) = match (&args.cert, &args.key) {
        (Some(cert_path), Some(key_path)) => {
            let (watcher, events, cert_path, key_path) = watch_tls_files(cert_path, key_path)?;
            (Some(watcher), Some(events), Some(cert_path), Some(key_path))
        }
        _ => (None, None, None, None),
    };
    let (server_tls_config, probe_sni_allowlist) = tls_config(args.sni.as_deref(), args.cert.as_deref(), args.key.as_deref())?;
    let tls_state = TlsState {
        acceptor: TlsAcceptor::from(Arc::new(server_tls_config)),
        probe_sni_allowlist: Arc::new(probe_sni_allowlist),
    };
    let (tls_state_tx, tls_state_rx) = watch::channel(tls_state);
    let panel_config = args.panel_sync_config()?;
    let panel_sync_enabled = panel_config.is_some();
    let traffic_audit: TrafficAuditPtr = Arc::new(tokio::sync::Mutex::new(TrafficAudit::new()));
    let mut panel_task: Option<JoinHandle<()>> = None;
    if let Some(config) = panel_config {
        let mut panel_client = PanelSyncClient::new(config);
        panel_client.sync_initial(&traffic_audit).await?;
        let audit = Arc::clone(&traffic_audit);
        let panel_cancel_token = cancel_token.clone();
        panel_task = Some(tokio::spawn(async move { panel_client.run(audit, panel_cancel_token).await }));
    }
    let padding_factory = if let Some(path) = &args.padding_scheme {
        let content = tokio::fs::read(path).await?;
        use std::io::{Error, ErrorKind::InvalidInput};
        PaddingFactory::new(&content)
            .ok_or_else(|| Error::new(InvalidInput, format!("Wrong format padding scheme file: {}", path.display())))?
    } else {
        PaddingFactory::new(DEFAULT_SCHEME).expect("default scheme is valid")
    };
    let listener = TcpListener::bind(args.listen).await?;
    let _tls_file_watcher = tls_file_watcher;
    let mut tls_reload_task = match (tls_events, cert_path, key_path) {
        (Some(events), Some(cert_path), Some(key_path)) => Some(tokio::spawn(reload_tls_on_events(
            events,
            tls_state_tx,
            cert_path,
            key_path,
            args.sni.clone(),
            cancel_token.clone(),
        ))),
        _ => None,
    };
    log::info!("{} -- AnyTLS server listening on {}", method_name_unstable!(), args.listen);
    let padding = Arc::new(tokio::sync::RwLock::new(padding_factory));
    let password = Arc::new(args.password.clone().unwrap_or_default());
    let mut session_id = 0usize;
    let mut last_emfile_warning = tokio::time::Instant::now() - Duration::from_secs(5);
    let mut connection_tasks = JoinSet::new();
    let accept_result = loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break Ok(()),
            result = listener.accept() => {
                let mn = method_name_unstable!();
                let (tcp, peer) = match result {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock || error.raw_os_error() == Some(24) => {
                        if last_emfile_warning.elapsed() >= Duration::from_secs(5) {
                            log::warn!("{mn} -- accept temporarily failed due to exhausted descriptors: {error}");
                            last_emfile_warning = tokio::time::Instant::now();
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                    Err(error) => break Err(error),
                };
                session_id = session_id.wrapping_add(1);
                let tls_state = tls_state_rx.borrow().clone();
                let padding = padding.clone();
                let password = password.clone();
                let max_streams = args.max_streams_per_session;
                let traffic_audit = Arc::clone(&traffic_audit);
                let forward_target = forward_target.clone();
                let cancel_token = cancel_token.clone();
                connection_tasks.spawn(async move {
                    log::trace!("{mn} -- accepted TLS session {session_id} from {peer}");
                    if let Err(error) = handle_connection(
                        tcp,
                        tls_state,
                        padding,
                        password,
                        session_id,
                        max_streams,
                        traffic_audit,
                        panel_sync_enabled,
                        forward_target,
                        cancel_token,
                    )
                    .await
                    {
                        log::warn!("{mn} -- session {session_id} from {peer} failed: {error}");
                    } else {
                        log::trace!("{mn} -- session {session_id} from {peer} closed");
                    }
                });
            }
            Some(result) = connection_tasks.join_next(), if !connection_tasks.is_empty() => {
                if let Err(error) = result {
                    log::warn!("{} -- server connection task failed: {error}", method_name_unstable!());
                }
            }
        }
    };

    cancel_token.cancel();
    if let Some(mut tls_reload_task) = tls_reload_task.take()
        && tokio::time::timeout(Duration::from_secs(5), &mut tls_reload_task).await.is_err()
    {
        tls_reload_task.abort();
        let _ = tls_reload_task.await;
    }
    if let Some(mut panel_task) = panel_task
        && tokio::time::timeout(Duration::from_secs(5), &mut panel_task).await.is_err()
    {
        panel_task.abort();
        let _ = panel_task.await;
    }
    let drain = async {
        while let Some(result) = connection_tasks.join_next().await {
            if let Err(error) = result {
                let mn = method_name_unstable!();
                log::warn!("{mn} -- server connection task failed during shutdown: {error}");
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(10), drain).await.is_err() {
        connection_tasks.abort_all();
        while connection_tasks.join_next().await.is_some() {}
    }
    accept_result
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    tcp: TcpStream,
    tls_state: TlsState,
    padding: Arc<tokio::sync::RwLock<PaddingFactory>>,
    password: Arc<String>,
    session_id: usize,
    max_streams: usize,
    traffic_audit: TrafficAuditPtr,
    panel_sync_enabled: bool,
    forward_target: Option<Url>,
    cancel_token: CancellationToken,
) -> std::io::Result<()> {
    let mut tls = tokio::select! {
        _ = cancel_token.cancelled() => return Ok(()),
        result = tls_state.acceptor.accept(tcp) => result?,
    };
    let client_addr = tls.get_ref().0.peer_addr()?;
    let probe_target = tls.get_ref().1.server_name().map(str::to_owned);
    let mut auth_data = [0u8; AUTH_HEADER_SIZE];
    let mut auth_bytes_read = 0;
    while auth_bytes_read < auth_data.len() {
        let bytes_read = tokio::select! {
            _ = cancel_token.cancelled() => return Ok(()),
            result = tls.read(&mut auth_data[auth_bytes_read..]) => result?,
        };
        if bytes_read == 0 {
            break;
        }
        auth_bytes_read += bytes_read;
    }

    let mn = method_name_unstable!();
    let authenticated = auth_bytes_read == AUTH_HEADER_SIZE && auth_data[..PASSWORD_DIGEST_SIZE] == password_digest(&password);
    if !authenticated {
        let prefix = auth_data[..auth_bytes_read].to_vec();
        if let Some(target_url) = forward_target.as_ref() {
            let relay_result = tokio::select! {
                _ = cancel_token.cancelled() => return Ok(()),
                result = relay_forward_stream(client_addr, target_url, tls, prefix) => result,
            };
            if let Err(error) = relay_result {
                log::debug!("{mn} -- forward relay failed for {client_addr}: {error}");
            }
        } else if let Some(target_host) = probe_target.filter(|target| sni_is_allowed(target, &tls_state.probe_sni_allowlist)) {
            let relay_result = tokio::select! {
                _ = cancel_token.cancelled() => return Ok(()),
                result = relay_probe_stream(client_addr, target_host, tls, prefix) => result,
            };
            if let Err(error) = relay_result {
                log::debug!("{mn} -- SNI probe relay failed for {client_addr}: {error}");
            }
        } else {
            log::debug!("{mn} -- authentication failed for {client_addr}; no forward target or matching probe SNI");
        }
        return Ok(());
    }

    let padding_len = u16::from_be_bytes([auth_data[32], auth_data[33]]) as usize;
    let mut padding_data = vec![0; padding_len];
    tokio::select! {
        _ = cancel_token.cancelled() => return Ok(()),
        result = async { tls.read_exact(&mut padding_data).await.map(|_| ()) } => result?,
    }
    let client_id = extract_client_id_from_padding(&padding_data);
    if panel_sync_enabled {
        let approved = match client_id {
            Some(client_id) => traffic_audit.lock().await.is_enabled(&client_id),
            None => false,
        };
        if !approved {
            log::debug!("{mn} -- session {session_id}: denied panel-managed client {client_id:?}");
            return Ok(());
        }
    }
    log::debug!("{mn} -- session {session_id}: TLS and AnyTLS authentication completed for {client_id:?}");

    let session = Session::new_server(
        session_id,
        Box::new(tls) as BoxTransport,
        padding,
        max_streams,
        DEFAULT_MAX_SESSION_AGE,
    );
    session.run().await?;
    let mut stream_tasks = JoinSet::new();
    let session_result = loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break Ok(()),
            result = session.accept_stream() => {
                let stream = match result {
                    Ok(stream) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                        log::info!("{mn} -- session {session_id} closed by peer");
                        break Ok(());
                    }
                    Err(error) => break Err(error),
                };
                let traffic_audit = Arc::clone(&traffic_audit);
                stream_tasks.spawn(async move {
                    let stream_id = stream.id();
                    if let Err(error) = relay_stream(stream, traffic_audit, client_id, panel_sync_enabled).await {
                        let mn = method_name_unstable!();
                        if is_peer_disconnect(&error) {
                            log::debug!("{mn} -- session {session_id} stream {stream_id} peer disconnected: {error}");
                        } else {
                            log::warn!("{mn} -- session {session_id} stream {stream_id} relay failed: {error}");
                        }
                    }
                });
            }
            Some(result) = stream_tasks.join_next(), if !stream_tasks.is_empty() => {
                if let Err(error) = result {
                    log::warn!("{mn} -- session {session_id} stream task failed: {error}");
                }
            }
        }
    };

    let shutdown_result = session.shutdown().await;
    let drain = async {
        while let Some(result) = stream_tasks.join_next().await {
            if let Err(error) = result {
                log::warn!("{mn} -- session {session_id} stream task failed during shutdown: {error}");
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(5), drain).await.is_err() {
        stream_tasks.abort_all();
        while stream_tasks.join_next().await.is_some() {}
    }
    session_result?;
    shutdown_result
}

async fn relay_stream(
    stream: Stream,
    traffic_audit: TrafficAuditPtr,
    client_id: Option<Uuid>,
    panel_sync_enabled: bool,
) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind::PermissionDenied, ErrorKind::TimedOut};
    let session_id = stream.session_id();
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
            let message = format!("panel-managed client {client_id:?} is disabled");
            stream_io.handshake_failure(&message).await?;
            return Err(Error::new(PermissionDenied, message));
        }
    }
    if uot_is_sentinel_destination(&destination) {
        return relay_uot_datagrams(stream_io, traffic_audit, client_id).await;
    }
    let mn = method_name_unstable!();
    log::debug!("{mn} -- session {session_id} stream {stream_id}: connecting to {destination}");
    let connected = tokio::time::timeout(Duration::from_secs(15), async {
        let addresses = tokio::net::lookup_host(destination.to_string()).await?.collect::<Vec<_>>();
        TcpStream::connect(&addresses[..]).await
    })
    .await
    .unwrap_or_else(|_| Err(Error::new(TimedOut, format!("proxy target connection to {destination} timed out"))));
    let outbound = match connected {
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
    let relay_result = relay::copy_bidirectional(&mut outbound, &mut stream_io).await;
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
            let elapsed = started.elapsed().as_secs();
            log::trace!(
                "{mn} -- session {session_id} stream {stream_id}: relay to {destination} closed: up={upstream}, down={downstream}, elapsed={elapsed}s",
            );
            Ok(())
        }
        Err(error) if error.is_peer_disconnect() => {
            log::debug!("{mn} -- session {session_id} stream {stream_id}: peer disconnected from {destination}: {error}");
            Ok(())
        }
        Err(error) => Err(error.into()),
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
                    Err(error) if is_peer_disconnect(&error) => break Ok(()),
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

fn watch_tls_files(
    cert_path: &Path,
    key_path: &Path,
) -> std::io::Result<(RecommendedWatcher, mpsc::UnboundedReceiver<notify::Result<Event>>, PathBuf, PathBuf)> {
    let absolute_path = |path: &Path| -> std::io::Result<PathBuf> {
        if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            Ok(std::env::current_dir()?.join(path))
        }
    };
    let normalize_path = |path: &Path| -> std::io::Result<PathBuf> {
        let path = absolute_path(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "TLS file path has no parent"))?;
        let file_name = path
            .file_name()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "TLS file path has no name"))?;
        Ok(parent.canonicalize()?.join(file_name))
    };
    let cert_path = normalize_path(cert_path)?;
    let key_path = normalize_path(key_path)?;
    let (event_sender, events) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = event_sender.send(event);
    })
    .map_err(std::io::Error::other)?;

    let directories: HashSet<PathBuf> = [&cert_path, &key_path]
        .into_iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect();
    for directory in directories {
        watcher
            .watch(&directory, RecursiveMode::NonRecursive)
            .map_err(std::io::Error::other)?;
    }

    Ok((watcher, events, cert_path, key_path))
}

async fn reload_tls_on_events(
    mut events: mpsc::UnboundedReceiver<notify::Result<Event>>,
    tls_state: watch::Sender<TlsState>,
    cert_path: PathBuf,
    key_path: PathBuf,
    sni: Option<String>,
    cancel_token: CancellationToken,
) {
    const DEBOUNCE: Duration = Duration::from_millis(250);
    let mn = method_name_unstable!();
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => return,
            event = events.recv() => match event {
                Some(Ok(event)) if !event_affects_tls_files(&event, &cert_path, &key_path) => continue,
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    log::warn!("{mn} -- TLS file watcher error: {error}");
                    continue;
                }
                None => return,
            },
        };
        let mut debounce = Box::pin(tokio::time::sleep(DEBOUNCE));
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => return,
                _ = &mut debounce => break,
                event = events.recv() => match event {
                    Some(Ok(event)) if event_affects_tls_files(&event, &cert_path, &key_path) => {
                        debounce.as_mut().reset(tokio::time::Instant::now() + DEBOUNCE);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => log::warn!("{mn} -- TLS file watcher error: {error}"),
                    None => return,
                },
            }
        }

        let cert_path_for_load = cert_path.clone();
        let key_path_for_load = key_path.clone();
        let sni_for_load = sni.clone();
        let load =
            tokio::task::spawn_blocking(move || tls_config(sni_for_load.as_deref(), Some(&cert_path_for_load), Some(&key_path_for_load)));
        let result = tokio::select! {
            _ = cancel_token.cancelled() => return,
            result = load => result,
        };
        match result {
            Ok(Ok((config, probe_sni_allowlist))) => {
                tls_state.send_replace(TlsState {
                    acceptor: TlsAcceptor::from(Arc::new(config)),
                    probe_sni_allowlist: Arc::new(probe_sni_allowlist),
                });
                log::info!("{mn} -- reloaded TLS certificate and private key");
            }
            Ok(Err(error)) => {
                log::warn!("{mn} -- failed to reload TLS certificate and private key; keeping the current configuration: {error}",);
            }
            Err(error) => log::error!("{mn} -- TLS reload task failed: {error}"),
        }
    }
}

fn event_affects_tls_files(event: &Event, cert_path: &Path, key_path: &Path) -> bool {
    !event.kind.is_access() && event.paths.iter().any(|path| path == cert_path || path == key_path)
}

fn tls_config(sni: Option<&str>, cert_path: Option<&Path>, key_path: Option<&Path>) -> std::io::Result<(ServerConfig, Vec<String>)> {
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

        let probe_sni_allowlist = extract_dns_names_from_certs(&certs, sni);
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(std::io::Error::other)?;
        return Ok((config, probe_sni_allowlist));
    }

    if cert_path.is_some() || key_path.is_some() {
        return Err(Error::new(InvalidInput, "both --cert and --key must be provided"));
    }

    let sni_name = sni.unwrap_or("localhost");
    let certified = rcgen::generate_simple_self_signed(vec![sni_name.to_owned()]).map_err(Error::other)?;
    let cert = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(std::io::Error::other)?;
    let probe_sni_allowlist = sni.map_or_else(Vec::new, |name| vec![name.to_owned()]);
    Ok((config, probe_sni_allowlist))
}

fn validate_forward_url(forward: Option<Url>) -> std::io::Result<Option<Url>> {
    use std::io::{Error, ErrorKind::InvalidInput};
    match forward {
        Some(url) if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() => Ok(Some(url)),
        Some(url) => Err(Error::new(
            InvalidInput,
            format!("forward URL must be an http:// or https:// URL with a host: {url}"),
        )),
        None => Ok(None),
    }
}

fn extract_dns_names_from_certs(certs: &[CertificateDer<'static>], fallback_sni: Option<&str>) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(cert) = certs.first()
        && let Ok((_, parsed_cert)) = x509_parser::parse_x509_certificate(cert.as_ref())
    {
        for extension in parsed_cert.extensions() {
            if let ParsedExtension::SubjectAlternativeName(san) = extension.parsed_extension() {
                for name in &san.general_names {
                    if let GeneralName::DNSName(name) = name {
                        names.push(name.to_string());
                    }
                }
            }
        }
    }
    if names.is_empty()
        && let Some(sni) = fallback_sni
    {
        names.push(sni.to_owned());
    }
    names.sort_unstable();
    names.dedup();
    names
}

fn sni_is_allowed(target: &str, allowlist: &[String]) -> bool {
    allowlist.iter().any(|allowed| allowed.eq_ignore_ascii_case(target))
}

fn create_probe_target_tls_config() -> std::io::Result<Arc<ClientConfig>> {
    let mut root_store = RootCertStore::empty();
    let cert_result = rustls_native_certs::load_native_certs();
    if !cert_result.errors.is_empty() {
        let mn = method_name_unstable!();
        log::warn!("{mn} -- failed to load some system root certificates: {:?}", cert_result.errors);
    }
    for cert in cert_result.certs {
        root_store.add(cert).map_err(std::io::Error::other)?;
    }
    Ok(Arc::new(
        ClientConfig::builder().with_root_certificates(root_store).with_no_client_auth(),
    ))
}

async fn relay_probe_stream<S>(client_addr: SocketAddr, target_host: String, mut inbound: S, prefix: Vec<u8>) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let tcp = TcpStream::connect((target_host.as_str(), 443)).await?;
    let connector = TlsConnector::from(create_probe_target_tls_config()?);
    let server_name = target_host.clone().try_into().map_err(std::io::Error::other)?;
    let mut outbound = connector.connect(server_name, tcp).await?;
    let mn = method_name_unstable!();
    log::trace!("{mn} -- SNI probe relay for {client_addr} to {target_host}:443",);
    outbound.write_all(&prefix).await?;
    outbound.flush().await?;
    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await?;
    Ok(())
}

enum ForwardOutbound {
    Http(TcpStream),
    Https(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for ForwardOutbound {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Http(stream) => Pin::new(stream).poll_read(cx, buffer),
            Self::Https(stream) => Pin::new(&mut **stream).poll_read(cx, buffer),
        }
    }
}

impl AsyncWrite for ForwardOutbound {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buffer: &[u8]) -> Poll<std::io::Result<usize>> {
        match &mut *self {
            Self::Http(stream) => Pin::new(stream).poll_write(cx, buffer),
            Self::Https(stream) => Pin::new(&mut **stream).poll_write(cx, buffer),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Http(stream) => Pin::new(stream).poll_flush(cx),
            Self::Https(stream) => Pin::new(&mut **stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Http(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Https(stream) => Pin::new(&mut **stream).poll_shutdown(cx),
        }
    }
}

async fn relay_forward_stream<S>(client_addr: SocketAddr, target: &Url, mut inbound: S, prefix: Vec<u8>) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use std::io::{Error, ErrorKind::InvalidInput};
    let host = target
        .host_str()
        .ok_or(Error::new(InvalidInput, "forward URL must include a host"))?;
    let port = target
        .port_or_known_default()
        .ok_or(Error::new(InvalidInput, "forward URL must include a port"))?;
    let tcp = TcpStream::connect((host, port)).await?;
    let mut outbound = match target.scheme() {
        "http" => ForwardOutbound::Http(tcp),
        "https" => {
            let connector = TlsConnector::from(create_probe_target_tls_config()?);
            let server_name = host.to_owned().try_into().map_err(std::io::Error::other)?;
            ForwardOutbound::Https(Box::new(connector.connect(server_name, tcp).await?))
        }
        scheme => {
            return Err(Error::new(InvalidInput, format!("unsupported forward URL scheme: {scheme}")));
        }
    };
    let mn = method_name_unstable!();
    log::trace!("{mn} -- forward relay for {client_addr} to {host}:{port} ({})", target.scheme());
    outbound.write_all(&prefix).await?;
    outbound.flush().await?;
    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await?;
    Ok(())
}

#[cfg(test)]
mod tls_config_tests {
    use super::{
        TlsState, event_affects_tls_files, relay_forward_stream, reload_tls_on_events, tls_config, validate_forward_url, watch_tls_files,
    };
    use notify::{Event, EventKind, event::AccessKind, event::ModifyKind};
    use std::{path::Path, sync::Arc};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;
    use url::Url;

    #[test]
    fn tls_reload_filters_unrelated_and_access_events() {
        let cert_path = Path::new("/tmp/certificate.pem");
        let key_path = Path::new("/tmp/private-key.pem");

        let mut sibling = Event::new(EventKind::Modify(ModifyKind::Any));
        sibling.paths.push(Path::new("/tmp/certificate.pem.next").to_path_buf());
        assert!(!event_affects_tls_files(&sibling, cert_path, key_path));

        let mut certificate_change = Event::new(EventKind::Modify(ModifyKind::Any));
        certificate_change.paths.push(cert_path.to_path_buf());
        assert!(event_affects_tls_files(&certificate_change, cert_path, key_path));

        let mut access = Event::new(EventKind::Access(AccessKind::Any));
        access.paths.push(cert_path.to_path_buf());
        assert!(!event_affects_tls_files(&access, cert_path, key_path));
    }

    #[test]
    fn requires_certificate_and_key_together() {
        let cert_only = tls_config(None, Some(Path::new("certificate.pem")), None).unwrap_err();
        assert_eq!(cert_only.kind(), std::io::ErrorKind::InvalidInput);

        let key_only = tls_config(None, None, Some(Path::new("private-key.pem"))).unwrap_err();
        assert_eq!(key_only.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn generates_probe_certificate_for_configured_sni() {
        let (_, allowlist) = tls_config(Some("probe.example.com"), None, None).unwrap();
        assert_eq!(allowlist, ["probe.example.com"]);
    }

    #[test]
    fn forward_url_accepts_only_http_and_https() {
        for value in ["http://example.com", "https://example.com:8443"] {
            assert!(validate_forward_url(Some(Url::parse(value).unwrap())).is_ok());
        }
        let error = validate_forward_url(Some(Url::parse("ftp://example.com").unwrap())).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn forward_relay_preserves_probe_prefix() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = listener.local_addr().unwrap();
        let target = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            stream.read_to_end(&mut request).await.unwrap();
            stream.write_all(b"probe response").await.unwrap();
            request
        });

        let (inbound, mut client) = tokio::io::duplex(1024);
        let target_url = Url::parse(&format!("http://{target_addr}/")).unwrap();
        let relay = tokio::spawn(async move { relay_forward_stream(target_addr, &target_url, inbound, b"GET /".to_vec()).await });
        client.write_all(b" HTTP/1.1\r\n\r\n").await.unwrap();
        client.shutdown().await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();

        assert_eq!(response, b"probe response");
        assert_eq!(target.await.unwrap(), b"GET / HTTP/1.1\r\n\r\n");
        relay.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn reloads_tls_state_after_certificate_and_key_change() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("anytls-tls-reload-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let cert_path = directory.join("certificate.pem");
        let key_path = directory.join("private-key.pem");
        let write_pair = |dns_name: &str, replace_atomically: bool| {
            let certified = rcgen::generate_simple_self_signed(vec![dns_name.to_owned()]).unwrap();
            let cert_output = if replace_atomically {
                directory.join("certificate.pem.next")
            } else {
                cert_path.clone()
            };
            let key_output = if replace_atomically {
                directory.join("private-key.pem.next")
            } else {
                key_path.clone()
            };
            std::fs::write(&cert_output, certified.cert.pem()).unwrap();
            std::fs::write(&key_output, certified.signing_key.serialize_pem()).unwrap();
            if replace_atomically {
                std::fs::rename(cert_output, &cert_path).unwrap();
                std::fs::rename(key_output, &key_path).unwrap();
            }
        };
        write_pair("initial.example.com", false);

        let (watcher, events, cert_path, key_path) = watch_tls_files(&cert_path, &key_path).unwrap();
        let (config, probe_sni_allowlist) = tls_config(None, Some(&cert_path), Some(&key_path)).unwrap();
        let (tls_state_tx, mut tls_state_rx) = watch::channel(TlsState {
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(config)),
            probe_sni_allowlist: Arc::new(probe_sni_allowlist),
        });
        let cancel_token = CancellationToken::new();
        let reload_task = tokio::spawn(reload_tls_on_events(
            events,
            tls_state_tx,
            cert_path,
            key_path,
            None,
            cancel_token.clone(),
        ));

        write_pair("updated.example.com", true);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                tls_state_rx.changed().await.unwrap();
                if tls_state_rx
                    .borrow()
                    .probe_sni_allowlist
                    .iter()
                    .any(|name| name == "updated.example.com")
                {
                    break;
                }
            }
        })
        .await
        .expect("updated certificate should be loaded");

        cancel_token.cancel();
        reload_task.await.unwrap();
        drop(watcher);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
