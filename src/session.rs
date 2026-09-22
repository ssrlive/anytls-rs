use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Mutex, RwLock, mpsc, oneshot},
    time::{Duration, timeout},
};
use tokio_util::sync::CancellationToken;

use crate::{
    frame::{Command, Frame},
    padding::PaddingFactory,
    string_map,
};

pub type BoxTransport = Box<dyn AsyncReadWrite>;

const FIN_ACK_TIMEOUT: Duration = Duration::from_secs(3);
const PUSH_QUEUE_CAPACITY: usize = 16;

pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncReadWrite for T {}

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

struct WriteRequest {
    bytes: Vec<u8>,
    disable_buffering_after: bool,
    result: Option<oneshot::Sender<std::io::Result<()>>>,
}

/// Represents an status of a logical stream within a session.
pub(crate) struct StreamEntry {
    pub(crate) writer: Arc<Mutex<tokio::io::DuplexStream>>,
    pub(crate) push_sender: mpsc::Sender<Vec<u8>>,
    /// Indicates whether the local side has sent a FIN for this stream.
    /// The stream is considered fully closed when both local and remote sides have sent FIN.
    pub(crate) local_fin: bool,
    /// Used to notify an active-close task that is waiting for a FIN response.
    pub(crate) close_token: CancellationToken,
}

impl StreamEntry {
    pub(crate) fn new(writer: tokio::io::DuplexStream) -> Self {
        let (push_sender, mut push_receiver) = mpsc::channel::<Vec<u8>>(PUSH_QUEUE_CAPACITY);
        let writer = Arc::new(Mutex::new(writer));
        let task_writer = Arc::clone(&writer);
        let close_token = CancellationToken::new();
        let task_close_token = close_token.clone();
        tokio::spawn(async move {
            while let Some(data) = push_receiver.recv().await {
                let result = tokio::select! {
                    result = async { task_writer.lock().await.write_all(&data).await } => result,
                    _ = task_close_token.cancelled() => break,
                };
                if result.is_err() {
                    break;
                }
            }
        });
        Self {
            writer,
            push_sender,
            local_fin: false,
            close_token,
        }
    }
}

pub struct Session {
    session_id: usize,
    created_at: std::time::Instant,
    max_streams: usize,
    write_sender: mpsc::Sender<WriteRequest>,
    streams: Mutex<HashMap<u32, StreamEntry>>,
    next_stream_id: std::sync::atomic::AtomicU32,
    /// Indicates whether the session has been closed.
    closed: Arc<std::sync::atomic::AtomicBool>,
    is_client: bool,
    peer_version: std::sync::atomic::AtomicU8,
    received_settings: std::sync::atomic::AtomicBool,
    padding: Arc<RwLock<PaddingFactory>>,
    reader: Mutex<tokio::io::ReadHalf<BoxTransport>>,
    close_token: CancellationToken,
    incoming: Mutex<Option<mpsc::Receiver<Stream>>>,
    incoming_sender: Option<mpsc::Sender<Stream>>,
    idle_sender: Mutex<Option<mpsc::UnboundedSender<Arc<Session>>>>,
}

impl Session {
    pub fn new_client(session_id: usize, transport: BoxTransport, padding: Arc<RwLock<PaddingFactory>>, max_streams: usize) -> Arc<Self> {
        Arc::new(Self::new(session_id, transport, padding, true, max_streams, None))
    }

