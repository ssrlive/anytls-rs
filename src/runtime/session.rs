use method_name::method_name_unstable;
use std::{
    collections::HashMap,
    sync::{
        Arc, Weak,
        atomic::{AtomicU32, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, Notify, RwLock, mpsc, oneshot},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

use crate::{CHECK_MARK, Command, Frame, HEADER_OVERHEAD_SIZE, PaddingFactory, from_bytes, runtime::BoxTransport, to_bytes};

const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_MAX_SESSION_AGE: Duration = Duration::from_secs(60 * 60);

static NEXT_STREAM_ID: AtomicU32 = AtomicU32::new(0);

fn allocate_stream_id(counter: &AtomicU32) -> u32 {
    loop {
        let stream_id = counter.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if stream_id != 0 {
            return stream_id;
        }
    }
}

pub fn is_peer_disconnect(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::ConnectionReset
}

struct WriteRequest {
    bytes: Vec<u8>,
    disable_buffering_after: bool,
    result_sender: Option<oneshot::Sender<std::io::Result<()>>>,
}

/// Represents an status of a logical stream within a session.
pub(crate) struct StreamState {
    pub(crate) writer: Arc<Mutex<tokio::io::DuplexStream>>,
    pub(crate) push_sender: mpsc::UnboundedSender<Option<Vec<u8>>>,
    /// Indicates whether the local side has sent a FIN for this stream.
    pub(crate) local_fin: bool,
    /// Indicates whether the remote side has sent a FIN for this stream.
    pub(crate) remote_fin: bool,
    /// Indicates whether all queued inbound data has drained to the application.
    pub(crate) read_closed: bool,
    /// Used to notify an active-close task that is waiting for a FIN response.
    pub(crate) close_token: CancellationToken,
}

impl StreamState {
    pub(crate) fn new(writer: tokio::io::DuplexStream, stream_id: u32, session: Weak<Session>) -> Self {
        let (push_sender, push_receiver) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
        let writer = Arc::new(Mutex::new(writer));
        let task_writer = Arc::clone(&writer);
        let close_token = CancellationToken::new();
        let task_close_token = close_token.clone();
        let session_id = session.upgrade().map(|s| s.id()).unwrap_or_default();
        tokio::spawn(Self::background_task_for_pump_data_to_stream(
            stream_id,
            session_id,
            session,
            task_close_token,
            push_receiver,
            task_writer,
        ));
        Self {
            writer,
            push_sender,
            local_fin: false,
            remote_fin: false,
            read_closed: false,
            close_token,
        }
    }

    async fn background_task_for_pump_data_to_stream(
        stream_id: u32,
        session_id: usize,
        session: Weak<Session>,
        task_close_token: CancellationToken,
        mut push_receiver: mpsc::UnboundedReceiver<Option<Vec<u8>>>,
        task_writer: Arc<Mutex<tokio::io::DuplexStream>>,
    ) {
        let result = loop {
            let message = tokio::select! {
                message = push_receiver.recv() => message,
                _ = task_close_token.cancelled() => break Ok(()),
            };
            let Some(message) = message else {
                break Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "push_receiver closed"));
            };
            let Some(data) = message else {
                let _ = task_writer.lock().await.shutdown().await;
                if let Some(session) = session.upgrade() {
                    session.on_stream_read_closed(stream_id).await;
                }
                break Ok(());
            };
            let result = tokio::select! {
                result = async { task_writer.lock().await.write_all(&data).await } => result,
                _ = task_close_token.cancelled() => break Ok(()),
            };
            if let Err(e) = result {
                break Err(e);
            }
        };
        let mn = method_name_unstable!();
        if let Err(e) = result {
            log::warn!("{mn} -- Session {session_id} stream {stream_id} task encountered an error: {e}");
        } else {
            log::trace!("{mn} -- Session {session_id} stream {stream_id} task completed successfully");
        }
    }
}

pub struct Session {
    session_id: usize,
    created_at: std::time::Instant,
    max_age: Duration,
    max_streams: usize,
    write_sender: mpsc::Sender<WriteRequest>,
    streams: Mutex<HashMap<u32, StreamState>>,
    /// Indicates whether the session has been closed.
    closed: Arc<std::sync::atomic::AtomicBool>,
    is_client: bool,
    peer_version: std::sync::atomic::AtomicU8,
    received_settings: std::sync::atomic::AtomicBool,
    padding: Arc<RwLock<PaddingFactory>>,
    reader: Mutex<Option<tokio::io::ReadHalf<BoxTransport>>>,
    close_token: CancellationToken,
    heart_response: Notify,
    incoming: Mutex<Option<mpsc::Receiver<Stream>>>,
    incoming_sender: Option<mpsc::Sender<Stream>>,
    idle_sender: Mutex<Option<mpsc::UnboundedSender<Arc<Session>>>>,
}

impl Session {
    pub fn new_client(
        session_id: usize,
        transport: BoxTransport,
        padding: Arc<RwLock<PaddingFactory>>,
        max_streams: usize,
        max_age: Duration,
    ) -> Arc<Self> {
        Arc::new(Self::new(session_id, transport, padding, true, max_streams, max_age, None))
    }

    pub fn new_server(
        session_id: usize,
        transport: BoxTransport,
        padding: Arc<RwLock<PaddingFactory>>,
        max_streams: usize,
        max_age: Duration,
    ) -> Arc<Self> {
        let incoming = Some(mpsc::channel(32));
        Arc::new(Self::new(session_id, transport, padding, false, max_streams, max_age, incoming))
    }

    fn new(
        session_id: usize,
        transport: BoxTransport,
        padding: Arc<RwLock<PaddingFactory>>,
        is_client: bool,
        max_streams: usize,
        max_age: Duration,
        incoming: Option<(mpsc::Sender<Stream>, mpsc::Receiver<Stream>)>,
    ) -> Self {
        let side = if is_client { "client" } else { "server" };
        log::trace!("{} -- Creating {side} session {session_id}", method_name_unstable!());
        let (reader, writer) = tokio::io::split(transport);
        let incoming_sender = incoming.as_ref().map(|(sender, _)| sender.clone());
        let close_token = CancellationToken::new();
        let writer_close_token = close_token.clone();
        let packet_counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let writer_packet_counter = Arc::clone(&packet_counter);
        let (write_sender, write_receiver) = mpsc::channel::<WriteRequest>(64);
        let writer_padding = Arc::clone(&padding);
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer_closed = Arc::clone(&closed);
        tokio::spawn(Self::background_task_for_session(
            writer,
            is_client,
            writer_packet_counter,
            write_receiver,
            writer_close_token,
            writer_closed,
            writer_padding,
        ));
        Self {
            session_id,
            created_at: std::time::Instant::now(),
            max_age,
            max_streams: max_streams.max(1),
            write_sender,
            streams: Mutex::new(HashMap::new()),
            closed,
            is_client,
            peer_version: std::sync::atomic::AtomicU8::new(0),
            received_settings: std::sync::atomic::AtomicBool::new(false),
            padding,
            reader: Mutex::new(Some(reader)),
            close_token,
            heart_response: Notify::new(),
            incoming: Mutex::new(incoming.map(|(_, receiver)| receiver)),
            incoming_sender,
            idle_sender: Mutex::new(None),
        }
    }

