use crate::AsyncReadWrite;
use crate::core::{Command, Frame, HEADER_OVERHEAD_SIZE, State};
use crate::proxy::session::Stream;
use crate::runtime::{FrameWrite, MAX_QUEUED_FRAME_BYTES, Protocol, ProtocolHost, WriterRuntimeState};
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, Notify, Semaphore, mpsc::Sender, watch};

static SESSION_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

pub struct Session {
    pub id: u64,
    reader: Mutex<tokio::io::ReadHalf<Box<dyn AsyncReadWrite>>>,
    streams: Arc<Mutex<HashMap<u32, Arc<Stream>>>>,
    next_stream_id: AtomicU32,
    max_incoming_streams: AtomicUsize,
    closed: AtomicBool,
    started: Mutex<bool>,
    pub(crate) is_client: bool,
    pub(crate) protocol_state: Arc<State>,
    writer_state: Arc<WriterRuntimeState>,
    idle_state: Arc<watch::Sender<bool>>,
    close_notify: Arc<Notify>,
    #[allow(clippy::type_complexity)]
    on_new_stream: Option<Arc<Box<dyn Fn(Arc<Stream>) + Send + Sync>>>,
    protocol: Arc<dyn Protocol>,
    control_tx: Sender<FrameWrite>,
    pub(crate) frame_tx: Sender<FrameWrite>,
    write_budget: Arc<Semaphore>,
}

impl Session {
    pub(crate) fn new_with_protocol(
        conn: Box<dyn AsyncReadWrite>,
        is_client: bool,
        on_new_stream: Option<Box<dyn Fn(Arc<Stream>) + Send + Sync>>,
        protocol: Arc<dyn Protocol>,
        protocol_state: Arc<State>,
        writer_state: Arc<WriterRuntimeState>,
    ) -> Self {
        let (reader, writer) = tokio::io::split(conn);
        let (control_tx, control_rx) = tokio::sync::mpsc::channel::<FrameWrite>(32);
        let (frame_tx, data_rx) = tokio::sync::mpsc::channel::<FrameWrite>(100);
        let write_budget = Arc::new(Semaphore::new(MAX_QUEUED_FRAME_BYTES));
        let (idle_state, _) = watch::channel(true);
        protocol.spawn_writer_task(writer, control_rx, data_rx, protocol_state.clone(), writer_state.clone());

        Self {
            id: SESSION_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            reader: Mutex::new(reader),
            streams: Arc::new(Mutex::new(HashMap::new())),
            next_stream_id: AtomicU32::new(0),
            max_incoming_streams: AtomicUsize::new(1024),
            closed: AtomicBool::new(false),
            started: Mutex::new(false),
            is_client,
            protocol_state,
            writer_state,
            idle_state: Arc::new(idle_state),
            close_notify: Arc::new(Notify::new()),
            on_new_stream: on_new_stream.map(Arc::new),
            protocol,
            control_tx,
            frame_tx,
            write_budget,
        }
    }

    pub async fn ensure_started(&self) -> std::io::Result<()> {
        let should_start = {
            let mut started = self.started.lock().await;
            if *started {
                false
            } else {
                *started = true;
                true
            }
        };

        if should_start && let Err(error) = self.protocol.on_session_start(self).await {
            *self.started.lock().await = false;
            return Err(error);
        }
        Ok(())
    }

    pub async fn run(&self) -> std::io::Result<()> {
        self.ensure_started().await?;
        let result = self.recv_loop().await;
        let _ = self.terminate().await;
        result
    }

    pub async fn open_stream(&self, max_streams: usize) -> std::io::Result<Arc<Stream>> {
        if self.is_terminated().await {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed"));
        }

        let (sid, stream) = {
            let mut streams = self.streams.lock().await;
            if self.closed.load(Ordering::Acquire) {
                return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed"));
            }
            if streams.len() >= max_streams {
                return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "Session stream limit reached"));
            }

