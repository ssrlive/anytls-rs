use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::{Mutex, RwLock, mpsc};

use crate::{
    padding::PaddingFactory,
    session::{BoxTransport, Session, Stream},
};

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
    idle_pool: Mutex<Vec<IdleSession>>,
    sessions: Mutex<Vec<std::sync::Weak<Session>>>,
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
            idle_pool: Mutex::new(Vec::new()),
            sessions: Mutex::new(Vec::new()),
            next_sequence: std::sync::atomic::AtomicUsize::new(0),
            max_streams_per_session: max_streams_per_session.max(1),
            max_session_age,
        });
        let cleanup_client = Arc::downgrade(&client);
        tokio::spawn(async move {
            let interval = if idle_timeout <= Duration::from_secs(5) {
                Duration::from_secs(30)
            } else {
                idle_timeout
            };
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
        loop {
            let _allocation = self.allocation.lock().await;
            let session = self.take_idle().await.or(self.take_session_with_capacity().await);
            let session = match session {
                Some(session) => session,
                None => self.create_session().await?,
            };
            match session.open_stream().await {
                Ok(stream) => return Ok(stream),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) => {
                    let _ = session.shutdown().await;
                    return Err(error);
                }
            }
        }
    }

    async fn create_session(self: &Arc<Self>) -> std::io::Result<Arc<Session>> {
        let transport = (self.dialer)().await?;
        let sequence = self.next_sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let session = Session::new_client(sequence, transport, Arc::clone(&self.padding), self.max_streams_per_session);
        self.sessions.lock().await.push(Arc::downgrade(&session));
        let (sender, mut receiver) = mpsc::unbounded_channel();
        session.set_idle_sender(sender).await;
        let client = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(session) = receiver.recv().await {
                let Some(client) = client.upgrade() else {
                    break;
                };
                if !session.is_closed() {
                    let mut idle = client.idle_pool.lock().await;
                    if !idle.iter().any(|item| item.session.id() == session.id()) {
                        idle.push(IdleSession::new(session));
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
            if !item.session.is_closed() && !item.session.is_expired(self.max_session_age) {
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
            if !session.is_closed() && !session.is_expired(self.max_session_age) && session.has_stream_capacity().await {
                candidates.push(session);
            }
        }
        candidates.into_iter().min_by_key(|session| session.id())
    }

    async fn cleanup(&self, timeout: Duration) {
        self.sessions.lock().await.retain(|session| session.strong_count() > 0);
        let expiration = Instant::now().checked_sub(timeout).unwrap_or_else(Instant::now);
        let mut idle = self.idle_pool.lock().await;
        let mut retained = Vec::with_capacity(idle.len());
        for item in idle.drain(..) {
            if item.since < expiration || item.session.is_expired(self.max_session_age) {
                let session = item.session;
                tokio::spawn(async move { session.shutdown().await });
            } else {
                retained.push(item);
            }
        }
        *idle = retained;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::padding::DEFAULT_SCHEME;
    use std::sync::atomic::Ordering;

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
        let session = first.session();
        let second = session.open_stream().await.unwrap();
        assert_eq!(session.id(), second.session().id());
        let mut third = client.create_stream().await.unwrap();
        assert_ne!(session.id(), third.session().id());
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
            Duration::from_nanos(1),
        );

        let mut first = client.create_stream().await.unwrap();
        let first_session_id = first.session().id();
        first.close().await.unwrap();
        tokio::task::yield_now().await;

        let second = client.create_stream().await.unwrap();
        assert_ne!(first_session_id, second.session().id());
        assert_eq!(dial_count.load(Ordering::Relaxed), 2);

        drop(second);
        for server in server_sessions.lock().await.drain(..) {
            let _ = server.shutdown().await;
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
        for server in server_sessions.lock().await.drain(..) {
            let _ = server.shutdown().await;
        }
    }
}