    async fn background_task_for_session(
        writer: tokio::io::WriteHalf<BoxTransport>,
        buffering: bool,
        writer_packet_counter: Arc<std::sync::atomic::AtomicU32>,
        mut write_receiver: mpsc::Receiver<WriteRequest>,
        writer_close_token: CancellationToken,
        writer_closed: Arc<std::sync::atomic::AtomicBool>,
        writer_padding: Arc<RwLock<PaddingFactory>>,
    ) {
        let mut writer = Writer {
            transport: writer,
            buffering,
            buffered: Vec::new(),
            send_padding: true,
            packet_counter: writer_packet_counter,
        };
        loop {
            let request = tokio::select! {
                request = write_receiver.recv() => request,
                _ = writer_close_token.cancelled() => None,
            };
            let Some(request) = request else {
                break;
            };
            let result = tokio::select! {
                biased;
                _ = writer_close_token.cancelled() => break,
                result = tokio::time::timeout(WRITE_TIMEOUT, async {
                    writer.write_conn(request.bytes, &writer_padding).await?;
                    if request.disable_buffering_after {
                        writer.buffering = false;
                        if !writer.buffered.is_empty() {
                            writer.write_conn(Vec::new(), &writer_padding).await?;
                        }
                    }
                    Ok(())
                }) => result.unwrap_or_else(|_| Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "AnyTLS transport write timed out"))),
            };
            let failed = result.is_err();
            if let Some(result_sender) = request.result_sender {
                let _ = result_sender.send(result);
            }
            if failed {
                writer_closed.store(true, std::sync::atomic::Ordering::Release);
                writer_close_token.cancel();
                break;
            }
        }
    }

    pub fn id(&self) -> usize {
        self.session_id
    }

    pub fn is_expired(&self) -> bool {
        !self.max_age.is_zero() && self.created_at.elapsed() >= self.max_age
    }

    #[inline]
    pub async fn has_stream_capacity(&self) -> bool {
        self.streams.lock().await.len() < self.max_streams
    }

    pub(crate) async fn is_idle(&self) -> bool {
        self.streams.lock().await.is_empty()
    }

    pub async fn set_idle_sender(&self, sender: mpsc::UnboundedSender<Arc<Session>>) {
        // The client pool installs this before the session is exposed.
        *self.idle_sender.lock().await = Some(sender);
    }

    pub async fn run(self: &Arc<Self>) -> std::io::Result<()> {
        if self.is_client {
            let padding = self.padding.read().await;
            let mut settings = HashMap::new();
            settings.insert("v".to_owned(), "2".to_owned());
            settings.insert("client".to_owned(), crate::PROGRAM_VERSION_NAME.to_owned());
            settings.insert("padding-md5".to_owned(), padding.md5.clone());
            drop(padding);
            let mut frame = Frame::new(Command::Settings, 0);
            frame.data = to_bytes(&settings);
            self.write_control(frame).await?;
        }

        let session = Arc::clone(self);
        let session_id = self.id();
        tokio::spawn(Self::do_main_loop_task(session, session_id));
        if self.is_client {
            let session = Arc::downgrade(self);
            let close_token = self.close_token.clone();
            tokio::spawn(Session::do_heartbeat_task(session, session_id, close_token));
        }
        if !self.max_age.is_zero() {
            let session = Arc::downgrade(self);
            let close_token = self.close_token.clone();
            let remaining = self.max_age.saturating_sub(self.created_at.elapsed());
            tokio::spawn(Self::do_wait_expiration(session, session_id, close_token, remaining));
        }
        Ok(())
    }

    async fn do_main_loop_task(session: Arc<Session>, session_id: usize) {
        let mn = method_name_unstable!();
        let result = Arc::clone(&session).receive_loop().await;
        match result {
            Ok(()) => log::debug!("{mn} -- Session {session_id} receive loop stopped",),
            Err(error) => log::warn!("{mn} -- Session {session_id} receive loop failed: {error}"),
        }
        let _ = session.shutdown().await;
        log::debug!("{mn} -- Session {session_id} shut down");
    }

    async fn do_heartbeat_task(session: Weak<Session>, session_id: usize, close_token: CancellationToken) {
        let mn = method_name_unstable!();
        let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = close_token.cancelled() => break,
                _ = ticker.tick() => {}
            }
            let Some(session) = session.upgrade() else {
                break;
            };
            if session.peer_version.load(std::sync::atomic::Ordering::Acquire) < 2 {
                continue;
            }

            let len = session.streams.lock().await.len();
            log::trace!("{mn} -- Session {session_id} sending heartbeat, total {len} streams");

            let response = session.heart_response.notified();
            tokio::pin!(response);
            response.as_mut().enable();
            let probe = async {
                session.write_control(Frame::new(Command::HeartRequest, 0)).await?;
                response.await;
                Ok::<(), std::io::Error>(())
            };
            let result = tokio::select! {
                _ = close_token.cancelled() => break,
                result = tokio::time::timeout(HEARTBEAT_TIMEOUT, probe) => result,
            };
            if !matches!(result, Ok(Ok(()))) {
                log::warn!("{mn} -- Session {session_id} heartbeat failed or timed out; closing stalled transport");
                let _ = session.shutdown().await;
                break;
            }
        }
    }

    async fn do_wait_expiration(session: Weak<Session>, session_id: usize, close_token: CancellationToken, remaining: Duration) {
        tokio::select! {
            _ = close_token.cancelled() => return,
            _ = tokio::time::sleep(remaining) => {}
        }
        if let Some(session) = session.upgrade()
            && session.is_idle().await
        {
            let mn = method_name_unstable!();
            log::debug!("{mn} -- Session {session_id} reached its maximum age while idle; closing");
            let _ = session.shutdown().await;
        }
    }

    /// Accepts an incoming stream from the session.
    /// It's used by the server to receive streams initiated by the client.
    pub async fn accept_stream(&self) -> std::io::Result<Stream> {
        use std::io::{Error, ErrorKind::BrokenPipe, ErrorKind::Unsupported};
        let mut incoming = self.incoming.lock().await;
        let session_id = self.id();
        let receiver = incoming
            .as_mut()
            .ok_or_else(|| Error::new(Unsupported, "client sessions do not accept streams"))?;
        let stream = tokio::select! {
            stream = receiver.recv() => stream.ok_or_else(|| Error::new(BrokenPipe, format!("session {session_id} closed because of remote closure")))?,
            _ = self.close_token.cancelled() => return Err(Error::new(BrokenPipe, format!("session {session_id} closed because of cancellation"))),
        };
        if stream.is_closed() {
            return Err(Error::new(BrokenPipe, format!("session {session_id} stream closed")));
        }
        Ok(stream)
    }

    /// Opens a new outgoing stream in the session.
    /// It's used by the client to initiate streams to the server.
    pub async fn open_stream(self: &Arc<Self>) -> std::io::Result<Stream> {
        let stream = self.reserve_stream().await?;
        self.open_reserved_stream(stream).await
    }

    pub(crate) async fn reserve_stream(self: &Arc<Self>) -> std::io::Result<Stream> {
        use std::io::{Error, ErrorKind::BrokenPipe, ErrorKind::TimedOut, ErrorKind::WouldBlock};
        let session_id = self.id();
        if self.is_closed() {
            return Err(Error::new(BrokenPipe, format!("session {session_id} closed")));
        }
        let id = allocate_stream_id(&NEXT_STREAM_ID);
        let (local, remote) = tokio::io::duplex(64 * 1024);

        {
            let mut streams = self.streams.lock().await;
            // Re-check under the streams lock so we cannot insert after shutdown drained.
            if self.is_closed() {
                return Err(Error::new(BrokenPipe, format!("session {session_id} closed")));
            }
            if self.is_expired() {
                return Err(Error::new(TimedOut, format!("session {session_id} reached its maximum age")));
            }
            if streams.len() >= self.max_streams {
                return Err(Error::new(WouldBlock, format!("session {session_id} stream limit reached")));
            }
            streams.insert(id, StreamState::new(remote, id, Arc::downgrade(self)));
            let mn = method_name_unstable!();
            let l = streams.len();
            log::debug!("{mn} -- session {session_id} reserved stream {id}, total streams: {l}",);
        }
        Ok(Stream::new(id, Arc::downgrade(self), local))
    }

    pub(crate) async fn open_reserved_stream(self: &Arc<Self>, stream: Stream) -> std::io::Result<Stream> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        let session_id = self.id();
        let stream_id = stream.id();
        if let Err(error) = self.enqueue_frame(Frame::new(Command::Syn, stream_id), true).await {
            self.remove_stream_by_id(stream_id).await;
            return Err(error);
        }
        if self.is_closed() {
            self.remove_stream_by_id(stream_id).await;
            return Err(Error::new(BrokenPipe, format!("session {session_id} closed")));
        }

        Ok(stream)
    }

    pub(crate) async fn write_data(&self, stream_id: u32, data: &[u8]) -> std::io::Result<usize> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        let mut frame = Frame::new(Command::Push, stream_id);
        frame.data.extend_from_slice(data);
        let bytes = frame.encode()?;

        let can_write = self.streams.lock().await.get(&stream_id).is_some_and(|stream| !stream.local_fin);
        if !can_write {
            return Err(Error::new(BrokenPipe, format!("stream {stream_id} closed")));
        }
        self.enqueue_bytes(bytes, false).await?;
        Ok(data.len())
    }

    pub(crate) async fn send_fin_by_id(self: &Arc<Self>, stream_id: u32) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        let remote_fin = {
            let mut streams = self.streams.lock().await;
            let stream = streams
                .get_mut(&stream_id)
                .ok_or_else(|| Error::new(BrokenPipe, format!("stream {stream_id} closed")))?;
            if stream.local_fin {
                return Ok(());
            }
            stream.local_fin = true;
            stream.remote_fin
        };
        self.enqueue_fin(stream_id).await?;
        if remote_fin {
            self.finish_stream_if_closed(stream_id).await;
        }
        Ok(())
    }

    async fn enqueue_fin(&self, stream_id: u32) -> std::io::Result<()> {
        self.write_control(Frame::new(Command::Fin, stream_id)).await
    }

    pub(crate) async fn close_stream_by_id(self: &Arc<Self>, stream_id: u32) -> std::io::Result<()> {
        if self.is_closed() {
            return Ok(());
        }
        let result = self.send_fin_by_id(stream_id).await;
        self.remove_stream_by_id(stream_id).await;
        if let Err(error) = result
            && error.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(error);
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> std::io::Result<()> {
        self.closed.store(true, std::sync::atomic::Ordering::Release);
        self.close_token.cancel();
        self.reader.lock().await.take();
        let streams = self.streams.lock().await.drain().map(|(_, entry)| entry).collect::<Vec<_>>();
        for entry in streams {
            entry.close_token.cancel();
        }
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Acquire)
    }

    async fn write_control(&self, frame: Frame) -> std::io::Result<()> {
        self.enqueue_frame(frame, false).await
    }

    fn queue_control(&self, frame: Frame) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind};
        self.write_sender
            .try_send(WriteRequest {
                bytes: frame.encode()?,
                disable_buffering_after: false,
                result_sender: None,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Error::new(ErrorKind::WouldBlock, "AnyTLS control queue is full"),
                mpsc::error::TrySendError::Closed(_) => Error::new(ErrorKind::BrokenPipe, "AnyTLS writer task closed"),
            })
    }

    async fn enqueue_frame(&self, frame: Frame, disable_buffering_after: bool) -> std::io::Result<()> {
        self.enqueue_encoded_with_mode(frame.encode()?, disable_buffering_after).await
    }

    async fn enqueue_bytes(&self, bytes: Vec<u8>, disable_buffering_after: bool) -> std::io::Result<()> {
        self.enqueue_encoded_with_mode(bytes, disable_buffering_after).await
    }

    async fn enqueue_encoded_with_mode(&self, bytes: Vec<u8>, disable_buffering_after: bool) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        let session_id = self.id();
        let (result_sender, result_receiver) = oneshot::channel();
        tokio::select! {
            result = self.write_sender.send(WriteRequest {
                bytes,
                disable_buffering_after,
                result_sender: Some(result_sender),
            }) => result.map_err(|_| Error::new(BrokenPipe, "writer task closed"))?,
            _ = self.close_token.cancelled() => return Err(Error::new(BrokenPipe, format!("session {session_id} closed in enqueue_encoded_with_mode"))),
        }
        tokio::select! {
            result = result_receiver => result.map_err(|_| Error::new(BrokenPipe, "writer task closed"))?,
            _ = self.close_token.cancelled() => Err(Error::new(BrokenPipe, format!("session {session_id} closed in enqueue_encoded_with_mode (result_receiver)"))),
        }
    }

    async fn receive_loop(self: Arc<Self>) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind::InvalidData};
        let session_id = self.id();
        let mn = method_name_unstable!();
        loop {
            let result = tokio::select! {
                result = async {
                    let mut reader = self.reader.lock().await;
                    match reader.as_mut() {
                        Some(reader) => Frame::read_from_or_eof(reader).await,
                        None => Ok(None),
                    }
                } => result,
                _ = self.close_token.cancelled() => break Ok(()),
            };
            let frame = match result {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    log::debug!("{mn} -- Session {session_id} peer closed transport");
                    break Ok(());
                }
                Err(error) if is_peer_disconnect(&error) => {
                    log::debug!("{mn} -- Session {session_id} transport reset: {error}");
                    break Ok(());
                }
                Err(error) => break Err(error),
            };
            let stream_id = frame.stream_id;
            if !frame.command.valid_stream_id(stream_id) {
                break Err(Error::new(InvalidData, "invalid stream id for command"));
            }
            if !frame.command.allows_data() && !frame.data.is_empty() {
                break Err(Error::new(InvalidData, "command must not contain data"));
            }
            match frame.command {
                Command::Push => {
                    let push_sender = self.streams.lock().await.get(&stream_id).map(|entry| entry.push_sender.clone());
                    if let Some(push_sender) = push_sender
                        && let Err(e) = push_sender.send(Some(frame.data))
                    {
                        log::debug!("{mn} -- Session {session_id} stream {stream_id} push worker closed, closing stream: {e}");
                        if !self.is_closed() {
                            self.queue_control(Frame::new(Command::Fin, stream_id))?;
                            self.remove_stream_by_id(stream_id).await;
                        }
                    }
                }
                Command::Waste => {}
                Command::Syn => self.on_receive_syn_cmd(stream_id).await?,
                Command::Fin => self.on_receive_fin_cmd(stream_id).await?,
                Command::Settings => self.on_receive_settings(&frame.data).await?,
                Command::SynAck => {
                    if !frame.data.is_empty() {
                        self.remove_stream_by_id(stream_id).await;
                        let i = String::from_utf8_lossy(&frame.data);
                        log::warn!("{mn} -- Session {session_id} stream {stream_id} received SynAck with unexpected data: {i}");
                    }
                    // TODO: Handle additional SynAck logic if necessary
                }
                Command::HeartRequest => {
                    log::trace!("{mn} -- Session {session_id}: Received HeartRequest");
                    self.queue_control(Frame::new(Command::HeartResponse, stream_id))?;
                }
                Command::HeartResponse => {
                    log::trace!("{mn} -- Session {session_id}: Received HeartResponse");
                    self.heart_response.notify_waiters();
                }
                Command::ServerSettings => {
                    let map = from_bytes(&frame.data);
                    if self.is_client
                        && let Some(version) = map.get("v").and_then(|value| value.parse().ok())
                    {
                        self.peer_version.store(version, std::sync::atomic::Ordering::Release);
                    }
                }
                Command::UpdatePaddingScheme => {
                    if self.is_client
                        && let Some(factory) = PaddingFactory::new(&frame.data)
                    {
                        *self.padding.write().await = factory;
                    }
                }
                Command::Alert => {
                    let info = String::from_utf8_lossy(&frame.data);
                    log::warn!("{mn} -- Session {session_id} received alert: {info}",);
                    break Ok(());
                }
            }
        }
    }

    async fn on_receive_settings(&self, data: &[u8]) -> std::io::Result<()> {
        if self.is_client {
            return Ok(());
        }
        self.received_settings.store(true, std::sync::atomic::Ordering::Release);
        let map = from_bytes(data);
        let update_scheme = {
            let padding = self.padding.read().await;
            (map.get("padding-md5") != Some(&padding.md5)).then(|| padding.raw_scheme().to_vec())
        };
        if let Some(scheme) = update_scheme {
            let mut update = Frame::new(Command::UpdatePaddingScheme, 0);
            update.data = scheme;
            self.queue_control(update)?;
        }
        if map.get("v").and_then(|value| value.parse::<u8>().ok()).unwrap_or(0) >= 2 {
            self.peer_version.store(2, std::sync::atomic::Ordering::Release);
            let mut settings = Frame::new(Command::ServerSettings, 0);
            settings.data = b"v=2".to_vec();
            self.queue_control(settings)?;
        }
        Ok(())
    }

    async fn on_receive_fin_cmd(self: &Arc<Self>, stream_id: u32) -> std::io::Result<()> {
        let push_sender = {
            let mut streams = self.streams.lock().await;
            match streams.get_mut(&stream_id) {
                Some(stream) if !stream.remote_fin => {
                    stream.remote_fin = true;
                    Some(stream.push_sender.clone())
                }
                Some(_) | None => None,
            }
        };
        if let Some(push_sender) = push_sender
            && push_sender.send(None).is_err()
        {
            self.on_stream_read_closed(stream_id).await;
        }
        Ok(())
    }

    async fn on_stream_read_closed(self: &Arc<Self>, stream_id: u32) {
        if let Some(stream) = self.streams.lock().await.get_mut(&stream_id) {
            stream.read_closed = true;
        }
        self.finish_stream_if_closed(stream_id).await;
    }

    async fn finish_stream_if_closed(self: &Arc<Self>, stream_id: u32) {
        let should_finish = self
            .streams
            .lock()
            .await
            .get(&stream_id)
            .is_some_and(|stream| stream.local_fin && stream.remote_fin && stream.read_closed);
        if should_finish {
            self.remove_stream_by_id(stream_id).await;
        }
    }

    async fn helper_drop_stream_by_id(self: &Arc<Self>, stream_id: u32) {
        let session_id = self.id();
        let should_send_fin = {
            let mut streams = self.streams.lock().await;
            match streams.get_mut(&stream_id) {
                Some(stream) if !stream.local_fin => {
                    stream.local_fin = true;
                    true
                }
                Some(_) | None => false,
            }
        };
        if should_send_fin && let Err(error) = self.enqueue_fin(stream_id).await {
            let mn = method_name_unstable!();
            log::warn!("{mn} -- Session {session_id}: Failed to send FIN for stream {stream_id}: {error}");
        }
        self.remove_stream_by_id(stream_id).await;
    }

    /// Finish the stream state identified by `stream_id` by removing it from the container of active stream states and dropping its resources.
    /// If this was the last active stream state, mark the session as idle.
    async fn remove_stream_by_id(self: &Arc<Self>, stream_id: u32) {
        let mn = method_name_unstable!();
        let session_id = self.id();
        let became_idle = {
            let mut streams = self.streams.lock().await;
            if let Some(entry) = streams.remove(&stream_id) {
                drop(entry.writer);
                entry.close_token.cancel();
                let l = streams.len();
                log::trace!("{mn} -- Stream {stream_id} removed in session {session_id}, remaining streams: {l}");
            }
            streams.is_empty()
        };

        if became_idle {
            if self.is_expired() {
                log::debug!("{mn} -- Expired session {session_id} drained; shutting it down");
                let _ = self.shutdown().await;
                return;
            }
            let sender = self.idle_sender.lock().await.clone();
            if let Some(sender) = sender
                && let Err(e) = sender.send(Arc::clone(self))
            {
                log::warn!("{mn} -- Failed to send session {session_id} to idle sessions pool: {e}");
            }
            log::trace!("{mn} -- Session {session_id} became idle.");
        }
    }

    async fn on_receive_syn_cmd(self: &Arc<Self>, stream_id: u32) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind::BrokenPipe, ErrorKind::InvalidData};
        if self.is_client || self.incoming_sender.is_none() {
            return Ok(());
        }
        if !self.received_settings.load(std::sync::atomic::Ordering::Acquire) {
            let mut alert = Frame::new(Command::Alert, 0);
            alert.data = b"client did not send its settings".to_vec();
            self.queue_control(alert)?;
            return Err(Error::new(InvalidData, "settings required before SYN"));
        }
        let (local, remote) = tokio::io::duplex(64 * 1024);
        let rejection = {
            let mut rejection = None;
            let mut streams = self.streams.lock().await;
            if streams.contains_key(&stream_id) {
                return Ok(());
            }
            if self.is_expired() {
                rejection = Some("session reached its maximum age");
            } else if streams.len() >= self.max_streams {
                rejection = Some("session stream limit reached");
            } else {
                streams.insert(stream_id, StreamState::new(remote, stream_id, Arc::downgrade(self)));
                let mn = method_name_unstable!();
                let l = streams.len();
                log::debug!("{mn} -- session {} accepted new stream {stream_id}, total streams: {l}", self.id(),);
            }
            rejection
        };
        if let Some(message) = rejection {
            let mut frame = Frame::new(Command::SynAck, stream_id);
            frame.data = message.as_bytes().to_vec();
            self.queue_control(frame)?;
            return Ok(());
        }
        let stream = Stream::new(stream_id, Arc::downgrade(self), local);
        let sender = self.incoming_sender.as_ref().unwrap().clone();
        match sender.try_send(stream) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                let mut rejection = Frame::new(Command::SynAck, stream_id);
                rejection.data = b"incoming stream queue is full".to_vec();
                self.queue_control(rejection)?;
                self.remove_stream_by_id(stream_id).await;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.remove_stream_by_id(stream_id).await;
                return Err(Error::new(BrokenPipe, "stream receiver closed"));
            }
        }
        Ok(())
    }
}