            let sid = self.next_stream_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            if sid == 0 {
                return Err(std::io::Error::other("Stream identifier exhausted"));
            }

            let stream = Arc::new(self.new_stream(sid));
            streams.insert(sid, stream.clone());
            self.idle_state.send_replace(false);
            (sid, stream)
        };

        if let Err(error) = self.write_frame(Frame::new(Command::Syn, sid)).await {
            self.remove_stream(sid).await;
            stream.close_from_session(Some(std::io::Error::other(error.to_string()))).await;
            return Err(error);
        }

        Ok(stream)
    }

    pub async fn write_frame(&self, frame: Frame) -> std::io::Result<usize> {
        let len = frame.data.len();
        let budget = self.acquire_write_budget(len).await?;
        let sender = if matches!(frame.cmd, Command::Psh) {
            &self.frame_tx
        } else {
            &self.control_tx
        };
        sender
            .send(FrameWrite::new(frame, None, Some(budget)))
            .await
            .map(|_| len)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed"))
    }

    pub async fn write_frame_sync(&self, frame: Frame) -> std::io::Result<usize> {
        let len = frame.data.len();
        let budget = self.acquire_write_budget(len).await?;
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let sender = if matches!(frame.cmd, Command::Psh) {
            &self.frame_tx
        } else {
            &self.control_tx
        };
        sender
            .send(FrameWrite::new(frame, Some(ack_tx), Some(budget)))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed"))?;
        ack_rx
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Writer dropped"))??;
        Ok(len)
    }

    pub async fn terminate(&self) -> std::io::Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        self.close_notify.notify_waiters();

        let streams = {
            let mut streams = self.streams.lock().await;
            let stream_list = streams.values().cloned().collect::<Vec<_>>();
            streams.clear();
            self.idle_state.send_replace(true);
            stream_list
        };
        for stream in streams {
            stream.close_from_session(None).await;
        }
        Ok(())
    }

    pub async fn is_terminated(&self) -> bool {
        self.closed.load(Ordering::Acquire) || self.frame_tx.is_closed() || self.control_tx.is_closed() || self.writer_state.is_failed()
    }

    pub async fn peer_version(&self) -> u8 {
        self.protocol_state.peer_version()
    }

    pub async fn wait_for_idle(&self) {
        let mut idle_state = self.idle_state.subscribe();
        loop {
            if *idle_state.borrow() || self.is_terminated().await {
                return;
            }
            if idle_state.changed().await.is_err() {
                return;
            }
        }
    }

    pub async fn is_stream_open(&self) -> bool {
        !self.streams.lock().await.is_empty()
    }

    pub async fn has_stream_capacity(&self, max_streams: usize) -> bool {
        self.streams.lock().await.len() < max_streams
    }

    fn new_stream(&self, sid: u32) -> Stream {
        Stream::new(
            sid,
            self.control_tx.clone(),
            self.frame_tx.clone(),
            self.write_budget.clone(),
            Arc::downgrade(&self.streams),
            Arc::downgrade(&self.idle_state),
            self.protocol
                .make_stream_protocol_hooks(self.control_tx.clone(), self.protocol_state.clone()),
        )
    }

    async fn acquire_write_budget(&self, len: usize) -> std::io::Result<tokio::sync::OwnedSemaphorePermit> {
        let permits = len.clamp(1, MAX_QUEUED_FRAME_BYTES) as u32;
        self.write_budget
            .clone()
            .acquire_many_owned(permits)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session write budget closed"))
    }

    async fn recv_loop(&self) -> std::io::Result<()> {
        let mut buffer = vec![0_u8; 4096];
        let mut pending = Vec::new();
        let writer_failure = self.writer_state.failure_notified();

        loop {
            if self.closed.load(Ordering::Acquire) {
                return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed"));
            }

            let bytes_read = tokio::select! {
                _ = self.close_notify.notified() => {
                    return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed"));
                }
                _ = writer_failure.notified() => {
                    let _ = self.terminate().await;
                    return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session writer failed"));
                }
                result = async {
                    self.reader.lock().await.read(&mut buffer).await
                } => result?,
            };
            if bytes_read == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Connection closed"));
            }
            pending.extend_from_slice(&buffer[..bytes_read]);

            while let Some(frame) = Frame::from_bytes(&pending) {
                let frame_len = HEADER_OVERHEAD_SIZE + frame.data.len();
                pending.drain(..frame_len);
                self.protocol.handle_frame(self, frame).await?;
            }
        }
    }

    async fn stream_for_sid(&self, sid: u32) -> Option<Arc<Stream>> {
        self.streams.lock().await.get(&sid).cloned()
    }

    async fn remove_stream(&self, sid: u32) -> Option<Arc<Stream>> {
        let (stream, is_idle) = {
            let mut streams = self.streams.lock().await;
            let stream = streams.remove(&sid);
            (stream, streams.is_empty())
        };
        if is_idle {
            self.idle_state.send_replace(true);
        }
        stream
    }

    pub(crate) fn set_max_incoming_streams(&self, max_streams: usize) {
        self.max_incoming_streams.store(max_streams.max(1), Ordering::Release);
    }

    async fn create_incoming_stream(&self, sid: u32) -> std::io::Result<Option<Arc<Stream>>> {
        if sid == 0 || self.is_terminated().await {
            return Ok(None);
        }

        let stream = {
            let mut streams = self.streams.lock().await;
            if streams.contains_key(&sid) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "duplicate incoming stream identifier",
                ));
            }
            if streams.len() >= self.max_incoming_streams.load(Ordering::Acquire) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "session incoming stream limit reached",
                ));
            }
            let stream = Arc::new(self.new_stream(sid));
            streams.insert(sid, stream.clone());
            self.idle_state.send_replace(false);
            stream
        };

        if let Some(callback) = &self.on_new_stream {
            let callback = callback.clone();
            let callback_stream = stream.clone();
            tokio::spawn(async move {
                callback(callback_stream);
            });
        }
        Ok(Some(stream))
    }
}

