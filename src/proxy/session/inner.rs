use crate::AsyncReadWrite;
use crate::core::{Command, Frame, HEADER_OVERHEAD_SIZE, State};
use crate::proxy::session::Stream;
use crate::runtime::{
    DataWrite, FrameWrite, MAX_QUEUED_CONTROL_BYTES, MAX_QUEUED_FRAME_BYTES, MAX_QUEUED_INBOUND_BYTES, Protocol, ProtocolHost,
    WriterRuntimeState, spawn_data_scheduler,
};
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, Semaphore, mpsc::Sender, watch};
use tokio_util::sync::CancellationToken;

static SESSION_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionPhase {
    Created = 0,
    Running = 1,
    Terminating = 2,
    Terminated = 3,
}

impl From<SessionPhase> for u8 {
    fn from(phase: SessionPhase) -> Self {
        phase as u8
    }
}

impl TryFrom<u8> for SessionPhase {
    type Error = std::io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        use std::io::{Error, ErrorKind};
        match value {
            0 => Ok(SessionPhase::Created),
            1 => Ok(SessionPhase::Running),
            2 => Ok(SessionPhase::Terminating),
            3 => Ok(SessionPhase::Terminated),
            _ => Err(Error::new(ErrorKind::InvalidData, format!("invalid session phase {value}"))),
        }
    }
}

pub struct Session {
    pub id: u64,
    reader: Mutex<tokio::io::ReadHalf<Box<dyn AsyncReadWrite>>>,
    streams: Arc<Mutex<HashMap<u32, Arc<Stream>>>>,
    next_stream_id: AtomicU32,
    max_incoming_streams: AtomicUsize,
    phase: AtomicU8,
    started: Mutex<bool>,
    pub(crate) is_client: bool,
    pub(crate) protocol_state: Arc<State>,
    writer_state: Arc<WriterRuntimeState>,
    idle_state: Arc<watch::Sender<bool>>,
    close_notify: CancellationToken,
    #[allow(clippy::type_complexity)]
    on_new_stream: Option<Arc<Box<dyn Fn(Arc<Stream>) + Send + Sync>>>,
    protocol: Arc<dyn Protocol>,
    heartbeat_tx: Sender<FrameWrite>,
    control_tx: Sender<FrameWrite>,
    pub(crate) frame_tx: Sender<FrameWrite>,
    data_enqueue_tx: Sender<DataWrite>,
    write_budget: Arc<Semaphore>,
    control_budget: Arc<Semaphore>,
    inbound_budget: Arc<Semaphore>,
    heartbeat_sent: Mutex<Option<tokio::time::Instant>>,
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
        let (heartbeat_tx, heartbeat_rx) = tokio::sync::mpsc::channel::<FrameWrite>(4);
        let (control_tx, control_rx) = tokio::sync::mpsc::channel::<FrameWrite>(32);
        let (frame_tx, data_rx) = tokio::sync::mpsc::channel::<FrameWrite>(100);
        let (data_enqueue_tx, data_enqueue_rx) = tokio::sync::mpsc::channel::<DataWrite>(100);
        spawn_data_scheduler(data_enqueue_rx, frame_tx.clone());
        let write_budget = Arc::new(Semaphore::new(MAX_QUEUED_FRAME_BYTES));
        let control_budget = Arc::new(Semaphore::new(MAX_QUEUED_CONTROL_BYTES));
        let inbound_budget = Arc::new(Semaphore::new(MAX_QUEUED_INBOUND_BYTES));
        let (idle_state, _) = watch::channel(true);
        protocol.spawn_writer_task(
            writer,
            heartbeat_rx,
            control_rx,
            data_rx,
            protocol_state.clone(),
            writer_state.clone(),
        );

        Self {
            id: SESSION_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            reader: Mutex::new(reader),
            streams: Arc::new(Mutex::new(HashMap::new())),
            next_stream_id: AtomicU32::new(0),
            max_incoming_streams: AtomicUsize::new(1024),
            phase: AtomicU8::new(SessionPhase::Created.into()),
            started: Mutex::new(false),
            is_client,
            protocol_state,
            writer_state,
            idle_state: Arc::new(idle_state),
            close_notify: CancellationToken::new(),
            on_new_stream: on_new_stream.map(Arc::new),
            protocol,
            heartbeat_tx,
            control_tx,
            frame_tx,
            data_enqueue_tx,
            write_budget,
            control_budget,
            inbound_budget,
            heartbeat_sent: Mutex::new(None),
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

        if should_start {
            let c0 = SessionPhase::Created.into();
            let new0 = SessionPhase::Running.into();
            if self.phase.compare_exchange(c0, new0, Ordering::AcqRel, Ordering::Acquire).is_err() {
                if self.phase()? == SessionPhase::Created {
                    *self.started.lock().await = false;
                }
                return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed"));
            }
            if let Err(error) = self.protocol.on_session_start(self).await {
                let c1 = SessionPhase::Running.into();
                let new1 = SessionPhase::Created.into();
                if self.phase.compare_exchange(c1, new1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    *self.started.lock().await = false;
                }
                return Err(error);
            }
        }
        Ok(())
    }