struct Writer {
    transport: tokio::io::WriteHalf<BoxTransport>,

    /// Client Settings and SYN are buffered until the initial SYN write request
    /// disables buffering, then flushed before subsequent data frames.
    buffering: bool,

    /// Buffer frames that have not yet been truly written to the underlying connection.
    buffered: Vec<u8>,

    /// Indicates whether padding still needs to be sent.
    /// Once the padding scheme's termination condition is met, this is set to false, and subsequent frames are sent directly without padding.
    send_padding: bool,

    /// Records the number of logical writes, used to select the padding record size.
    packet_counter: Arc<std::sync::atomic::AtomicU32>,
}

impl Writer {
    async fn write_conn(&mut self, mut bytes: Vec<u8>, padding: &Arc<RwLock<PaddingFactory>>) -> std::io::Result<usize> {
        if self.buffering {
            self.buffered.extend_from_slice(&bytes);
            return Ok(bytes.len());
        }
        if !self.buffered.is_empty() {
            let mut buffered = std::mem::take(&mut self.buffered);
            buffered.append(&mut bytes);
            bytes = buffered;
        }
        if self.send_padding {
            let packet_counter = self.packet_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let factory = padding.read().await;
            let sizes = (packet_counter < factory.stop).then(|| factory.generate_record_payload_sizes(packet_counter));
            drop(factory);
            if let Some(sizes) = sizes {
                for size in sizes {
                    if size == CHECK_MARK {
                        if bytes.is_empty() {
                            break;
                        }
                        continue;
                    }
                    let size = size as usize;
                    if bytes.len() > size {
                        self.transport.write_all(&bytes[..size]).await?;
                        bytes.drain(..size);
                    } else if !bytes.is_empty() {
                        let padding_len = size.saturating_sub(bytes.len() + HEADER_OVERHEAD_SIZE);
                        if padding_len > 0 {
                            let mut waste = Frame::new(Command::Waste, 0).encode()?;
                            waste[5..7].copy_from_slice(&(padding_len as u16).to_be_bytes());
                            waste.resize(HEADER_OVERHEAD_SIZE + padding_len, 0);
                            bytes.extend_from_slice(&waste);
                        }
                        self.transport.write_all(&bytes).await?;
                        bytes.clear();
                    } else {
                        let mut waste = Frame::new(Command::Waste, 0).encode()?;
                        waste[5..7].copy_from_slice(&(size as u16).to_be_bytes());
                        waste.resize(HEADER_OVERHEAD_SIZE + size, 0);
                        self.transport.write_all(&waste).await?;
                    }
                }
            } else {
                self.send_padding = false;
            }
        }
        if !bytes.is_empty() {
            self.transport.write_all(&bytes).await?;
        }
        self.transport.flush().await?;
        Ok(bytes.len())
    }
}