#[async_trait]
impl ProtocolHost for Session {
    fn is_client(&self) -> bool {
        self.is_client
    }

    fn protocol_state(&self) -> Arc<State> {
        self.protocol_state.clone()
    }

    async fn send_frame(&self, frame: Frame) -> std::io::Result<usize> {
        self.write_frame(frame).await
    }

    async fn send_frame_sync(&self, frame: Frame) -> std::io::Result<usize> {
        self.write_frame_sync(frame).await
    }

    async fn push_stream_data(&self, sid: u32, data: Bytes) -> std::io::Result<()> {
        if let Some(stream) = self.stream_for_sid(sid).await {
            if stream.is_closed() {
                log::debug!("Ignoring payload for locally closed stream sid={sid}");
                return Ok(());
            }
            if let Err(error) = stream.push_data(data.as_ref()).await {
                if stream.is_closed() {
                    log::debug!("Ignoring push_data error for closed stream sid={sid}: {error}");
                    return Ok(());
                }
                return Err(error);
            }
        } else {
            log::debug!("Ignoring payload for unknown stream sid={sid}");
        }
        Ok(())
    }

    async fn ensure_incoming_stream(&self, sid: u32) -> std::io::Result<()> {
        if sid == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "SYN cannot use control sid 0"));
        }
        self.create_incoming_stream(sid).await?;
        Ok(())
    }

    async fn close_logical_stream(&self, sid: u32) -> std::io::Result<()> {
        if let Some(stream) = self.remove_stream(sid).await {
            stream.close_from_peer(None).await;
        }
        Ok(())
    }

    async fn terminate_session(&self, sid: u32, message: Option<String>) -> std::io::Result<()> {
        if let Some(stream) = self.remove_stream(sid).await {
            stream
                .close_from_peer(message.map(|message| std::io::Error::other(format!("remote: {message}"))))
                .await;
        }
        Ok(())
    }

    async fn resolve_stream_handshake(&self, sid: u32, message: String) -> std::io::Result<()> {
        if message.is_empty() {
            log::trace!("SYNACK succeeded for stream sid={sid}");
            // The Go implementation closes the whole Session on a SYNACK error.
            // That loses unrelated multiplexed streams, so isolate the failure to
            // this stream and keep the Session available for the others.
            if let Some(stream) = self.stream_for_sid(sid).await {
                stream.resolve_handshake(None);
            }
        } else if let Some(stream) = self.remove_stream(sid).await {
            stream.resolve_handshake(Some(format!("remote: {message}")));
            stream
                .close_from_peer(Some(std::io::Error::other(format!("remote: {message}"))))
                .await;
        }
        Ok(())
    }

    async fn release_write_buffering(&self) {
        self.writer_state.set_buffering(false).await;
    }
}