    pub fn new_server(session_id: usize, transport: BoxTransport, padding: Arc<RwLock<PaddingFactory>>, max_streams: usize) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel(32);
        Arc::new(Self::new(
            session_id,
            transport,
            padding,
            false,
            max_streams,
            Some((sender, receiver)),
        ))
    }

    fn new(
        session_id: usize,
        transport: BoxTransport,
        padding: Arc<RwLock<PaddingFactory>>,
        is_client: bool,
        max_streams: usize,
        incoming: Option<(mpsc::Sender<Stream>, mpsc::Receiver<Stream>)>,
    ) -> Self {
        let (reader, writer) = tokio::io::split(transport);
        let incoming_sender = incoming.as_ref().map(|(sender, _)| sender.clone());
        let close_token = CancellationToken::new();
        let writer_close_token = close_token.clone();
        let packet_counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let writer_packet_counter = Arc::clone(&packet_counter);
        let (write_sender, mut write_receiver) = mpsc::channel::<WriteRequest>(64);
        let writer_padding = Arc::clone(&padding);
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer_closed = Arc::clone(&closed);
        tokio::spawn(async move {
            let mut writer = Writer {
                transport: writer,
                buffering: is_client,
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
                    let _ = writer.transport.shutdown().await;
                    break;
                };
                let mut result = writer.write_conn(request.bytes, &writer_padding).await.map(|_| ());
                if result.is_ok() && request.disable_buffering_after {
                    writer.buffering = false;
                    if !writer.buffered.is_empty() {
                        result = writer.write_conn(Vec::new(), &writer_padding).await.map(|_| ());
                    }
                }
                let failed = result.is_err();
                if let Some(result_sender) = request.result {
                    let _ = result_sender.send(result);
                }
                if failed {
                    writer_closed.store(true, std::sync::atomic::Ordering::Release);
                    writer_close_token.cancel();
                }
            }
        });
        Self {
            session_id,
            created_at: std::time::Instant::now(),
            max_streams: max_streams.max(1),
            write_sender,
            streams: Mutex::new(HashMap::new()),
            next_stream_id: std::sync::atomic::AtomicU32::new(0),
            closed,
            is_client,
            peer_version: std::sync::atomic::AtomicU8::new(0),
            received_settings: std::sync::atomic::AtomicBool::new(false),
            padding,
            reader: Mutex::new(reader),
            close_token,
            incoming: Mutex::new(incoming.map(|(_, receiver)| receiver)),
            incoming_sender,
            idle_sender: Mutex::new(None),
        }
    }

    pub fn id(&self) -> usize {
        self.session_id
    }

    pub fn is_expired(&self, max_age: Duration) -> bool {
        !max_age.is_zero() && self.created_at.elapsed() >= max_age
    }

    #[inline]
    pub async fn has_stream_capacity(&self) -> bool {
        self.streams.lock().await.len() < self.max_streams
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
            frame.data = string_map::to_bytes(&settings);
            self.write_control(frame).await?;
        }

        let session = Arc::clone(self);
        tokio::spawn(async move {
            let _ = Arc::clone(&session).receive_loop().await;
            let _ = session.shutdown().await;
        });
        Ok(())
    }

    pub async fn accept_stream(&self) -> std::io::Result<Stream> {
        use std::io::{Error, ErrorKind::BrokenPipe, ErrorKind::Unsupported};
        let mut incoming = self.incoming.lock().await;
        let receiver = incoming
            .as_mut()
            .ok_or_else(|| Error::new(Unsupported, "client sessions do not accept streams"))?;
        let stream = tokio::select! {
            stream = receiver.recv() => stream.ok_or_else(|| Error::new(BrokenPipe, "session closed because of remote closure"))?,
            _ = self.close_token.cancelled() => return Err(Error::new(BrokenPipe, "session closed because of cancellation")),
        };
        if stream.is_closed() {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stream closed"));
        }
        Ok(stream)
    }

    pub async fn open_stream(self: &Arc<Self>) -> std::io::Result<Stream> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        if self.is_closed() {
            return Err(Error::new(BrokenPipe, "session closed"));
        }
        let id = self.next_stream_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let (local, remote) = tokio::io::duplex(64 * 1024);

        {
            let mut streams = self.streams.lock().await;
            // Re-check under the streams lock so we cannot insert after shutdown drained.
            if self.is_closed() {
                return Err(Error::new(BrokenPipe, "session closed"));
            }
            if streams.len() >= self.max_streams {
                return Err(Error::new(std::io::ErrorKind::WouldBlock, "session stream limit reached"));
            }
            streams.insert(id, StreamEntry::new(remote));
        }
        if let Err(error) = self.enqueue_frame(Frame::new(Command::Syn, id), true).await {
            self.finish_stream_by_id(id).await;
            return Err(error);
        }
        if self.is_closed() {
            self.finish_stream_by_id(id).await;
            return Err(Error::new(BrokenPipe, "session closed"));
        }

        Ok(Stream::new(id, Arc::downgrade(self), local))
    }

    pub(crate) async fn write_data(&self, stream_id: u32, data: &[u8]) -> std::io::Result<usize> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        let mut frame = Frame::new(Command::Push, stream_id);
        frame.data.extend_from_slice(data);
        let bytes = frame.encode()?;

        if !self.streams.lock().await.contains_key(&stream_id) {
            return Err(Error::new(BrokenPipe, format!("stream {stream_id} closed")));
        }
        self.enqueue_encoded(bytes, false).await?;
        Ok(data.len())
    }

    pub(crate) async fn close_stream_by_id(self: &Arc<Self>, stream_id: u32) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind::BrokenPipe, ErrorKind::TimedOut};
        if self.is_closed() {
            return Ok(());
        }
        let close_notify = {
            let mut streams = self.streams.lock().await;
            let stream = streams.get_mut(&stream_id).ok_or_else(|| Error::new(BrokenPipe, "stream closed"))?;
            if stream.local_fin {
                return Ok(());
            }
            stream.local_fin = true;
            stream.close_token.clone()
        };
        if let Err(e) = self.enqueue_frame(Frame::new(Command::Fin, stream_id), false).await {
            self.finish_stream_by_id(stream_id).await;
            return Err(e);
        }
        if timeout(FIN_ACK_TIMEOUT, close_notify.cancelled()).await.is_err() {
            self.finish_stream_by_id(stream_id).await;
            return Err(Error::new(TimedOut, "timed out waiting for FIN reply"));
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> std::io::Result<()> {
        self.closed.store(true, std::sync::atomic::Ordering::Release);
        self.close_token.cancel();
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

    async fn enqueue_frame(&self, frame: Frame, disable_buffering_after: bool) -> std::io::Result<()> {
        self.enqueue_encoded_with_mode(frame.encode()?, disable_buffering_after).await
    }

    async fn enqueue_encoded(&self, bytes: Vec<u8>, disable_buffering_after: bool) -> std::io::Result<()> {
        self.enqueue_encoded_with_mode(bytes, disable_buffering_after).await
    }

    async fn enqueue_encoded_with_mode(&self, bytes: Vec<u8>, disable_buffering_after: bool) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        let (result_sender, result_receiver) = oneshot::channel();
        tokio::select! {
            result = self.write_sender.send(WriteRequest {
                bytes,
                disable_buffering_after,
                result: Some(result_sender),
            }) => result.map_err(|_| Error::new(BrokenPipe, "writer task closed"))?,
            _ = self.close_token.cancelled() => return Err(Error::new(BrokenPipe, "session closed")),
        }
        tokio::select! {
            result = result_receiver => result.map_err(|_| Error::new(BrokenPipe, "writer task closed"))?,
            _ = self.close_token.cancelled() => Err(Error::new(BrokenPipe, "session closed")),
        }
    }

    async fn receive_loop(self: Arc<Self>) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind::InvalidData};
        loop {
            let frame = tokio::select! {
                result = async {
                    Frame::read_from(&mut *self.reader.lock().await).await
                } => result?,
                _ = self.close_token.cancelled() => break Ok(()),
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
                    if let Some(push_sender) = push_sender {
                        match push_sender.try_send(frame.data) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_data)) => {
                                log::warn!("Push queue full for stream {stream_id}; closing stream");
                                self.send_fin_before_finish(stream_id).await;
                                self.finish_stream_by_id(stream_id).await;
                            }
                            Err(mpsc::error::TrySendError::Closed(_data)) => {
                                log::warn!("Push worker closed for stream {stream_id}; closing stream");
                                self.send_fin_before_finish(stream_id).await;
                                self.finish_stream_by_id(stream_id).await;
                            }
                        }
                    }
                }
                Command::Waste => {}
                Command::Syn => self.on_receive_syn_cmd(stream_id).await?,
                Command::Fin => self.on_receive_fin_cmd(stream_id).await?,
                Command::Settings => self.on_receive_settings(&frame.data).await?,
                Command::SynAck => {
                    if !frame.data.is_empty() {
                        self.finish_stream_by_id(stream_id).await;
                        let info = String::from_utf8_lossy(&frame.data);
                        log::warn!("Received SynAck with unexpected data for stream {stream_id}, error: {info}");
                    }
                    // TODO: Handle additional SynAck logic if necessary
                }
                Command::HeartRequest => {
                    if let Err(error) = self.enqueue_frame(Frame::new(Command::HeartResponse, stream_id), false).await {
                        log::warn!("Failed to queue heart response: {error}");
                    }
                }
                Command::HeartResponse => {}
                Command::ServerSettings => {
                    let map = string_map::from_bytes(&frame.data);
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
                    log::warn!("Received session alert: {}", String::from_utf8_lossy(&frame.data));
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
        let map = string_map::from_bytes(data);
        let update_scheme = {
            let padding = self.padding.read().await;
            (map.get("padding-md5") != Some(&padding.md5)).then(|| padding.raw_scheme().to_vec())
        };
        if let Some(scheme) = update_scheme {
            let mut update = Frame::new(Command::UpdatePaddingScheme, 0);
            update.data = scheme;
            if let Err(error) = self.enqueue_frame(update, false).await {
                log::warn!("Failed to queue padding scheme update: {error}");
            }
        }
        if map.get("v").and_then(|value| value.parse::<u8>().ok()).unwrap_or(0) >= 2 {
            self.peer_version.store(2, std::sync::atomic::Ordering::Release);
            let mut settings = Frame::new(Command::ServerSettings, 0);
            settings.data = b"v=2".to_vec();
            if let Err(error) = self.enqueue_frame(settings, false).await {
                log::warn!("Failed to queue server settings: {error}");
            }
        }
        Ok(())
    }

    async fn on_receive_fin_cmd(self: &Arc<Self>, stream_id: u32) -> std::io::Result<()> {
        let should_reply = {
            let mut streams = self.streams.lock().await;
            match streams.get_mut(&stream_id) {
                Some(stream) if !stream.local_fin => {
                    stream.local_fin = true;
                    true
                }
                Some(_) | None => false,
            }
        };
        let result = if should_reply {
            self.enqueue_frame(Frame::new(Command::Fin, stream_id), false).await
        } else {
            Ok(())
        };
        self.finish_stream_by_id(stream_id).await;
        result
    }

    async fn drop_stream_by_id(self: &Arc<Self>, stream_id: u32) {
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
        if should_send_fin && let Err(e) = self.write_control(Frame::new(Command::Fin, stream_id)).await {
            log::warn!("Failed to send FIN for stream {stream_id}: {e}");
        }
        self.finish_stream_by_id(stream_id).await;
    }

    async fn send_fin_before_finish(&self, stream_id: u32) {
        if let Err(error) = self.enqueue_frame(Frame::new(Command::Fin, stream_id), false).await {
            log::warn!("Failed to queue FIN for stream {stream_id}: {error}");
        }
    }

    async fn finish_stream_by_id(self: &Arc<Self>, stream_id: u32) {
        let became_idle = {
            let mut streams = self.streams.lock().await;
            if let Some(entry) = streams.remove(&stream_id) {
                drop(entry.writer);
                entry.close_token.cancel();
            }
            streams.is_empty()
        };

        if became_idle {
            let sender = self.idle_sender.lock().await.clone();
            if let Some(sender) = sender
                && let Err(e) = sender.send(Arc::clone(self))
            {
                log::warn!("Failed to send idle session: {e}");
            }
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
            self.enqueue_frame(alert, false).await?;
            return Err(Error::new(InvalidData, "settings required before SYN"));
        }
        let (local, remote) = tokio::io::duplex(64 * 1024);
        let mut reject = false;
        {
            let mut streams = self.streams.lock().await;
            if streams.contains_key(&stream_id) {
                return Ok(());
            }
            if streams.len() >= self.max_streams {
                reject = true;
            } else {
                streams.insert(stream_id, StreamEntry::new(remote));
            }
        }
        if reject {
            let mut rejection = Frame::new(Command::SynAck, stream_id);
            rejection.data = b"session stream limit reached".to_vec();
            self.enqueue_frame(rejection, false).await?;
            return Ok(());
        }
        let stream = Stream::new(stream_id, Arc::downgrade(self), local);
        let sender = self.incoming_sender.as_ref().unwrap().clone();
        match sender.try_send(stream) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                let mut rejection = Frame::new(Command::SynAck, stream_id);
                rejection.data = b"incoming stream queue is full".to_vec();
                self.enqueue_frame(rejection, false).await?;
                self.finish_stream_by_id(stream_id).await;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.finish_stream_by_id(stream_id).await;
                return Err(Error::new(BrokenPipe, "stream receiver closed"));
            }
        }
        Ok(())
    }
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
            if packet_counter < factory.stop {
                for size in factory.generate_record_payload_sizes(packet_counter) {
                    if size == crate::padding::CHECK_MARK {
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
                        let padding_len = size.saturating_sub(bytes.len() + crate::frame::HEADER_OVERHEAD_SIZE);
                        if padding_len > 0 {
                            let mut waste = Frame::new(Command::Waste, 0).encode()?;
                            waste[5..7].copy_from_slice(&(padding_len as u16).to_be_bytes());
                            waste.resize(crate::frame::HEADER_OVERHEAD_SIZE + padding_len, 0);
                            bytes.extend_from_slice(&waste);
                        }
                        self.transport.write_all(&bytes).await?;
                        bytes.clear();
                    } else {
                        let mut waste = Frame::new(Command::Waste, 0).encode()?;
                        waste[5..7].copy_from_slice(&(size as u16).to_be_bytes());
                        waste.resize(crate::frame::HEADER_OVERHEAD_SIZE + size, 0);
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
    io: tokio::io::DuplexStream,
    closed: bool,
    handshake_reported: std::sync::atomic::AtomicBool,
}

impl Stream {
    fn new(id: u32, session: Weak<Session>, io: tokio::io::DuplexStream) -> Self {
        Self {
            id,
            session,
            io,
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

    #[cfg(test)]
    pub(crate) fn session(&self) -> Arc<Session> {
        self.session.upgrade().expect("session should still be alive")
    }

    pub async fn read(&mut self, data: &mut [u8]) -> std::io::Result<usize> {
        self.io.read(data).await
    }

    pub async fn write(&self, data: &[u8]) -> std::io::Result<usize> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        if self.closed {
            return Err(Error::new(BrokenPipe, "stream closed"));
        }
        let session = self.session.upgrade().ok_or_else(|| Error::new(BrokenPipe, "session closed"))?;
        session.write_data(self.id, data).await
    }

    pub async fn handshake_success(&self) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        use std::sync::atomic::Ordering::{AcqRel, Acquire};
        let session = self.session.upgrade().ok_or_else(|| Error::new(BrokenPipe, "session closed"))?;
        if session.peer_version.load(Acquire) >= 2 && self.handshake_reported.compare_exchange(false, true, AcqRel, Acquire).is_ok() {
            session.write_control(Frame::new(Command::SynAck, self.id)).await?;
        }
        Ok(())
    }

    pub async fn handshake_failure(&self, error: &str) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        use std::sync::atomic::Ordering::{AcqRel, Acquire};
        let session = self.session.upgrade().ok_or_else(|| Error::new(BrokenPipe, "session closed"))?;
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
            None => Err(Error::new(BrokenPipe, "session closed")),
        };
        let shutdown_result = self.io.shutdown().await;
        close_result.and(shutdown_result)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;

        let Some(session) = self.session.upgrade() else {
            return;
        };
        let id = self.id;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                session.drop_stream_by_id(id).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::padding::DEFAULT_SCHEME;

    fn padding() -> Arc<RwLock<PaddingFactory>> {
        Arc::new(RwLock::new(PaddingFactory::new(DEFAULT_SCHEME).unwrap()))
    }

    #[tokio::test]
    async fn exchanges_settings_syn_data_and_synack() {
        let (client_io, server_io) = tokio::io::duplex(128 * 1024);
        let client = Session::new_client(1, Box::new(client_io), padding(), 2);
        let server = Session::new_server(1, Box::new(server_io), padding(), 2);
        client.run().await.unwrap();
        server.run().await.unwrap();

        let mut client_stream = client.open_stream().await.unwrap();
        assert_eq!(client_stream.id(), 1);

        client_stream.write(b"hello").await.unwrap();
        let mut server_stream = server.accept_stream().await.unwrap();
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
        let client = Session::new_client(1, Box::new(client_io), padding(), 1);
        let server = Session::new_server(1, Box::new(server_io), padding(), 1);
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
    async fn shutdown_drains_streams_after_session_closed() {
        let (client_io, server_io) = tokio::io::duplex(128 * 1024);
        let client = Session::new_client(1, Box::new(client_io), padding(), 1);
        let server = Session::new_server(1, Box::new(server_io), padding(), 1);
        client.run().await.unwrap();
        server.run().await.unwrap();

        let mut client_stream = client.open_stream().await.unwrap();
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
        let client = Session::new_client(1, Box::new(client_io), padding(), 1);
        let server = Session::new_server(1, Box::new(server_io), padding(), 1);
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
        let client = Session::new_client(1, Box::new(client_io), padding(), 1);
        let server = Session::new_server(1, Box::new(server_io), padding(), 1);
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
