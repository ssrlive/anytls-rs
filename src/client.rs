use method_name::method_name_unstable;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::{Mutex, RwLock, mpsc};

use crate::{PaddingFactory, Session, Stream, runtime::BoxTransport};

const MAX_IDLE_SESSIONS: usize = 4;
const SESSION_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub type Dialer = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = std::io::Result<BoxTransport>> + Send>> + Send + Sync>;

struct IdleSession {
    since: Instant,
    session: Arc<Session>,
}

impl IdleSession {
    fn new(session: Arc<Session>) -> Self {
        Self {
            since: Instant::now(),
            session,
        }
    }
}

pub struct Client {
    dialer: Dialer,
    padding: Arc<RwLock<PaddingFactory>>,
    idle_pool: Mutex<Vec<IdleSession>>,
    close_token: tokio_util::sync::CancellationToken,
    next_sequence: std::sync::atomic::AtomicUsize,
    max_session_age: Duration,
}

impl Client {
    pub fn new(dialer: Dialer, padding: Arc<RwLock<PaddingFactory>>, idle_timeout: Duration, max_session_age: Duration) -> Arc<Self> {
        let client = Arc::new(Self {
            dialer,
            padding,
            idle_pool: Mutex::new(Vec::new()),
            close_token: tokio_util::sync::CancellationToken::new(),
            next_sequence: std::sync::atomic::AtomicUsize::new(0),
            max_session_age,
        });
        let cleanup_client = Arc::downgrade(&client);
        tokio::spawn(async move {
            let interval = idle_timeout.clamp(Duration::from_secs(1), Duration::from_secs(5));
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                let Some(client) = cleanup_client.upgrade() else {
                    break;
                };
                client.cleanup(idle_timeout).await;
            }
        });
        client
    }

    pub async fn create_stream(self: &Arc<Self>) -> std::io::Result<Stream> {
        self.ensure_running()?;
        let (session, stream) = match self.reserve_pooled_stream().await? {
            Some(reserved) => reserved,
            None => {
                let session = tokio::select! {
                    _ = self.close_token.cancelled() => return Err(self.shutting_down_error()),
                    result = tokio::time::timeout(SESSION_CONNECT_TIMEOUT, self.create_session()) => {
                        result.map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "AnyTLS session connection timed out"))??
                    }
                };
                self.ensure_running()?;
                let stream = session.reserve_stream().await?;
                (session, stream)
            }
        };
        match session.open_reserved_stream(stream).await {
            Ok(stream) => Ok(stream),
            Err(error) => {
                let _ = session.shutdown().await;
                Err(error)
            }
        }
    }

    fn ensure_running(&self) -> std::io::Result<()> {
        if self.close_token.is_cancelled() {
            Err(self.shutting_down_error())
        } else {
            Ok(())
        }
    }

    fn shutting_down_error(&self) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "AnyTLS client is shutting down")
    }

    async fn reserve_pooled_stream(&self) -> std::io::Result<Option<(Arc<Session>, Stream)>> {
        self.ensure_running()?;
        let Some(session) = self.take_idle().await else {
            return Ok(None);
        };
        use std::io::ErrorKind::{BrokenPipe, TimedOut, WouldBlock};
        match session.reserve_stream().await {
            Ok(stream) => Ok(Some((session, stream))),
            Err(error) if matches!(error.kind(), WouldBlock | BrokenPipe | TimedOut) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn create_session(self: &Arc<Self>) -> std::io::Result<Arc<Session>> {
        let transport = (self.dialer)().await?;
        let sequence = self.next_sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let session = Session::new_client(sequence, transport, Arc::clone(&self.padding), self.max_session_age);
        let client_close = self.close_token.clone();
        let session_close = session.close_signal();
        let session_ref = Arc::downgrade(&session);
        tokio::spawn(async move {
            tokio::select! {
                _ = client_close.cancelled() => {
                    if let Some(session) = session_ref.upgrade() {
                        let _ = session.shutdown().await;
                    }
                }
                _ = session_close.cancelled() => {}
            }
        });
        let (sender, mut receiver) = mpsc::unbounded_channel();
        session.set_idle_sender(sender).await;
        let client = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(session) = receiver.recv().await {
                let Some(client) = client.upgrade() else {
                    break;
                };
                if client.close_token.is_cancelled() {
                    let _ = session.shutdown().await;
                    break;
                }
                if !session.is_closed() && session.is_idle().await {
                    let mn = method_name_unstable!();
                    let session_id = session.id();
                    let peer_session_id = session.peer_session_id();
                    if session.is_expired() {
                        log::info!("{mn} -- closing expired idle session {session_id} peer={peer_session_id:?}");
                        let _ = session.shutdown().await;
                        continue;
                    }
                    let mut idle = client.idle_pool.lock().await;
                    if !idle.iter().any(|item| item.session.id() == session.id()) {
                        if idle.len() < MAX_IDLE_SESSIONS {
                            idle.push(IdleSession::new(session));
                            let len = idle.len();
                            log::trace!("{mn} -- added idle session {session_id} peer={peer_session_id:?}, total idle: {len}");
                        } else {
                            drop(idle);
                            log::trace!("{mn} -- closing excess idle session {session_id} peer={peer_session_id:?}");
                            let _ = session.shutdown().await;
                        }
                    }
                }
            }
        });
        session.run().await?;
        Ok(session)
    }

    async fn take_idle(&self) -> Option<Arc<Session>> {
        let mut idle = self.idle_pool.lock().await;
        while let Some((index, _)) = idle.iter().enumerate().min_by_key(|(_, item)| item.session.id()) {
            let item = idle.swap_remove(index);
            if !item.session.is_closed() && !item.session.is_expired() {
                return Some(item.session);
            }
            tokio::spawn(async move { item.session.shutdown().await });
        }
        None
    }

    pub async fn shutdown(&self) -> std::io::Result<()> {
        self.close_token.cancel();
        let idle_sessions = self.idle_pool.lock().await.drain(..).map(|idle| idle.session).collect::<Vec<_>>();
        let mut first_error = None;
        for session in idle_sessions {
            if let Err(error) = session.shutdown().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn cleanup(&self, timeout: Duration) {
        let expiration = Instant::now().checked_sub(timeout).unwrap_or_else(Instant::now);
        let mut idle = self.idle_pool.lock().await;
        let mut retained = Vec::with_capacity(idle.len());
        for item in idle.drain(..) {
            if item.since < expiration || item.session.is_expired() {
                let session = item.session;
                tokio::spawn(async move { session.shutdown().await });
            } else {
                retained.push(item);
            }
        }
        let mn = method_name_unstable!();
        log::trace!("{mn} -- cleaned up idle sessions, total idle: {}", retained.len());
        *idle = retained;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::session::tests::gated_transport;
    use crate::{DEFAULT_MAX_SESSION_AGE, DEFAULT_SCHEME};
    use std::sync::atomic::Ordering;

    async fn close_stream_cleanly(stream: &mut Stream, server: &Arc<Session>) {
        let server_stream = server.accept_stream().await.unwrap();
        server_stream.shutdown_write_by_send_fin_to_remote().await.unwrap();
        let mut buffer = [0u8; 1];
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if stream.read(&mut buffer).await.unwrap() == 0 {
                    break;
                }
            }
        })
        .await
        .expect("client should observe the server FIN and drain it");
        stream.close().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !server.is_idle().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("server should remove the stream after both FINs");
    }

    #[tokio::test]
    async fn blocked_syn_does_not_block_independent_session_creation() {
        let gates = Arc::new(std::sync::Mutex::new(Vec::new()));
        let servers = Arc::new(Mutex::new(Vec::new()));
        let dial_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dialer: Dialer = {
            let gates = Arc::clone(&gates);
            let servers = Arc::clone(&servers);
            let dial_count = Arc::clone(&dial_count);
            Arc::new(move || {
                let (transport, peer, gate) = gated_transport();
                gates.lock().unwrap().push(gate);
                let server = Session::new_server(
                    dial_count.fetch_add(1, Ordering::Relaxed),
                    Box::new(peer),
                    Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
                    DEFAULT_MAX_SESSION_AGE,
                );
                let servers = Arc::clone(&servers);
                Box::pin(async move {
                    server.run().await?;
                    servers.lock().await.push(server);
                    Ok(transport)
                })
            })
        };
        let client = Client::new(
            dialer,
            Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
            Duration::from_secs(30),
            Duration::from_secs(3600),
        );
        let mut first = client.create_stream().await.unwrap();
        let second = client.create_stream().await.unwrap();
        let healthy = client.create_stream().await.unwrap();
        assert_ne!(first.session_id(), second.session_id());
        assert_ne!(second.session_id(), healthy.session_id());
        let first_server = Arc::clone(&servers.lock().await[0]);
        close_stream_cleanly(&mut first, &first_server).await;
        let gate = Arc::clone(&gates.lock().unwrap()[0]);
        gate.block();
        let blocked = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.create_stream().await }
        });
        gate.wait_pending().await;
        let later = tokio::time::timeout(Duration::from_secs(1), client.create_stream())
            .await
            .expect("blocked SYN must not prevent an independent Session")
            .unwrap();
        assert_ne!(later.session_id(), healthy.session_id());
        assert_eq!(dial_count.load(Ordering::Relaxed), 4);
        gate.release();
        let recovered = blocked.await.unwrap().unwrap();
        assert_eq!(recovered.session_id(), first.session_id());
        for server in servers.lock().await.drain(..) {
            server.shutdown().await.unwrap();
        }
        drop((recovered, later, healthy, second));
    }

    #[tokio::test]
    async fn stalled_dial_does_not_block_reuse_of_existing_session() {
        let dial_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dial_started = Arc::new(tokio::sync::Notify::new());
        let dial_resume = Arc::new(tokio::sync::Notify::new());
        let servers = Arc::new(Mutex::new(Vec::new()));
        let dialer: Dialer = {
            let dial_count = Arc::clone(&dial_count);
            let dial_started = Arc::clone(&dial_started);
            let dial_resume = Arc::clone(&dial_resume);
            let servers = Arc::clone(&servers);
            Arc::new(move || {
                let sequence = dial_count.fetch_add(1, Ordering::Relaxed);
                let dial_started = Arc::clone(&dial_started);
                let dial_resume = Arc::clone(&dial_resume);
                let servers = Arc::clone(&servers);
                Box::pin(async move {
                    if sequence == 1 {
                        dial_started.notify_one();
                        dial_resume.notified().await;
                    }
                    let (transport, peer) = tokio::io::duplex(128 * 1024);
                    let server = Session::new_server(
                        sequence,
                        Box::new(peer),
                        Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
                        DEFAULT_MAX_SESSION_AGE,
                    );
                    server.run().await?;
                    servers.lock().await.push(server);
                    Ok(Box::new(transport) as BoxTransport)
                })
            })
        };
        let client = Client::new(
            dialer,
            Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
            Duration::from_secs(30),
            Duration::from_secs(3600),
        );
        let mut first = client.create_stream().await.unwrap();
        let first_session = first.session_id();
        let dialing = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.create_stream().await }
        });
        tokio::time::timeout(Duration::from_secs(1), dial_started.notified()).await.unwrap();
        let first_server = Arc::clone(&servers.lock().await[0]);
        close_stream_cleanly(&mut first, &first_server).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while client.idle_pool.lock().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fully closed session should be available while another dial stalls");
        let reused = tokio::time::timeout(Duration::from_secs(1), client.create_stream())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reused.session_id(), first_session);
        dial_resume.notify_one();
        let created = dialing.await.unwrap().unwrap();
        for server in servers.lock().await.drain(..) {
            server.shutdown().await.unwrap();
        }
        drop((reused, created));
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_dials_timeout_independently() {
        let dial_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dialer: Dialer = {
            let dial_count = Arc::clone(&dial_count);
            Arc::new(move || {
                dial_count.fetch_add(1, Ordering::Relaxed);
                Box::pin(std::future::pending())
            })
        };
        let client = Client::new(
            dialer,
            Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
            Duration::from_secs(30),
            Duration::from_secs(3600),
        );
        for _attempt in 0..2 {
            let error = client.create_stream().await.err().expect("stalled dial must time out");
            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        }
        assert_eq!(dial_count.load(Ordering::Relaxed), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_dial_proceeds_while_another_stalls() {
        let dial_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_dial_started = Arc::new(tokio::sync::Notify::new());
        let servers = Arc::new(Mutex::new(Vec::new()));
        let dialer: Dialer = {
            let dial_count = Arc::clone(&dial_count);
            let first_dial_started = Arc::clone(&first_dial_started);
            let servers = Arc::clone(&servers);
            Arc::new(move || {
                let sequence = dial_count.fetch_add(1, Ordering::Relaxed);
                let first_dial_started = Arc::clone(&first_dial_started);
                let servers = Arc::clone(&servers);
                Box::pin(async move {
                    if sequence == 0 {
                        first_dial_started.notify_one();
                        return std::future::pending().await;
                    }
                    let (client_io, server_io) = tokio::io::duplex(128 * 1024);
                    let server = Session::new_server(
                        sequence,
                        Box::new(server_io),
                        Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
                        DEFAULT_MAX_SESSION_AGE,
                    );
                    server.run().await?;
                    servers.lock().await.push(server);
                    Ok(Box::new(client_io) as BoxTransport)
                })
            })
        };
        let client = Client::new(
            dialer,
            Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
            Duration::from_secs(30),
            Duration::from_secs(3600),
        );

        let first = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.create_stream().await }
        });
        first_dial_started.notified().await;
        let concurrent = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.create_stream().await }
        });
        let stream = tokio::time::timeout(Duration::from_secs(1), concurrent)
            .await
            .expect("independent dial should not queue behind the stalled dial")
            .unwrap()
            .unwrap();
        assert!(!first.is_finished());

        client.shutdown().await.unwrap();
        assert!(first.await.unwrap().is_err());
        for server in servers.lock().await.drain(..) {
            server.shutdown().await.unwrap();
        }
        drop(stream);
    }

    #[tokio::test]
    async fn reuses_oldest_idle_session() {
        let dial_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_sessions = Arc::new(Mutex::new(Vec::new()));
        let dial_count_for_dialer = Arc::clone(&dial_count);
        let server_sessions_for_dialer = Arc::clone(&server_sessions);
        let dialer: Dialer = Arc::new(move || {
            let dial_count = Arc::clone(&dial_count_for_dialer);
            let server_sessions = Arc::clone(&server_sessions_for_dialer);
            Box::pin(async move {
                dial_count.fetch_add(1, Ordering::Relaxed);
                let (client_io, server_io) = tokio::io::duplex(128 * 1024);
                let server = Session::new_server(
                    1,
                    Box::new(server_io),
                    Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
                    DEFAULT_MAX_SESSION_AGE,
                );
                server.run().await?;
                server_sessions.lock().await.push(server);
                Ok(Box::new(client_io) as BoxTransport)
            })
        });
        let client = Client::new(
            dialer,
            Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
            Duration::from_secs(60),
            Duration::from_secs(60 * 60),
        );

        let mut first = client.create_stream().await.unwrap();
        let session = first.session().unwrap();
        let mut second = client.create_stream().await.unwrap();
        assert_ne!(session.id(), second.session_id());
        assert_eq!(dial_count.load(Ordering::Relaxed), 2);
        let first_server = Arc::clone(&server_sessions.lock().await[0]);
        close_stream_cleanly(&mut first, &first_server).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while client.idle_pool.lock().await.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fully closed session should enter the idle pool");
        assert_eq!(client.idle_pool.lock().await.len(), 1);
        let mut third = client.create_stream().await.unwrap();
        assert_eq!(session.id(), third.session_id());
        assert_eq!(dial_count.load(Ordering::Relaxed), 2);
        let second_server = Arc::clone(&server_sessions.lock().await[1]);
        close_stream_cleanly(&mut second, &second_server).await;
        close_stream_cleanly(&mut third, &first_server).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while client.idle_pool.lock().await.len() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both fully closed sessions should enter the idle pool");

        assert_eq!(client.idle_pool.lock().await.len(), 2);
        for server in server_sessions.lock().await.drain(..) {
            let _ = server.shutdown().await;
        }
    }

    #[tokio::test]
    async fn does_not_reuse_session_until_peer_stream_finishes() {
        let dial_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_sessions = Arc::new(Mutex::new(Vec::new()));
        let dial_count_for_dialer = Arc::clone(&dial_count);
        let servers_for_dialer = Arc::clone(&server_sessions);
        let dialer: Dialer = Arc::new(move || {
            let sequence = dial_count_for_dialer.fetch_add(1, Ordering::Relaxed);
            let servers = Arc::clone(&servers_for_dialer);
            Box::pin(async move {
                let (client_io, server_io) = tokio::io::duplex(128 * 1024);
                let server = Session::new_server(
                    sequence,
                    Box::new(server_io),
                    Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
                    DEFAULT_MAX_SESSION_AGE,
                );
                server.run().await?;
                servers.lock().await.push(server);
                Ok(Box::new(client_io) as BoxTransport)
            })
        });
        let client = Client::new(
            dialer,
            Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
            Duration::from_secs(60),
            Duration::from_secs(60 * 60),
        );

        let mut first = client.create_stream().await.unwrap();
        let first_session = first.session().unwrap();
        let first_server = Arc::clone(&server_sessions.lock().await[0]);
        let _server_stream = first_server.accept_stream().await.unwrap();
        first.close().await.unwrap();
        assert!(!first_session.is_closed(), "session should stay open while the peer stream drains");
        assert!(!first_session.is_idle().await, "stream slot must stay reserved until peer FIN");

        let second = client.create_stream().await.unwrap();
        assert_ne!(first_session.id(), second.session_id());
        assert_eq!(dial_count.load(Ordering::Relaxed), 2);

        client.shutdown().await.unwrap();
        for server in server_sessions.lock().await.drain(..) {
            let _ = server.shutdown().await;
        }
    }

    #[tokio::test]
    async fn shutdown_closes_and_removes_idle_sessions() {
        let server_sessions = Arc::new(Mutex::new(Vec::new()));
        let servers_for_dialer = Arc::clone(&server_sessions);
        let dialer: Dialer = Arc::new(move || {
            let servers = Arc::clone(&servers_for_dialer);
            Box::pin(async move {
                let (client_io, server_io) = tokio::io::duplex(128 * 1024);
                let server = Session::new_server(
                    1,
                    Box::new(server_io),
                    Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
                    DEFAULT_MAX_SESSION_AGE,
                );
                server.run().await?;
                servers.lock().await.push(Arc::clone(&server));
                Ok(Box::new(client_io) as BoxTransport)
            })
        });
        let client = Client::new(
            dialer,
            Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
            Duration::from_secs(60),
            Duration::from_secs(3600),
        );

        let active_stream = client.create_stream().await.unwrap();
        let active_session = active_stream.session().unwrap();
        let mut idle_stream = client.create_stream().await.unwrap();
        let idle_session = idle_stream.session().unwrap();
        let idle_server = Arc::clone(&server_sessions.lock().await[1]);
        close_stream_cleanly(&mut idle_stream, &idle_server).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while client.idle_pool.lock().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("closed stream should return its session to the idle pool");

        client.shutdown().await.unwrap();

        assert!(idle_session.is_closed());
        tokio::time::timeout(Duration::from_secs(1), async {
            while !active_session.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("client cancellation should close active sessions");
        assert!(client.idle_pool.lock().await.is_empty());
        assert_eq!(client.create_stream().await.err().unwrap().kind(), std::io::ErrorKind::BrokenPipe);
        for server in server_sessions.lock().await.drain(..) {
            server.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn rotates_expired_session_without_closing_active_streams() {
        let dial_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_sessions = Arc::new(Mutex::new(Vec::new()));
        let dial_count_for_dialer = Arc::clone(&dial_count);
        let server_sessions_for_dialer = Arc::clone(&server_sessions);
        let dialer: Dialer = Arc::new(move || {
            let dial_count = Arc::clone(&dial_count_for_dialer);
            let server_sessions = Arc::clone(&server_sessions_for_dialer);
            Box::pin(async move {
                dial_count.fetch_add(1, Ordering::Relaxed);
                let (client_io, server_io) = tokio::io::duplex(128 * 1024);
                let server = Session::new_server(
                    1,
                    Box::new(server_io),
                    Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
                    DEFAULT_MAX_SESSION_AGE,
                );
                server.run().await?;
                server_sessions.lock().await.push(server);
                Ok(Box::new(client_io) as BoxTransport)
            })
        });
        let client = Client::new(
            dialer,
            Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
            Duration::from_secs(60),
            Duration::from_millis(30),
        );

        let mut first = client.create_stream().await.unwrap();
        let first_session_id = first.session_id();
        tokio::time::sleep(Duration::from_millis(40)).await;
        first.close().await.unwrap();
        tokio::task::yield_now().await;

        let second = client.create_stream().await.unwrap();
        assert_ne!(first_session_id, second.session_id());
        assert_eq!(dial_count.load(Ordering::Relaxed), 2);

        drop(second);
        for server in server_sessions.lock().await.drain(..) {
            let _ = server.shutdown().await;
        }
    }

    #[tokio::test]
    async fn expired_session_is_closed_instead_of_entering_idle_pool() {
        let server_sessions = Arc::new(Mutex::new(Vec::new()));
        let servers_for_dialer = Arc::clone(&server_sessions);
        let dialer: Dialer = Arc::new(move || {
            let servers = Arc::clone(&servers_for_dialer);
            Box::pin(async move {
                let (client_io, server_io) = tokio::io::duplex(128 * 1024);
                let server = Session::new_server(
                    1,
                    Box::new(server_io),
                    Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
                    DEFAULT_MAX_SESSION_AGE,
                );
                server.run().await?;
                servers.lock().await.push(server);
                Ok(Box::new(client_io) as BoxTransport)
            })
        });
        let client = Client::new(
            dialer,
            Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
            Duration::from_secs(60),
            Duration::from_millis(30),
        );

        let mut stream = client.create_stream().await.unwrap();
        let session = stream.session().unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        stream.close().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !session.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expired session should shut down as soon as its last stream closes");
        assert!(client.idle_pool.lock().await.is_empty());

        for server in server_sessions.lock().await.drain(..) {
            server.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn concurrent_stream_creation_uses_one_session_per_active_stream() {
        let dial_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_sessions = Arc::new(Mutex::new(Vec::new()));
        let dial_count_for_dialer = Arc::clone(&dial_count);
        let server_sessions_for_dialer = Arc::clone(&server_sessions);
        let dialer: Dialer = Arc::new(move || {
            let dial_count = Arc::clone(&dial_count_for_dialer);
            let server_sessions = Arc::clone(&server_sessions_for_dialer);
            Box::pin(async move {
                dial_count.fetch_add(1, Ordering::Relaxed);
                let (client_io, server_io) = tokio::io::duplex(128 * 1024);
                let server = Session::new_server(
                    dial_count.load(Ordering::Relaxed),
                    Box::new(server_io),
                    Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
                    DEFAULT_MAX_SESSION_AGE,
                );
                server.run().await?;
                server_sessions.lock().await.push(server);
                Ok(Box::new(client_io) as BoxTransport)
            })
        });
        let client = Client::new(
            dialer,
            Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
            Duration::from_secs(60),
            Duration::from_secs(60 * 60),
        );
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..100 {
            let client = Arc::clone(&client);
            tasks.spawn(async move { client.create_stream().await.unwrap() });
        }
        let mut streams = Vec::with_capacity(100);
        while let Some(result) = tasks.join_next().await {
            streams.push(result.unwrap());
        }

        assert_eq!(streams.len(), 100);
        assert_eq!(dial_count.load(Ordering::Relaxed), 100);
        let sessions = streams.iter().map(|stream| stream.session().unwrap()).collect::<Vec<_>>();
        drop(streams);
        client.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while sessions.iter().any(|session| !session.is_closed()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("client shutdown should close all sessions with dropped streams");
        for server in server_sessions.lock().await.drain(..) {
            let _ = server.shutdown().await;
        }
    }

    #[tokio::test]
    async fn default_scale_concurrency_uses_one_session_per_stream() {
        let dial_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_sessions = Arc::new(Mutex::new(Vec::new()));
        let dial_count_for_dialer = Arc::clone(&dial_count);
        let server_sessions_for_dialer = Arc::clone(&server_sessions);
        let dialer: Dialer = Arc::new(move || {
            let dial_count = Arc::clone(&dial_count_for_dialer);
            let server_sessions = Arc::clone(&server_sessions_for_dialer);
            Box::pin(async move {
                dial_count.fetch_add(1, Ordering::Relaxed);
                let (client_io, server_io) = tokio::io::duplex(256 * 1024);
                let server = Session::new_server(
                    1,
                    Box::new(server_io),
                    Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
                    DEFAULT_MAX_SESSION_AGE,
                );
                server.run().await?;
                server_sessions.lock().await.push(server);
                Ok(Box::new(client_io) as BoxTransport)
            })
        });
        let client = Client::new(
            dialer,
            Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap())),
            Duration::from_secs(30),
            Duration::from_secs(60 * 60),
        );
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..100 {
            let client = Arc::clone(&client);
            tasks.spawn(async move { client.create_stream().await.unwrap() });
        }
        let mut streams = Vec::with_capacity(100);
        while let Some(result) = tasks.join_next().await {
            streams.push(result.unwrap());
        }

        assert_eq!(streams.len(), 100);
        assert_eq!(dial_count.load(Ordering::Relaxed), 100);
        drop(streams);
        for server in server_sessions.lock().await.drain(..) {
            let _ = server.shutdown().await;
        }
    }
}