pub struct Stream {
    id: u32,
    session: Weak<Session>,
    session_id: usize,
    reader: Arc<Mutex<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    writer: Arc<Mutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>>,
    closed: bool,
    handshake_reported: std::sync::atomic::AtomicBool,
}

impl Stream {
    fn new(id: u32, session: Weak<Session>, io: tokio::io::DuplexStream) -> Self {
        let mn = method_name_unstable!();
        let (reader, writer) = tokio::io::split(io);
        let session_id = session.upgrade().map(|s| s.id()).unwrap_or_default();
        log::trace!("{mn} -- Creating stream {id} in session {session_id}");
        Self {
            id,
            session,
            session_id,
            reader: Arc::new(Mutex::new(reader)),
            writer: Arc::new(Mutex::new(writer)),
            closed: false,
            handshake_reported: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn session_id(&self) -> usize {
        self.session_id
    }

    #[cfg(all(test, feature = "client"))]
    pub(crate) fn session(&self) -> Option<Arc<Session>> {
        self.session.upgrade()
    }

    pub async fn read(&self, data: &mut [u8]) -> std::io::Result<usize> {
        self.reader.lock().await.read(data).await
    }

    pub async fn write(&self, data: &[u8]) -> std::io::Result<usize> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        if self.closed {
            let mn = method_name_unstable!();
            log::debug!("{mn} -- Stream {} is closed, can't write data", self.id);
            return Err(Error::new(BrokenPipe, format!("stream {} closed, can't write data", self.id)));
        }
        let session = self
            .session
            .upgrade()
            .ok_or_else(|| Error::new(BrokenPipe, format!("session {} closed, can't write data", self.session_id)))?;
        session.write_data(self.id, data).await
    }

    pub async fn handshake_success(&self) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        use std::sync::atomic::Ordering::{AcqRel, Acquire};
        let session = self
            .session
            .upgrade()
            .ok_or_else(|| Error::new(BrokenPipe, format!("session {} closed", self.session_id)))?;
        if session.peer_version.load(Acquire) >= 2 && self.handshake_reported.compare_exchange(false, true, AcqRel, Acquire).is_ok() {
            session.write_control(Frame::new(Command::SynAck, self.id)).await?;
        }
        Ok(())
    }

