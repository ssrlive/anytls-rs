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
    allocation: Mutex<()>,
    creation: Mutex<()>,
    idle_pool: Mutex<Vec<IdleSession>>,
    sessions: Mutex<Vec<std::sync::Weak<Session>>>,
    shutting_down: std::sync::atomic::AtomicBool,
    next_sequence: std::sync::atomic::AtomicUsize,
    max_streams_per_session: usize,
    max_session_age: Duration,
}

impl Client {
    pub fn new(
        dialer: Dialer,
        padding: Arc<RwLock<PaddingFactory>>,
        idle_timeout: Duration,
        max_streams_per_session: usize,
        max_session_age: Duration,
    ) -> Arc<Self> {
        let client = Arc::new(Self {
            dialer,
            padding,
            allocation: Mutex::new(()),
            creation: Mutex::new(()),
            idle_pool: Mutex::new(Vec::new()),
            sessions: Mutex::new(Vec::new()),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            next_sequence: std::sync::atomic::AtomicUsize::new(0),
            max_streams_per_session: max_streams_per_session.max(1),
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
                let deadline = tokio::time::Instant::now() + SESSION_CONNECT_TIMEOUT;
                let _creation = tokio::time::timeout_at(deadline, self.creation.lock())
                    .await
                    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "waiting for AnyTLS session creation timed out"))?;
                match self.reserve_pooled_stream().await? {
                    Some(reserved) => reserved,
                    None => {
                        let session = tokio::time::timeout_at(deadline, self.create_session())
                            .await
                            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "AnyTLS session connection timed out"))??;
                        let _allocation = self.allocation.lock().await;
                        if self.shutting_down.load(std::sync::atomic::Ordering::Acquire) {
                            drop(_allocation);
                            let _ = session.shutdown().await;
                            return Err(self.shutting_down_error());
                        }
                        let stream = session.reserve_stream().await?;
                        self.sessions.lock().await.push(Arc::downgrade(&session));
                        (session, stream)
                    }
                }
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
        if self.shutting_down.load(std::sync::atomic::Ordering::Acquire) {
            Err(self.shutting_down_error())
        } else {
            Ok(())
        }
    }

    fn shutting_down_error(&self) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "AnyTLS client is shutting down")
    }

    async fn reserve_pooled_stream(&self) -> std::io::Result<Option<(Arc<Session>, Stream)>> {
        let _allocation = self.allocation.lock().await;
        self.ensure_running()?;
        loop {
            let session = match self.take_idle().await {
                Some(session) => Some(session),
                None => self.take_session_with_capacity().await,
            };
            let Some(session) = session else {
                return Ok(None);
            };
            match session.reserve_stream().await {
                Ok(stream) => return Ok(Some((session, stream))),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => continue,
                Err(error) => return Err(error),
            }
        }
    }

    async fn create_session(self: &Arc<Self>) -> std::io::Result<Arc<Session>> {
        let transport = (self.dialer)().await?;
        let sequence = self.next_sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let session = Session::new_client(
            sequence,
            transport,
            Arc::clone(&self.padding),
            self.max_streams_per_session,
            self.max_session_age,
        );
        let (sender, mut receiver) = mpsc::unbounded_channel();
        session.set_idle_sender(sender).await;
        let client = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(session) = receiver.recv().await {
                let Some(client) = client.upgrade() else {
                    break;
                };
                let _allocation = client.allocation.lock().await;
                if client.shutting_down.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                if !session.is_closed() && session.is_idle().await {
                    let mn = method_name_unstable!();
                    let session_id = session.id();
                    if session.is_expired() {
                        log::info!("{mn} -- closing expired idle session {session_id}");
                        let _ = session.shutdown().await;
                        continue;
                    }
                    let mut idle = client.idle_pool.lock().await;
                    if !idle.iter().any(|item| item.session.id() == session.id()) {
                        if idle.len() < MAX_IDLE_SESSIONS {
                            idle.push(IdleSession::new(session));
                            log::trace!("{mn} -- added idle session {session_id}, total idle: {}", idle.len());
                        } else {
                            drop(idle);
                            log::trace!("{mn} -- closing excess idle session {session_id}");
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

    async fn take_session_with_capacity(&self) -> Option<Arc<Session>> {
        let sessions = {
            let mut registered = self.sessions.lock().await;
            registered.retain(|session| session.strong_count() > 0);
            registered.iter().filter_map(std::sync::Weak::upgrade).collect::<Vec<_>>()
        };
        let mut candidates = Vec::new();
        for session in sessions {
            if !session.is_closed() && !session.is_expired() && session.has_stream_capacity().await {
                candidates.push(session);
            }
        }
        candidates.into_iter().min_by_key(|session| session.id())
    }

    pub async fn shutdown(&self) -> std::io::Result<()> {
        self.shutting_down.store(true, std::sync::atomic::Ordering::Release);
        let _allocation = self.allocation.lock().await;
        let mut sessions = {
            let mut registered = self.sessions.lock().await;
            registered.retain(|session| session.strong_count() > 0);
            registered.iter().filter_map(std::sync::Weak::upgrade).collect::<Vec<_>>()
        };
        let idle_sessions = self.idle_pool.lock().await.drain(..).map(|idle| idle.session).collect::<Vec<_>>();
        for session in idle_sessions {
            if !sessions.iter().any(|registered| Arc::ptr_eq(registered, &session)) {
                sessions.push(session);
            }
        }
        drop(_allocation);

        let mut first_error = None;
        for session in sessions {
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
        self.sessions.lock().await.retain(|session| session.strong_count() > 0);
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

    #[tokio::test]
    async fn blocked_syn_does_not_block_allocation_on_healthy_session() {
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
                    2,
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
            2,
            Duration::from_secs(3600),
        );
        let mut first = client.create_stream().await.unwrap();
        let second = client.create_stream().await.unwrap();
        let healthy = client.create_stream().await.unwrap();
        assert_eq!(first.session_id(), second.session_id());
        assert_ne!(second.session_id(), healthy.session_id());
        first.close().await.unwrap();
        let gate = Arc::clone(&gates.lock().unwrap()[0]);
        gate.block();
        let blocked = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.create_stream().await }
        });
        gate.wait_pending().await;
        let later = tokio::time::timeout(Duration::from_secs(1), client.create_stream())
            .await
            .expect("blocked SYN must not hold global allocation")
            .unwrap();
        assert_eq!(later.session_id(), healthy.session_id());
        assert_eq!(dial_count.load(Ordering::Relaxed), 2);
        gate.release();
        let recovered = blocked.await.unwrap().unwrap();
        assert_eq!(recovered.session_id(), second.session_id());
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
                        1,
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
            1,
            Duration::from_secs(3600),
        );
        let mut first = client.create_stream().await.unwrap();
        let first_session = first.session_id();
        let dialing = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.create_stream().await }
        });
        tokio::time::timeout(Duration::from_secs(1), dial_started.notified()).await.unwrap();
        first.close().await.unwrap();
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
    async fn stalled_dial_times_out_and_releases_creation_lock() {
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
            1,
            Duration::from_secs(3600),
        );
        for _attempt in 0..2 {
            let error = client.create_stream().await.err().expect("stalled dial must time out");
            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        }
        assert_eq!(dial_count.load(Ordering::Relaxed), 2);
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
                    2,
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
            2,
            Duration::from_secs(60 * 60),
        );

        let mut first = client.create_stream().await.unwrap();
        let session = first.session().unwrap();
        let second = session.open_stream().await.unwrap();
        assert_eq!(session.id(), second.session_id());
        let mut third = client.create_stream().await.unwrap();
        assert_ne!(session.id(), third.session_id());
        assert_eq!(dial_count.load(Ordering::Relaxed), 2);
        first.close().await.unwrap();
        tokio::task::yield_now().await;
        assert!(client.idle_pool.lock().await.is_empty());
        let mut second = second;
        second.close().await.unwrap();
        third.close().await.unwrap();
        tokio::task::yield_now().await;

        assert_eq!(client.idle_pool.lock().await.len(), 2);
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
                    1,
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
            1,
            Duration::from_secs(3600),
        );

        let mut stream = client.create_stream().await.unwrap();
        let session = stream.session().unwrap();
        stream.close().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while client.idle_pool.lock().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("closed stream should return its session to the idle pool");

        client.shutdown().await.unwrap();

        assert!(session.is_closed());
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
                    1,
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
            1,
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
                    1,
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
            1,
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
    async fn concurrent_stream_creation_reuses_sessions_within_capacity() {
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
                    16,
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
            16,
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
        assert_eq!(dial_count.load(Ordering::Relaxed), 7);
        drop(streams);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if client.idle_pool.lock().await.len() == MAX_IDLE_SESSIONS {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("idle pool should retain only the configured cap");
        assert_eq!(client.idle_pool.lock().await.len(), MAX_IDLE_SESSIONS);
        for server in server_sessions.lock().await.drain(..) {
            let _ = server.shutdown().await;
        }
    }

    #[tokio::test]
    async fn default_scale_concurrency_fits_in_one_session() {
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
                    128,
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
            128,
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
        assert_eq!(dial_count.load(Ordering::Relaxed), 1);
        drop(streams);
        for server in server_sessions.lock().await.drain(..) {
            let _ = server.shutdown().await;
        }
    }
}