#[cfg(test)]
mod tests {
    use super::Session;
    use crate::runtime::ProtocolHost;
    use bytes::Bytes;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, duplex};
    use tokio::time::timeout;

    fn test_session() -> Session {
        let (io, _peer) = duplex(1024);
        Session::new_with_protocol(
            Box::new(io),
            false,
            None,
            Arc::new(crate::runtime::AnyTlsProtocol),
            crate::core::State::new(crate::core::PaddingFactory::default()),
            crate::runtime::WriterRuntimeState::new(false),
        )
    }

    #[tokio::test]
    async fn streams_route_payloads_by_sid() {
        let session = test_session();
        session.ensure_incoming_stream(1).await.expect("first stream should be created");
        session.ensure_incoming_stream(2).await.expect("second stream should be created");
        session
            .push_stream_data(1, Bytes::from_static(b"one"))
            .await
            .expect("first payload should route");
        session
            .push_stream_data(2, Bytes::from_static(b"two"))
            .await
            .expect("second payload should route");

        let first = session.stream_for_sid(1).await.expect("first stream should exist");
        let second = session.stream_for_sid(2).await.expect("second stream should exist");
        let mut first_buf = [0_u8; 3];
        let mut second_buf = [0_u8; 3];
        assert_eq!(first.read(&mut first_buf).await.expect("first read should succeed"), 3);
        assert_eq!(second.read(&mut second_buf).await.expect("second read should succeed"), 3);
        assert_eq!(&first_buf, b"one");
        assert_eq!(&second_buf, b"two");
    }

    #[tokio::test]
    async fn stream_fin_drains_payload_queued_before_fin() {
        let session = test_session();
        session.ensure_incoming_stream(7).await.expect("stream should be created");
        let stream = session.stream_for_sid(7).await.expect("stream should exist");
        session
            .push_stream_data(7, Bytes::from_static(b"payload"))
            .await
            .expect("payload should route");
        session.close_logical_stream(7).await.expect("peer FIN should close stream");

        let mut buffer = [0_u8; 16];
        let len = stream.read(&mut buffer).await.expect("queued payload should be readable");
        assert_eq!(&buffer[..len], b"payload");
        assert_eq!(stream.read(&mut buffer).await.expect("FIN should produce EOF"), 0);
        assert!(session.stream_for_sid(7).await.is_none());
    }

    #[tokio::test]
    async fn late_payload_for_closed_stream_does_not_fail_session() {
        let session = test_session();
        session.ensure_incoming_stream(7).await.expect("stream should be created");
        session.close_logical_stream(7).await.expect("peer FIN should close stream");

        session
            .push_stream_data(7, Bytes::from_static(b"late payload"))
            .await
            .expect("late payload should be ignored");
    }

    #[tokio::test]
    async fn stream_handshake_failure_does_not_terminate_session() {
        let session = test_session();
        session.ensure_incoming_stream(2).await.expect("first stream should be created");
        session.ensure_incoming_stream(3).await.expect("second stream should be created");

        session
            .resolve_stream_handshake(2, "upstream refused".to_string())
            .await
            .expect("handshake failure should be routed");

        assert!(!session.is_terminated().await);
        assert!(session.stream_for_sid(2).await.is_none());
        assert!(session.stream_for_sid(3).await.is_some());
    }

    #[tokio::test]
    async fn idle_waiter_observes_last_stream_closure() {
        let session = test_session();
        session.ensure_incoming_stream(7).await.expect("stream should be created");
        let wait_for_idle = session.wait_for_idle();
        session.close_logical_stream(7).await.expect("peer FIN should close stream");
        timeout(Duration::from_secs(1), wait_for_idle)
            .await
            .expect("idle state should be observed without a missed notification");
    }

    #[tokio::test]
    async fn terminate_wakes_blocked_run_loop() {
        let (io, _peer) = duplex(1024);
        let session = Arc::new(Session::new_with_protocol(
            Box::new(io),
            false,
            None,
            Arc::new(crate::runtime::AnyTlsProtocol),
            crate::core::State::new(crate::core::PaddingFactory::default()),
            crate::runtime::WriterRuntimeState::new(false),
        ));
        let run_session = session.clone();
        let run_task = tokio::spawn(async move { run_session.run().await });

        tokio::task::yield_now().await;
        session.terminate().await.expect("session should terminate");
        let result = timeout(Duration::from_secs(1), run_task)
            .await
            .expect("terminating a session should stop its run task")
            .expect("run task should join");
        assert!(result.is_err(), "terminated session run loop should return an error");
    }

    #[tokio::test]
    async fn writer_failure_marks_session_terminated() {
        let (io, peer) = duplex(1024);
        drop(peer);
        let session = Session::new_with_protocol(
            Box::new(io),
            true,
            None,
            Arc::new(crate::runtime::AnyTlsProtocol),
            crate::core::State::new(crate::core::PaddingFactory::default()),
            crate::runtime::WriterRuntimeState::new(true),
        );

        session
            .write_frame(crate::core::Frame::new(crate::core::Command::Waste, 0))
            .await
            .expect("frame should be queued before writer observes the closed peer");

        timeout(Duration::from_secs(1), async {
            loop {
                if session.is_terminated().await {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer failure should terminate the session");
    }

    #[tokio::test]
    async fn stream_limit_rejects_extra_streams() {
        let (io, mut peer) = duplex(1024);
        tokio::spawn(async move {
            let mut buffer = [0_u8; 1024];
            while peer.read(&mut buffer).await.is_ok() {}
        });
        let session = Session::new_with_protocol(
            Box::new(io),
            false,
            None,
            Arc::new(crate::runtime::AnyTlsProtocol),
            crate::core::State::new(crate::core::PaddingFactory::default()),
            crate::runtime::WriterRuntimeState::new(false),
        );
        session.open_stream(1).await.expect("first stream should open");
        let error = match session.open_stream(1).await {
            Ok(_) => panic!("second stream should exceed the limit"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    }

    #[tokio::test]
    async fn incoming_stream_limit_rejects_new_sid() {
        let session = test_session();
        session.set_max_incoming_streams(1);
        session.ensure_incoming_stream(1).await.expect("first incoming stream should open");

        let error = session
            .ensure_incoming_stream(2)
            .await
            .expect_err("incoming stream limit should reject a new SID");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    }

    #[tokio::test]
    async fn duplicate_incoming_sid_is_rejected() {
        let session = test_session();
        session.ensure_incoming_stream(1).await.expect("first incoming stream should open");

        let error = session
            .ensure_incoming_stream(1)
            .await
            .expect_err("duplicate incoming SID should be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    }
}