    pub async fn handshake_failure(&self, error: &str) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        use std::sync::atomic::Ordering::{AcqRel, Acquire};
        let session = self
            .session
            .upgrade()
            .ok_or_else(|| Error::new(BrokenPipe, format!("session {} closed", self.session_id)))?;
        if session.peer_version.load(Acquire) >= 2 && self.handshake_reported.compare_exchange(false, true, AcqRel, Acquire).is_ok() {
            let mut frame = Frame::new(Command::SynAck, self.id);
            frame.data = error.as_bytes().to_vec();
            session.write_control(frame).await?;
        }
        Ok(())
    }

    pub async fn close(&mut self) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let close_result = match self.session.upgrade() {
            Some(session) => session.close_stream_by_id(self.id).await,
            None => Err(Error::new(BrokenPipe, format!("session {} closed already", self.session_id))),
        };
        let shutdown_result = self.writer.lock().await.shutdown().await;
        close_result.and(shutdown_result)
    }

    pub async fn shutdown_write_by_send_fin_to_remote(&self) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        let session_id = self.session_id;
        match self.session.upgrade() {
            Some(session) => session.send_fin_by_id(self.id).await,
            None => Err(Error::new(BrokenPipe, format!("can't send FIN for session {session_id} closed"))),
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let id = self.id;
        let session_id = self.session_id;
        let mn = method_name_unstable!();

        log::trace!("{mn} -- Dropping stream {id} of session {session_id}...");
        if self.closed {
            log::debug!("{mn} -- Stream {id} of session {session_id} already closed, skipping drop it.");
            return;
        }
        self.closed = true;

        let Some(session) = self.session.upgrade() else {
            log::debug!("{mn} -- Stream {id}: Session {session_id} already closed but stream lifecycle not complete yet, skipping drop.");
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                session.helper_drop_stream_by_id(id).await;
                log::debug!("{mn} -- Dropped stream {id} of session {session_id} successfully.");
            });
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::DEFAULT_SCHEME;

    #[test]
    fn stream_ids_skip_zero_when_wrapping() {
        let counter = AtomicU32::new(u32::MAX - 1);
        assert_eq!(allocate_stream_id(&counter), u32::MAX);
        assert_eq!(allocate_stream_id(&counter), 1);
    }

    #[test]
    fn stream_ids_are_allocated_globally() {
        let first = allocate_stream_id(&NEXT_STREAM_ID);
        let second = allocate_stream_id(&NEXT_STREAM_ID);
        assert_ne!(first, 0);
        assert_ne!(second, 0);
        assert_ne!(first, second);
    }

    #[derive(Default)]
    pub(crate) struct WriteGate {
        blocked: std::sync::atomic::AtomicBool,
        dropped: std::sync::atomic::AtomicBool,
        waker: std::sync::Mutex<Option<std::task::Waker>>,
        pending: Notify,
    }

    impl WriteGate {
        pub(crate) fn block(&self) {
            self.blocked.store(true, std::sync::atomic::Ordering::SeqCst);
        }

        #[cfg(feature = "client")]
        pub(crate) fn release(&self) {
            self.blocked.store(false, std::sync::atomic::Ordering::SeqCst);
            if let Some(waker) = self.waker.lock().unwrap().take() {
                waker.wake();
            }
        }

        pub(crate) async fn wait_pending(&self) {
            tokio::time::timeout(Duration::from_secs(1), self.pending.notified()).await.unwrap();
        }
    }

    struct GatedTransport {
        inner: tokio::io::DuplexStream,
        gate: Arc<WriteGate>,
    }

    impl Drop for GatedTransport {
        fn drop(&mut self) {
            self.gate.dropped.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl tokio::io::AsyncRead for GatedTransport {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            context: &mut std::task::Context<'_>,
            buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_read(context, buffer)
        }
    }

    impl tokio::io::AsyncWrite for GatedTransport {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            context: &mut std::task::Context<'_>,
            data: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if self.gate.blocked.load(std::sync::atomic::Ordering::SeqCst) {
                *self.gate.waker.lock().unwrap() = Some(context.waker().clone());
                self.gate.pending.notify_one();
                return std::task::Poll::Pending;
            }
            std::pin::Pin::new(&mut self.inner).poll_write(context, data)
        }

        fn poll_flush(mut self: std::pin::Pin<&mut Self>, context: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_flush(context)
        }

        fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, context: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_shutdown(context)
        }
    }

    pub(crate) fn gated_transport() -> (BoxTransport, tokio::io::DuplexStream, Arc<WriteGate>) {
        let (inner, peer) = tokio::io::duplex(128 * 1024);
        let gate = Arc::new(WriteGate::default());
        (
            Box::new(GatedTransport {
                inner,
                gate: Arc::clone(&gate),
            }),
            peer,
            gate,
        )
    }

    #[tokio::test]
    async fn blocked_heart_response_does_not_block_inbound_data() {
        let (transport, mut peer, gate) = gated_transport();
        let session = Session::new_client(1, transport, padding(), 1, DEFAULT_MAX_SESSION_AGE);
        session.run().await.unwrap();
        let stream = session.open_stream().await.unwrap();
        gate.block();
        peer.write_all(&Frame::new(Command::HeartRequest, 0).encode().unwrap())
            .await
            .unwrap();
        gate.wait_pending().await;
        let mut push = Frame::new(Command::Push, stream.id());
        push.data = b"response".to_vec();
        peer.write_all(&push.encode().unwrap()).await.unwrap();
        let mut received = [0u8; 8];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), stream.read(&mut received))
                .await
                .unwrap()
                .unwrap(),
            8
        );
        assert_eq!(&received, b"response");
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_interrupts_blocked_writer_and_releases_transport() {
        let (transport, _peer, gate) = gated_transport();
        let session = Session::new_client(1, transport, padding(), 1, DEFAULT_MAX_SESSION_AGE);
        session.run().await.unwrap();
        let stream = session.open_stream().await.unwrap();
        gate.block();
        let write = tokio::spawn(async move { stream.write(b"pending").await });
        gate.wait_pending().await;
        session.shutdown().await.unwrap();
        assert!(write.await.unwrap().is_err());
        tokio::time::timeout(Duration::from_secs(1), async {
            while !gate.dropped.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown must release transport without unblocking the peer");
        assert!(session.is_closed());
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_writer_times_out_and_closes_session() {
        let (transport, _peer, gate) = gated_transport();
        let session = Session::new_client(1, transport, padding(), 1, DEFAULT_MAX_SESSION_AGE);
        session.run().await.unwrap();
        let stream = session.open_stream().await.unwrap();
        gate.block();
        let write = tokio::spawn(async move { stream.write(b"pending").await });
        gate.wait_pending().await;
        tokio::time::advance(WRITE_TIMEOUT).await;
        session.close_token.cancelled().await;
        assert!(session.is_closed());
        assert!(write.await.unwrap().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn unanswered_v2_heartbeat_closes_silent_session() {
        let (transport, _peer, _gate) = gated_transport();
        let session = Session::new_client(1, transport, padding(), 1, DEFAULT_MAX_SESSION_AGE);
        session.run().await.unwrap();
        let _stream = session.open_stream().await.unwrap();
        session.peer_version.store(2, std::sync::atomic::Ordering::Release);
        tokio::task::yield_now().await;
        tokio::time::advance(HEARTBEAT_INTERVAL).await;
        tokio::task::yield_now().await;
        tokio::time::advance(HEARTBEAT_TIMEOUT).await;
        session.close_token.cancelled().await;
        assert!(session.is_closed());
    }

    #[tokio::test(start_paused = true)]
    async fn v1_session_does_not_send_proactive_heartbeats() {
        let (local, mut peer) = tokio::io::duplex(128 * 1024);
        let padding = Arc::new(RwLock::new(PaddingFactory::new(b"stop=0").unwrap()));
        let session = Session::new_client(1, Box::new(local), padding, 1, DEFAULT_MAX_SESSION_AGE);
        session.run().await.unwrap();
        let _stream = session.open_stream().await.unwrap();
        assert_eq!(Frame::read_from(&mut peer).await.unwrap().command, Command::Settings);
        assert_eq!(Frame::read_from(&mut peer).await.unwrap().command, Command::Syn);
        tokio::time::advance(HEARTBEAT_INTERVAL * 4).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), Frame::read_from(&mut peer))
                .await
                .is_err()
        );
        assert!(!session.is_closed());
        session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeats_keep_healthy_v2_session_open() {
        let (local, peer) = tokio::io::duplex(128 * 1024);
        let client = Session::new_client(1, Box::new(local), padding(), 1, DEFAULT_MAX_SESSION_AGE);
        let server = Session::new_server(2, Box::new(peer), padding(), 1, DEFAULT_MAX_SESSION_AGE);
        client.run().await.unwrap();
        server.run().await.unwrap();
        let _stream = client.open_stream().await.unwrap();
        let _peer_stream = server.accept_stream().await.unwrap();
        for _probe in 0..4 {
            tokio::time::advance(HEARTBEAT_INTERVAL).await;
            for _turn in 0..20 {
                tokio::task::yield_now().await;
            }
            assert!(!client.is_closed());
            assert!(!server.is_closed());
        }
        client.shutdown().await.unwrap();
        server.shutdown().await.unwrap();
    }

    #[test]
    fn classifies_peer_disconnect_errors() {
        use std::io::ErrorKind;

        for kind in [
            ErrorKind::InvalidData,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::UnexpectedEof,
            ErrorKind::NotConnected,
        ] {
            assert!(!is_peer_disconnect(&std::io::Error::from(kind)));
        }
        assert!(is_peer_disconnect(&std::io::Error::from(ErrorKind::ConnectionReset)));
    }

    #[tokio::test]
    async fn peer_eof_shuts_down_session_without_receive_error() {
        let (session_io, peer_io) = tokio::io::duplex(128 * 1024);
        let session = Session::new_client(1, Box::new(session_io), padding(), 1, DEFAULT_MAX_SESSION_AGE);
        session.run().await.unwrap();

        drop(peer_io);
        tokio::time::timeout(Duration::from_secs(1), session.close_token.cancelled())
            .await
            .unwrap();
        assert!(session.is_closed());
    }

    fn padding() -> Arc<RwLock<PaddingFactory>> {
        Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap()))
    }

    #[tokio::test]
    async fn direct_session_api_rejects_new_streams_after_max_age_and_drains_existing_streams() {
        let (local, _peer) = tokio::io::duplex(128 * 1024);
        let session = Session::new_client(1, Box::new(local), padding(), 2, Duration::from_millis(30));
        session.run().await.unwrap();
        let mut existing = session.open_stream().await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;

        let error = match session.open_stream().await {
            Ok(_) => panic!("expired session must reject new streams"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(!session.is_closed(), "existing streams should be allowed to drain");

        existing.close().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), session.close_token.cancelled())
            .await
            .expect("expired session should close after its final stream drains");
    }

    #[tokio::test]
    async fn server_rejects_incoming_stream_after_max_age() {
        let (transport, mut peer, _gate) = gated_transport();
        let session = Session::new_server(1, transport, padding(), 1, Duration::from_millis(1));
        session.received_settings.store(true, std::sync::atomic::Ordering::Release);
        tokio::time::sleep(Duration::from_millis(5)).await;

        session.on_receive_syn_cmd(1).await.unwrap();
        assert!(session.is_idle().await);
        let rejection = tokio::time::timeout(Duration::from_secs(1), Frame::read_from(&mut peer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rejection.command, Command::SynAck);
        assert_eq!(rejection.data, b"session reached its maximum age");
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn exchanges_settings_syn_data_and_synack() {
        let (client_io, server_io) = tokio::io::duplex(128 * 1024);
        let client = Session::new_client(1, Box::new(client_io), padding(), 2, DEFAULT_MAX_SESSION_AGE);
        let server = Session::new_server(1, Box::new(server_io), padding(), 2, DEFAULT_MAX_SESSION_AGE);
        client.run().await.unwrap();
        server.run().await.unwrap();

        let mut client_stream = client.open_stream().await.unwrap();
        assert_ne!(client_stream.id(), 0);

        client_stream.write(b"hello").await.unwrap();
        let server_stream = server.accept_stream().await.unwrap();
        assert_eq!(server_stream.id(), client_stream.id());
        let mut received = [0u8; 5];
        server_stream.read(&mut received).await.unwrap();
        assert_eq!(&received, b"hello");

        server_stream.handshake_success().await.unwrap();
        server_stream.write(b"world").await.unwrap();
        let mut response = [0u8; 5];
        client_stream.read(&mut response).await.unwrap();
        assert_eq!(&response, b"world");

        client_stream.close().await.unwrap();
        let _ = server.shutdown().await;
        let _ = client.shutdown().await;
    }

    #[tokio::test]
    async fn flushes_syn_without_waiting_for_stream_data() {
        let (client_io, server_io) = tokio::io::duplex(128 * 1024);
        let client = Session::new_client(1, Box::new(client_io), padding(), 1, DEFAULT_MAX_SESSION_AGE);
        let server = Session::new_server(1, Box::new(server_io), padding(), 1, DEFAULT_MAX_SESSION_AGE);
        client.run().await.unwrap();
        server.run().await.unwrap();

        let _client_stream = client.open_stream().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), server.accept_stream())
            .await
            .unwrap()
            .unwrap();

        let _ = server.shutdown().await;
        let _ = client.shutdown().await;
    }

    #[tokio::test]
    async fn drains_large_push_queue_before_fin_eof() {
        let (client_io, server_io) = tokio::io::duplex(128 * 1024);
        let client = Session::new_client(1, Box::new(client_io), padding(), 1, DEFAULT_MAX_SESSION_AGE);
        let server = Session::new_server(1, Box::new(server_io), padding(), 1, DEFAULT_MAX_SESSION_AGE);
        client.run().await.unwrap();
        server.run().await.unwrap();

        let client_stream = client.open_stream().await.unwrap();
        let server_stream = server.accept_stream().await.unwrap();
        let expected = vec![0x5a; 512 * 1024];
        let send_payload = expected.clone();
        let sender = tokio::spawn(async move {
            for chunk in send_payload.chunks(16 * 1024) {
                server_stream.write(chunk).await.unwrap();
            }
            let mut server_stream = server_stream;
            server_stream.close().await.unwrap();
        });

        let mut received = Vec::with_capacity(expected.len());
        let mut buffer = [0u8; 8192];
        loop {
            let size = tokio::time::timeout(Duration::from_secs(5), client_stream.read(&mut buffer))
                .await
                .expect("stream data should keep arriving")
                .unwrap();
            if size == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..size]);
        }

        sender.await.unwrap();
        assert_eq!(received, expected);
        drop(client_stream);
        let _ = client.shutdown().await;
        let _ = server.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_drains_streams_after_session_closed() {
        let (client_io, server_io) = tokio::io::duplex(128 * 1024);
        let client = Session::new_client(1, Box::new(client_io), padding(), 1, DEFAULT_MAX_SESSION_AGE);
        let server = Session::new_server(1, Box::new(server_io), padding(), 1, DEFAULT_MAX_SESSION_AGE);
        client.run().await.unwrap();
        server.run().await.unwrap();

        let client_stream = client.open_stream().await.unwrap();
        let _server_stream = server.accept_stream().await.unwrap();
        client.closed.store(true, std::sync::atomic::Ordering::Release);
        client.shutdown().await.unwrap();

        assert!(client.streams.lock().await.is_empty());
        let mut data = [0u8; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), client_stream.read(&mut data))
                .await
                .unwrap()
                .unwrap(),
            0
        );

        let _ = server.shutdown().await;
    }

    #[tokio::test]
    async fn dropping_stream_releases_session_stream() {
        let (client_io, server_io) = tokio::io::duplex(128 * 1024);
        let client = Session::new_client(1, Box::new(client_io), padding(), 1, DEFAULT_MAX_SESSION_AGE);
        let server = Session::new_server(1, Box::new(server_io), padding(), 1, DEFAULT_MAX_SESSION_AGE);
        client.run().await.unwrap();
        server.run().await.unwrap();

        let client_stream = client.open_stream().await.unwrap();
        client_stream.write(b"x").await.unwrap();
        let _server_stream = server.accept_stream().await.unwrap();
        drop(client_stream);

        tokio::time::timeout(Duration::from_secs(1), async {
            while !client.streams.lock().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropped stream should be released");

        let _ = server.shutdown().await;
        let _ = client.shutdown().await;
    }

    #[tokio::test]
    async fn open_stream_fails_after_shutdown() {
        let (client_io, server_io) = tokio::io::duplex(128 * 1024);
        let client = Session::new_client(1, Box::new(client_io), padding(), 1, DEFAULT_MAX_SESSION_AGE);
        let server = Session::new_server(1, Box::new(server_io), padding(), 1, DEFAULT_MAX_SESSION_AGE);
        client.run().await.unwrap();
        server.run().await.unwrap();

        client.shutdown().await.unwrap();
        let err = client.open_stream().await;
        assert!(err.is_err());
        if let Err(err) = &err {
            assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        }
        assert!(client.streams.lock().await.is_empty());

        let _ = server.shutdown().await;
    }
}