    pub async fn run(&self) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind};
        log::debug!("session={} client={} stage=session_run", self.id, self.is_client);
        let writer_failure = self.writer_state.failure_notified();
        let result = tokio::select! {
            biased;
            _ = self.close_notify.cancelled() => Err(Error::new(ErrorKind::BrokenPipe, "Session closed")),
            _ = writer_failure.cancelled() => Err(Error::new(ErrorKind::BrokenPipe, "Session writer failed")),
            result = async {
                self.ensure_started().await?;
                self.recv_loop().await
            } => result,
        };
        let _ = self.terminate_with_error(result.as_ref().err()).await;
        result
    }

    pub async fn open_stream(&self, max_streams: usize) -> std::io::Result<Arc<Stream>> {
        use std::io::{Error, ErrorKind};
        self.ensure_started().await?;
        if self.is_terminated().await {
            return Err(Error::new(ErrorKind::BrokenPipe, "Session closed"));
        }

        let (sid, stream) = {
            let mut streams = self.streams.lock().await;
            if self.phase()? != SessionPhase::Running {
                return Err(Error::new(ErrorKind::BrokenPipe, "Session closed"));
            }
            if streams.len() >= max_streams {
                return Err(Error::new(ErrorKind::WouldBlock, "Session stream limit reached"));
            }

            let sid = self
                .next_stream_id
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| current.checked_add(1))
                .map(|sid| sid + 1)
                .map_err(|_| Error::other("Stream identifier exhausted, please restart your client"))?;

            let stream = Arc::new(self.new_stream(sid));
            streams.insert(sid, stream.clone());
            self.idle_state.send_replace(false);
            (sid, stream)
        };

        log::debug!("session={} stream={sid} stage=syn_submit", self.id);
        if let Err(error) = self.write_frame_sync(Frame::new(Command::Syn, sid)).await {
            self.remove_stream(sid).await;
            stream.close_from_session(Some(Error::other(error.to_string()))).await;
            return Err(error);
        }

        log::debug!("session={} stream={sid} stage=syn_written", self.id);
        Ok(stream)
    }

    pub async fn write_frame(&self, frame: Frame) -> std::io::Result<usize> {
        use std::io::{Error, ErrorKind};
        if matches!(self.phase()?, SessionPhase::Terminating | SessionPhase::Terminated) {
            return Err(Error::new(ErrorKind::BrokenPipe, "Session closed"));
        }
        let len = frame.data.len();
        let budget = if matches!(frame.cmd, Command::Psh) {
            self.acquire_write_budget(len).await?
        } else {
            self.acquire_control_budget(len).await?
        };
        if matches!(frame.cmd, Command::Psh) {
            self.data_enqueue_tx
                .send(DataWrite {
                    sid: frame.sid,
                    frame: FrameWrite::new(frame, None, Some(budget)),
                })
                .await
                .map(|_| len)
                .map_err(|_| Error::new(ErrorKind::BrokenPipe, "Session closed"))
        } else {
            self.control_tx
                .send(FrameWrite::new(frame, None, Some(budget)))
                .await
                .map(|_| len)
                .map_err(|_| Error::new(ErrorKind::BrokenPipe, "Session closed"))
        }
    }

    pub async fn write_frame_sync(&self, frame: Frame) -> std::io::Result<usize> {
        use std::io::{Error, ErrorKind};
        if matches!(self.phase()?, SessionPhase::Terminating | SessionPhase::Terminated) {
            return Err(Error::new(ErrorKind::BrokenPipe, "Session closed"));
        }
        let len = frame.data.len();
        let budget = if matches!(frame.cmd, Command::Psh) {
            self.acquire_write_budget(len).await?
        } else {
            self.acquire_control_budget(len).await?
        };
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        if matches!(frame.cmd, Command::Psh) {
            self.data_enqueue_tx
                .send(DataWrite {
                    sid: frame.sid,
                    frame: FrameWrite::new(frame, Some(ack_tx), Some(budget)),
                })
                .await
                .map_err(|_| Error::new(ErrorKind::BrokenPipe, "Session closed"))
        } else {
            self.control_tx
                .send(FrameWrite::new(frame, Some(ack_tx), Some(budget)))
                .await
                .map_err(|_| Error::new(ErrorKind::BrokenPipe, "Session closed"))
        }?;
        ack_rx.await.map_err(|_| Error::new(ErrorKind::BrokenPipe, "Writer dropped"))??;
        Ok(len)
    }

    pub async fn terminate(&self) -> std::io::Result<()> {
        self.terminate_with_error(None).await
    }

    async fn terminate_with_error(&self, error: Option<&std::io::Error>) -> std::io::Result<()> {
        let mut current: SessionPhase = self.phase.load(Ordering::Acquire).try_into()?;
        loop {
            match current {
                SessionPhase::Created | SessionPhase::Running => {
                    let c = current.into();
                    let new = SessionPhase::Terminating.into();
                    match self.phase.compare_exchange(c, new, Ordering::AcqRel, Ordering::Acquire) {
                        Ok(_) => break,
                        Err(next) => current = next.try_into()?,
                    }
                }
                SessionPhase::Terminating | SessionPhase::Terminated => return Ok(()),
            }
        }

        self.close_notify.cancel();
        self.writer_state.stopped.cancel();
        self.write_budget.close();
        self.control_budget.close();
        self.inbound_budget.close();

        let streams = {
            let mut streams = self.streams.lock().await;
            let stream_list = streams.values().cloned().collect::<Vec<_>>();
            streams.clear();
            self.idle_state.send_replace(true);
            stream_list
        };
        let id = self.id;
        let c = if self.is_client { "client" } else { "server" };
        let len = streams.len();
        if let Some(error) = error {
            if error.kind() == std::io::ErrorKind::UnexpectedEof && streams.is_empty() {
                log::debug!("session={id} {c} side, stage=session_closed active_streams=0 reason={error}");
            } else {
                log::warn!("session={id} {c} side, stage=session_failed active_streams={len} reason={error}");
            }
        } else {
            log::debug!("session={id} {c} side, stage=session_terminated active_streams={len}");
        }
        for stream in streams {
            let reason = error.map(|error| std::io::Error::new(error.kind(), format!("session {} ended: {error}", self.id)));
            stream.close_from_session(reason).await;
        }
        self.phase.store(SessionPhase::Terminated.into(), Ordering::Release);
        Ok(())
    }

    fn phase(&self) -> std::io::Result<SessionPhase> {
        SessionPhase::try_from(self.phase.load(Ordering::Acquire))
    }

    pub async fn is_terminated(&self) -> bool {
        let p = self.phase().unwrap_or(SessionPhase::Terminated);
        matches!(p, SessionPhase::Terminating | SessionPhase::Terminated)
            || self.frame_tx.is_closed()
            || self.control_tx.is_closed()
            || self.writer_state.is_failed()
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
            self.id,
            self.data_enqueue_tx.clone(),
            self.write_budget.clone(),
            self.inbound_budget.clone(),
            Arc::downgrade(&self.streams),
            Arc::downgrade(&self.idle_state),
            self.protocol
                .make_stream_protocol_hooks(self.control_tx.clone(), self.protocol_state.clone()),
        )
    }

    async fn acquire_write_budget(&self, len: usize) -> std::io::Result<tokio::sync::OwnedSemaphorePermit> {
        self.acquire_budget(&self.write_budget, len, MAX_QUEUED_FRAME_BYTES).await
    }

    async fn acquire_control_budget(&self, len: usize) -> std::io::Result<tokio::sync::OwnedSemaphorePermit> {
        self.acquire_budget(&self.control_budget, len, MAX_QUEUED_CONTROL_BYTES).await
    }

    fn try_queue_heartbeat(&self, frame: Frame) -> std::io::Result<bool> {
        use std::io::{Error, ErrorKind};
        if matches!(self.phase()?, SessionPhase::Terminating | SessionPhase::Terminated) {
            return Err(Error::new(ErrorKind::BrokenPipe, "Session closed"));
        }
        match self.heartbeat_tx.try_send(FrameWrite::new(frame, None, None)) {
            Ok(()) => Ok(true),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(false),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(Error::new(ErrorKind::BrokenPipe, "Session closed")),
        }
    }

    async fn acquire_budget(
        &self,
        budget: &Arc<Semaphore>,
        len: usize,
        capacity: usize,
    ) -> std::io::Result<tokio::sync::OwnedSemaphorePermit> {
        let permits = len.clamp(1, capacity) as u32;
        budget
            .clone()
            .acquire_many_owned(permits)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session write budget closed"))
    }

    async fn recv_loop(&self) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind};
        self.ensure_started().await?;
        let mut buffer = vec![0_u8; 4096];
        let mut pending = Vec::new();
        let writer_failure = self.writer_state.failure_notified();
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(30));
        heartbeat.tick().await;

        loop {
            if self.phase()? != SessionPhase::Running {
                return Err(Error::new(ErrorKind::BrokenPipe, "Session closed"));
            }

            let bytes_read = tokio::select! {
                _ = heartbeat.tick() => {
                    let send_heartbeat = {
                        let mut sent = self.heartbeat_sent.lock().await;
                        if sent.is_some_and(|time| time.elapsed() >= std::time::Duration::from_secs(90)) {
                            log::warn!(
                                "session={} client={} stage=heartbeat_timeout elapsed_ms={}",
                                self.id,
                                self.is_client,
                                sent.expect("heartbeat timestamp was checked").elapsed().as_millis()
                            );
                            return Err(Error::new(ErrorKind::TimedOut, "session heartbeat timed out"));
                        }
                        if sent.is_none() {
                            *sent = Some(tokio::time::Instant::now());
                            true
                        } else {
                            false
                        }
                    };
                    if send_heartbeat {
                        log::debug!("session={} client={} stage=heartbeat_request_submit", self.id, self.is_client);
                        if self.try_queue_heartbeat(Frame::new(Command::HeartRequest, 0))? {
                            log::debug!("session={} client={} stage=heartbeat_request_queued", self.id, self.is_client);
                        } else {
                            self.heartbeat_sent.lock().await.take();
                            log::debug!("session={} client={} stage=heartbeat_request_deferred", self.id, self.is_client);
                        }
                    }
                    continue;
                }
                _ = self.close_notify.cancelled() => {
                    return Err(Error::new(ErrorKind::BrokenPipe, "Session closed"));
                }
                _ = writer_failure.cancelled() => {
                    return Err(Error::new(ErrorKind::BrokenPipe, "Session writer failed"));
                }
                result = async {
                    self.reader.lock().await.read(&mut buffer).await
                } => result?,
            };
            if bytes_read == 0 {
                return Err(Error::new(ErrorKind::UnexpectedEof, "Connection closed"));
            }
            pending.extend_from_slice(&buffer[..bytes_read]);

            while let Some(frame) = Frame::from_bytes(&pending) {
                let frame_len = HEADER_OVERHEAD_SIZE + frame.data.len();
                pending.drain(..frame_len);
                let command = frame.cmd;
                let sid = frame.sid;
                let started = tokio::time::Instant::now();
                log::debug!(
                    "session={} stream={sid} stage=frame_received command={command} bytes={}",
                    self.id,
                    frame.data.len()
                );
                let handling = self.protocol.handle_frame(self, frame);
                tokio::pin!(handling);
                match tokio::time::timeout(std::time::Duration::from_secs(1), &mut handling).await {
                    Ok(result) => result?,
                    Err(_) => {
                        log::warn!(
                            "session={} stream={sid} stage=frame_handler_blocked command={command} elapsed_ms={} inbound_available={}",
                            self.id,
                            started.elapsed().as_millis(),
                            self.inbound_budget.available_permits()
                        );
                        handling.await?;
                        log::debug!(
                            "session={} stream={sid} stage=frame_handler_resumed elapsed_ms={}",
                            self.id,
                            started.elapsed().as_millis()
                        );
                    }
                }
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
        use std::io::{Error, ErrorKind};
        if sid == 0 || self.is_terminated().await {
            return Ok(None);
        }

        let stream = {
            let mut streams = self.streams.lock().await;
            if streams.contains_key(&sid) {
                return Err(Error::new(ErrorKind::AlreadyExists, "duplicate incoming stream identifier"));
            }
            if streams.len() >= self.max_incoming_streams.load(Ordering::Acquire) {
                return Err(Error::new(ErrorKind::WouldBlock, "session incoming stream limit reached"));
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

    fn try_send_heartbeat(&self, frame: Frame) -> std::io::Result<bool> {
        self.try_queue_heartbeat(frame)
    }

    async fn send_frame_sync(&self, frame: Frame) -> std::io::Result<usize> {
        self.write_frame_sync(frame).await
    }

    async fn push_stream_data(&self, sid: u32, data: Bytes) -> std::io::Result<()> {
        if let Some(stream) = self.stream_for_sid(sid).await {
            if stream.is_read_closed() {
                log::debug!("Ignoring payload for locally closed stream sid={sid}");
                return Ok(());
            }
            if let Err(error) = stream.push_data(data.as_ref()).await {
                if stream.is_read_closed() {
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
        if let Some(stream) = self.stream_for_sid(sid).await {
            stream.close_from_peer(None).await;
        }
        Ok(())
    }

    async fn terminate_session(&self, sid: u32, msg: Option<String>) -> std::io::Result<()> {
        use std::io::Error;
        if let Some(stream) = self.remove_stream(sid).await {
            stream.close_from_peer(msg.map(|msg| Error::other(format!("remote: {msg}")))).await;
        }
        Ok(())
    }

    async fn resolve_stream_handshake(&self, sid: u32, message: String) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind::InvalidData};
        if sid == 0 {
            return Err(Error::new(InvalidData, "SYNACK cannot use control sid 0"));
        }
        let Some(stream) = self.stream_for_sid(sid).await else {
            if self.is_client && sid <= self.next_stream_id.load(Ordering::Relaxed) {
                log::debug!("Ignoring late SYNACK for closed stream sid={sid}");
                return Ok(());
            }
            return Err(Error::new(InvalidData, "SYNACK for unknown stream"));
        };
        if message.is_empty() {
            log::trace!("SYNACK succeeded for stream sid={sid}");
            // The Go implementation closes the whole Session on a SYNACK error.
            // That loses unrelated multiplexed streams, so isolate the failure to
            // this stream and keep the Session available for the others.
            if !stream.resolve_handshake(None) {
                return Err(Error::new(InvalidData, "duplicate SYNACK"));
            }
        } else {
            if !stream.resolve_handshake(Some(format!("remote: {message}"))) {
                return Err(Error::new(InvalidData, "duplicate SYNACK"));
            }
            self.remove_stream(sid).await;
            stream.close_from_session(Some(Error::other(format!("remote: {message}")))).await;
        }
        Ok(())
    }

    async fn release_write_buffering(&self) {
        self.writer_state.set_buffering(false).await;
    }

    async fn heartbeat_response(&self) {
        let elapsed = self.heartbeat_sent.lock().await.take().map(|sent| sent.elapsed().as_millis());
        log::debug!(
            "session={} client={} stage=heartbeat_response_received elapsed_ms={:?}",
            self.id,
            self.is_client,
            elapsed
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{Session, SessionPhase};
    use crate::core::{Command, Frame};
    use crate::runtime::ProtocolHost;
    use bytes::Bytes;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
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
    async fn session_lifecycle_transitions_are_explicit() {
        let session = test_session();
        assert_eq!(session.phase().unwrap(), SessionPhase::Created);

        session.ensure_started().await.expect("session should start");
        assert_eq!(session.phase().unwrap(), SessionPhase::Running);

        session.terminate().await.expect("session should terminate");
        assert_eq!(session.phase().unwrap(), SessionPhase::Terminated);
        assert!(session.open_stream(1).await.is_err());
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
        assert!(
            session.stream_for_sid(7).await.is_some(),
            "peer FIN should only close the read direction"
        );
        stream.close().await.expect("local close should finish the stream");
        assert!(session.stream_for_sid(7).await.is_none());
    }

    #[tokio::test]
    async fn recv_loop_preserves_payload_order_before_fin() {
        let (io, mut peer) = duplex(4096);
        let session = Session::new_with_protocol(
            Box::new(io),
            false,
            None,
            Arc::new(crate::runtime::AnyTlsProtocol),
            crate::core::State::new(crate::core::PaddingFactory::default()),
            crate::runtime::WriterRuntimeState::new(false),
        );
        for sid in [1, 2] {
            session.ensure_incoming_stream(sid).await.unwrap();
        }
        let mut wire = Vec::new();
        for index in 0..48_u8 {
            for sid in [1, 2] {
                let frame = Frame::with_data(Command::Psh, sid, Bytes::from(vec![sid as u8, index]));
                wire.extend_from_slice(&frame.to_bytes().unwrap());
            }
        }
        for sid in [1, 2] {
            wire.extend_from_slice(&Frame::new(Command::Fin, sid).to_bytes().unwrap());
        }
        peer.write_all(&wire).await.unwrap();
        peer.shutdown().await.unwrap();
        let error = timeout(Duration::from_secs(1), session.recv_loop()).await.unwrap().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        for sid in [1, 2] {
            let stream = session.stream_for_sid(sid).await.unwrap();
            let mut received = Vec::new();
            timeout(Duration::from_secs(1), async {
                let mut buffer = [0; 16];
                loop {
                    let count = stream.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    received.extend_from_slice(&buffer[..count]);
                }
            })
            .await
            .unwrap();
            let expected: Vec<u8> = (0..48_u8).flat_map(|index| [sid as u8, index]).collect();
            assert_eq!(received, expected);
        }
    }

    #[tokio::test]
    async fn recv_loop_rejects_payload_on_control_sid() {
        let (io, mut peer) = duplex(1024);
        let session = Session::new_with_protocol(
            Box::new(io),
            false,
            None,
            Arc::new(crate::runtime::AnyTlsProtocol),
            crate::core::State::new(crate::core::PaddingFactory::default()),
            crate::runtime::WriterRuntimeState::new(false),
        );
        let frame = Frame::with_data(Command::Psh, 0, Bytes::from_static(b"invalid"));
        peer.write_all(&frame.to_bytes().unwrap()).await.unwrap();
        peer.shutdown().await.unwrap();
        let error = timeout(Duration::from_secs(1), session.recv_loop()).await.unwrap().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
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
    async fn late_synack_for_cancelled_stream_preserves_reused_session() {
        for message in ["", "upstream timed out"] {
            let (io, _peer) = duplex(1024);
            let session = Session::new_with_protocol(
                Box::new(io),
                true,
                None,
                Arc::new(crate::runtime::AnyTlsProtocol),
                crate::core::State::new(crate::core::PaddingFactory::default()),
                crate::runtime::WriterRuntimeState::new(false),
            );
            let cancelled = session.open_stream(1).await.expect("first stream should open");
            cancelled.terminate().await.expect("timed out stream should close");
            let active = session.open_stream(1).await.expect("session should be reusable");

            session
                .protocol
                .handle_frame(
                    &session,
                    crate::core::Frame::with_data(
                        crate::core::Command::SynAck,
                        cancelled.id(),
                        Bytes::copy_from_slice(message.as_bytes()),
                    ),
                )
                .await
                .expect("late SYNACK must not fail the receive loop");

            assert!(!session.is_terminated().await);
            assert!(session.stream_for_sid(active.id()).await.is_some());
            session
                .resolve_stream_handshake(active.id(), String::new())
                .await
                .expect("new stream handshake should succeed");
            timeout(Duration::from_secs(1), active.wait_for_handshake())
                .await
                .expect("handshake waiter should wake")
                .expect("new stream should remain usable");
        }
    }

    #[tokio::test]
    async fn invalid_synack_is_still_rejected() {
        let (io, _peer) = duplex(1024);
        let session = Session::new_with_protocol(
            Box::new(io),
            true,
            None,
            Arc::new(crate::runtime::AnyTlsProtocol),
            crate::core::State::new(crate::core::PaddingFactory::default()),
            crate::runtime::WriterRuntimeState::new(false),
        );
        let stream = session.open_stream(1).await.expect("stream should open");
        for message in ["", "upstream refused"] {
            for sid in [0, stream.id() + 1] {
                let error = session
                    .resolve_stream_handshake(sid, message.to_string())
                    .await
                    .expect_err("unallocated and control stream IDs must be rejected");
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            }
        }
        session
            .resolve_stream_handshake(stream.id(), String::new())
            .await
            .expect("first SYNACK should succeed");
        for message in ["", "upstream refused"] {
            let error = session
                .resolve_stream_handshake(stream.id(), message.to_string())
                .await
                .expect_err("duplicate SYNACK must be rejected");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        }
    }

    #[tokio::test]
    async fn full_stream_closure_does_not_stall_other_stream_handshakes() {
        for local_close in [false, true] {
            let (io, mut peer) = duplex(4096);
            let session = Arc::new(Session::new_with_protocol(
                Box::new(io),
                true,
                None,
                Arc::new(crate::runtime::AnyTlsProtocol),
                crate::core::State::new(crate::core::PaddingFactory::default()),
                crate::runtime::WriterRuntimeState::new(false),
            ));
            let slow = session.open_stream(2).await.unwrap();
            let pending = session.open_stream(2).await.unwrap();
            for _ in 0..64 {
                session.push_stream_data(slow.id(), Bytes::from_static(b"queued")).await.unwrap();
            }
            let frame = if local_close {
                Frame::with_data(Command::Psh, slow.id(), Bytes::from_static(b"blocked"))
            } else {
                Frame::new(Command::Fin, slow.id())
            };
            peer.write_all(&frame.to_bytes().unwrap()).await.unwrap();
            peer.write_all(&Frame::new(Command::SynAck, pending.id()).to_bytes().unwrap())
                .await
                .unwrap();
            let running = session.clone();
            let task = tokio::spawn(async move { running.run().await });

            // A full or blocked sibling stream must never delay this stream's
            // SYNACK: inbound delivery is decoupled from the receive loop, so the
            // handshake resolves promptly without first closing the slow stream.
            timeout(Duration::from_secs(1), pending.wait_for_handshake())
                .await
                .expect("a full sibling stream must not stall the next SYNACK")
                .expect("other stream handshake must succeed");
            if local_close {
                timeout(Duration::from_secs(1), slow.close()).await.unwrap().unwrap();
            }
            assert!(!session.is_terminated().await);
            timeout(Duration::from_secs(1), session.terminate())
                .await
                .expect("session termination must not wait for a full stream queue")
                .unwrap();
            assert!(timeout(Duration::from_secs(1), task).await.unwrap().unwrap().is_err());
        }
    }

    #[tokio::test]
    async fn blocked_stream_data_does_not_stall_control_frames() {
        let (io, mut peer) = duplex(4096);
        let session = Arc::new(Session::new_with_protocol(
            Box::new(io),
            true,
            None,
            Arc::new(crate::runtime::AnyTlsProtocol),
            crate::core::State::new(crate::core::PaddingFactory::default()),
            crate::runtime::WriterRuntimeState::new(false),
        ));
        let slow = session.open_stream(2).await.unwrap();
        let pending = session.open_stream(2).await.unwrap();
        let held = session
            .inbound_budget
            .clone()
            .acquire_many_owned(crate::runtime::MAX_QUEUED_INBOUND_BYTES as u32)
            .await
            .unwrap();

        peer.write_all(
            &Frame::with_data(Command::Psh, slow.id(), Bytes::from_static(b"blocked"))
                .to_bytes()
                .unwrap(),
        )
        .await
        .unwrap();
        peer.write_all(&Frame::new(Command::SynAck, pending.id()).to_bytes().unwrap())
            .await
            .unwrap();
        let running = session.clone();
        let task = tokio::spawn(async move { running.run().await });

        timeout(Duration::from_secs(1), pending.wait_for_handshake())
            .await
            .expect("control frame must not wait for slow stream data")
            .expect("other stream handshake must succeed");
        assert!(!session.is_terminated().await);

        drop(held);
        session.terminate().await.unwrap();
        timeout(Duration::from_secs(1), task).await.unwrap().unwrap().unwrap_err();
    }

    #[tokio::test]
    async fn session_failure_reason_reaches_pending_handshake() {
        for invalid_frame in [false, true] {
            let (io, mut peer) = duplex(4096);
            let session = Session::new_with_protocol(
                Box::new(io),
                true,
                None,
                Arc::new(crate::runtime::AnyTlsProtocol),
                crate::core::State::new(crate::core::PaddingFactory::default()),
                crate::runtime::WriterRuntimeState::new(false),
            );
            let stream = session.open_stream(1).await.unwrap();
            assert_eq!(stream.session_id(), session.id);
            if invalid_frame {
                peer.write_all(&Frame::new(Command::SynAck, 0).to_bytes().unwrap()).await.unwrap();
            } else {
                peer.shutdown().await.unwrap();
            }
            let reason = timeout(Duration::from_secs(1), session.run()).await.unwrap().unwrap_err();
            let failure = timeout(Duration::from_secs(1), stream.wait_for_handshake())
                .await
                .unwrap()
                .unwrap_err();
            assert!(failure.to_string().contains(&reason.to_string()));
            assert!(failure.to_string().contains(&format!("session {} ended", session.id)));
            assert!(session.is_terminated().await);
        }
    }

    #[tokio::test]
    async fn idle_waiter_observes_last_stream_closure() {
        let session = test_session();
        session.ensure_incoming_stream(7).await.expect("stream should be created");
        let stream = session.stream_for_sid(7).await.expect("stream should exist");
        let wait_for_idle = session.wait_for_idle();
        session.close_logical_stream(7).await.expect("peer FIN should close stream");
        assert!(!stream.is_write_closed(), "peer FIN must preserve the local write direction");
        stream.close().await.expect("local close should finish the stream");
        timeout(Duration::from_secs(1), wait_for_idle)
            .await
            .expect("idle state should be observed without a missed notification");
    }

    #[tokio::test]
    async fn terminate_interrupts_full_inbound_delivery() {
        let (io, mut peer) = duplex(4096);
        let session = Arc::new(Session::new_with_protocol(
            Box::new(io),
            false,
            None,
            Arc::new(crate::runtime::AnyTlsProtocol),
            crate::core::State::new(crate::core::PaddingFactory::default()),
            crate::runtime::WriterRuntimeState::new(false),
        ));
        session.ensure_incoming_stream(1).await.unwrap();
        for _ in 0..64 {
            session.push_stream_data(1, Bytes::from_static(b"queued")).await.unwrap();
        }
        peer.write_all(
            &Frame::with_data(Command::Psh, 1, Bytes::from_static(b"blocked"))
                .to_bytes()
                .unwrap(),
        )
        .await
        .unwrap();
        let running = session.clone();
        let task = tokio::spawn(async move { running.run().await });
        timeout(Duration::from_secs(1), async {
            while session.inbound_budget.available_permits() != crate::runtime::MAX_QUEUED_INBOUND_BYTES - 64 * 6 - 7 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        session.terminate().await.unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .expect("termination must interrupt blocked delivery")
            .unwrap()
            .unwrap_err();
        assert_eq!(session.inbound_budget.available_permits(), crate::runtime::MAX_QUEUED_INBOUND_BYTES);
    }

    #[tokio::test]
    async fn stream_close_cancels_wait_for_shared_inbound_budget() {
        let (io, _peer) = duplex(1024);
        let session = Arc::new(Session::new_with_protocol(
            Box::new(io),
            false,
            None,
            Arc::new(crate::runtime::AnyTlsProtocol),
            crate::core::State::new(crate::core::PaddingFactory::default()),
            crate::runtime::WriterRuntimeState::new(false),
        ));
        session.ensure_incoming_stream(1).await.unwrap();
        let stream = session.stream_for_sid(1).await.unwrap();
        let held = session
            .inbound_budget
            .clone()
            .acquire_many_owned(crate::runtime::MAX_QUEUED_INBOUND_BYTES as u32)
            .await
            .unwrap();
        let delivery_session = session.clone();
        let delivering = tokio::spawn(async move { delivery_session.push_stream_data(1, Bytes::from_static(b"pending")).await });
        tokio::task::yield_now().await;
        stream.close().await.unwrap();
        timeout(Duration::from_secs(1), delivering).await.unwrap().unwrap().unwrap();
        assert!(!session.is_terminated().await);
        drop(held);
        session.terminate().await.unwrap();
    }

    #[tokio::test]
    async fn terminate_stops_blocked_writer_and_budget_waiters() {
        let (io, mut peer) = duplex(1);
        let session = Arc::new(Session::new_with_protocol(
            Box::new(io),
            false,
            None,
            Arc::new(crate::runtime::AnyTlsProtocol),
            crate::core::State::new(crate::core::PaddingFactory::default()),
            crate::runtime::WriterRuntimeState::new(false),
        ));
        session.ensure_incoming_stream(1).await.unwrap();
        let stream = session.stream_for_sid(1).await.unwrap();
        let held = session
            .write_budget
            .clone()
            .acquire_many_owned(crate::runtime::MAX_QUEUED_FRAME_BYTES as u32)
            .await
            .unwrap();
        let writing_stream = stream.clone();
        let waiting = tokio::spawn(async move { writing_stream.write(b"waiting for budget").await });
        let writing_session = session.clone();
        let writing = tokio::spawn(async move {
            writing_session
                .write_frame_sync(Frame::with_data(Command::Waste, 0, Bytes::from(vec![0; 4096])))
                .await
        });
        let mut first = [0; 1];
        timeout(Duration::from_secs(1), peer.read_exact(&mut first)).await.unwrap().unwrap();
        session.terminate().await.unwrap();
        timeout(Duration::from_secs(1), waiting).await.unwrap().unwrap().unwrap_err();
        timeout(Duration::from_secs(1), writing).await.unwrap().unwrap().unwrap_err();
        timeout(Duration::from_secs(1), session.data_enqueue_tx.closed())
            .await
            .expect("scheduler must stop without new input");
        let mut remaining = Vec::new();
        timeout(Duration::from_secs(1), peer.read_to_end(&mut remaining))
            .await
            .expect("writer must shut down the transport")
            .unwrap();
        drop(held);
        assert!(stream.is_closed());
    }

    #[tokio::test]
    async fn writer_failure_before_run_is_observed() {
        let (io, _peer) = duplex(1024);
        let session = Session::new_with_protocol(
            Box::new(io),
            false,
            None,
            Arc::new(crate::runtime::AnyTlsProtocol),
            crate::core::State::new(crate::core::PaddingFactory::default()),
            crate::runtime::WriterRuntimeState::new(false),
        );
        session.writer_state.mark_failed();
        let error = timeout(Duration::from_secs(1), session.run())
            .await
            .expect("failure must persist until observed")
            .unwrap_err();
        assert!(error.to_string().contains("writer"));
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
