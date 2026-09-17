use std::{collections::HashMap, sync::Arc};

use tokio::time::{Duration, timeout};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Mutex, Notify, RwLock, mpsc},
};

use crate::{
    frame::{Command, Frame},
    padding::PaddingFactory,
    string_map,
};

pub type BoxTransport = Box<dyn AsyncReadWrite>;

const FIN_ACK_TIMEOUT: Duration = Duration::from_secs(3);

pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncReadWrite for T {}

struct Writer {
    transport: tokio::io::WriteHalf<BoxTransport>,
    buffering: bool,
    buffered: Vec<u8>,
    send_padding: bool,
    packet_counter: u32,
}

pub(crate) struct StreamEntry {
    pub(crate) writer: tokio::io::DuplexStream,
    pub(crate) local_fin: bool,
    pub(crate) closed: Arc<Notify>,
}

pub struct Session {
    session_id: usize,
    max_streams: usize,
    writer: Mutex<Writer>,
    streams: Mutex<HashMap<u32, StreamEntry>>,
    next_stream_id: std::sync::atomic::AtomicU32,
    closed: std::sync::atomic::AtomicBool,
    is_client: bool,
    peer_version: std::sync::atomic::AtomicU8,
    received_settings: std::sync::atomic::AtomicBool,
    padding: Arc<RwLock<PaddingFactory>>,
    reader: Mutex<tokio::io::ReadHalf<BoxTransport>>,
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
        Self {
            session_id,
            max_streams: max_streams.max(1),
            writer: Mutex::new(Writer {
                transport: writer,
                buffering: is_client,
                buffered: Vec::new(),
                send_padding: true,
                packet_counter: 0,
            }),
            streams: Mutex::new(HashMap::new()),
            next_stream_id: std::sync::atomic::AtomicU32::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
            is_client,
            peer_version: std::sync::atomic::AtomicU8::new(0),
            received_settings: std::sync::atomic::AtomicBool::new(false),
            padding,
            reader: Mutex::new(reader),
            incoming: Mutex::new(incoming.map(|(_, receiver)| receiver)),
            incoming_sender,
            idle_sender: Mutex::new(None),
        }
    }

    pub fn id(&self) -> usize {
        self.session_id
    }

    pub async fn active_streams(&self) -> usize {
        self.streams.lock().await.len()
    }

    pub async fn has_stream_capacity(&self) -> bool {
        self.active_streams().await < self.max_streams
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
            let _ = session.close().await;
        });
        Ok(())
    }

    pub async fn accept_stream(&self) -> std::io::Result<Stream> {
        let mut incoming = self.incoming.lock().await;
        let receiver = incoming
            .as_mut()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Unsupported, "client sessions do not accept streams"))?;
        receiver
            .recv()
            .await
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "session closed"))
    }

    pub async fn open_stream(self: &Arc<Self>) -> std::io::Result<Stream> {
        if self.is_closed() {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "session closed"));
        }
        let id = self.next_stream_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let (local, remote) = tokio::io::duplex(64 * 1024);
        {
            let mut streams = self.streams.lock().await;
            if streams.len() >= self.max_streams {
                return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "session stream limit reached"));
            }
            streams.insert(
                id,
                StreamEntry {
                    writer: remote,
                    local_fin: false,
                    closed: Arc::new(Notify::new()),
                },
            );
        }
        if let Err(error) = self.write_control(Frame::new(Command::Syn, id)).await {
            self.streams.lock().await.remove(&id);
            return Err(error);
        }
        self.writer.lock().await.buffering = false;
        Ok(Stream::new(id, Arc::clone(self), local))
    }

    pub(crate) async fn write_data(&self, id: u32, data: &[u8]) -> std::io::Result<usize> {
        let mut frame = Frame::new(Command::Push, id);
        frame.data.extend_from_slice(data);
        let bytes = frame.encode()?;
        let mut writer = self.writer.lock().await;
        writer.write_conn(bytes, &self.padding).await?;
        Ok(data.len())
    }

    pub(crate) async fn close_stream(self: &Arc<Self>, id: u32) -> std::io::Result<()> {
        if self.is_closed() {
            return Ok(());
        }
        let closed = {
            let mut streams = self.streams.lock().await;
            let stream = streams
                .get_mut(&id)
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stream closed"))?;
            if stream.local_fin {
                return Ok(());
            }
            stream.local_fin = true;
            Arc::clone(&stream.closed)
        };
        self.write_control(Frame::new(Command::Fin, id)).await?;
        if timeout(FIN_ACK_TIMEOUT, closed.notified()).await.is_err() {
            self.finish_stream(id).await;
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out waiting for FIN reply"));
        }
        Ok(())
    }

    pub async fn close(&self) -> std::io::Result<()> {
        if self.closed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Ok(());
        }
        let streams = self.streams.lock().await.drain().map(|(_, entry)| entry).collect::<Vec<_>>();
        for entry in streams {
            entry.closed.notify_one();
        }
        let mut writer = self.writer.lock().await;
        writer.transport.shutdown().await
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Acquire)
    }

    async fn write_control(&self, frame: Frame) -> std::io::Result<()> {
        let bytes = frame.encode()?;
        let mut writer = self.writer.lock().await;
        writer.write_conn(bytes, &self.padding).await.map(|_| ())
    }

    async fn receive_loop(self: Arc<Self>) -> std::io::Result<()> {
        loop {
            let frame = Frame::read_from(&mut *self.reader.lock().await).await?;
            if !valid_stream_id(frame.command, frame.stream_id) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid stream id for command",
                ));
            }
            match frame.command {
                Command::Push => {
                    if let Some(entry) = self.streams.lock().await.get_mut(&frame.stream_id) {
                        entry.writer.write_all(&frame.data).await?;
                    }
                }
                Command::Waste => {}
                Command::Syn => self.receive_syn(frame.stream_id).await?,
                Command::Fin => {
                    self.close_remote_stream(frame.stream_id).await?;
                }
                Command::Settings => self.receive_settings(&frame.data).await?,
                Command::SynAck => {
                    if !frame.data.is_empty() {
                        self.finish_stream(frame.stream_id).await;
                    }
                }
                Command::HeartRequest => {
                    self.write_control(Frame::new(Command::HeartResponse, frame.stream_id)).await?;
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
                Command::Alert => return Ok(()),
            }
        }
    }

    async fn receive_settings(&self, data: &[u8]) -> std::io::Result<()> {
        if self.is_client {
            return Ok(());
        }
        self.received_settings.store(true, std::sync::atomic::Ordering::Release);
        let map = string_map::from_bytes(data);
        let padding = self.padding.read().await;
        if map.get("padding-md5") != Some(&padding.md5) {
            let mut update = Frame::new(Command::UpdatePaddingScheme, 0);
            update.data = padding.raw_scheme().to_vec();
            self.write_control(update).await?;
        }
        if map.get("v").and_then(|value| value.parse::<u8>().ok()).unwrap_or(0) >= 2 {
            self.peer_version.store(2, std::sync::atomic::Ordering::Release);
            let mut settings = Frame::new(Command::ServerSettings, 0);
            settings.data = b"v=2".to_vec();
            self.write_control(settings).await?;
        }
        Ok(())
    }

    async fn close_remote_stream(self: &Arc<Self>, id: u32) -> std::io::Result<()> {
        self.write_control(Frame::new(Command::Fin, id)).await?;
        self.finish_stream(id).await;
        Ok(())
    }

    async fn finish_stream(self: &Arc<Self>, id: u32) {
        if let Some(mut entry) = self.streams.lock().await.remove(&id) {
            let _ = entry.writer.shutdown().await;
            entry.closed.notify_waiters();
        }
        if self.active_streams().await == 0
            && let Some(sender) = self.idle_sender.lock().await.as_ref()
        {
            let _ = sender.send(Arc::clone(self));
        }
    }

    async fn receive_syn(self: &Arc<Self>, id: u32) -> std::io::Result<()> {
        if self.is_client || self.incoming_sender.is_none() {
            return Ok(());
        }
        if !self.received_settings.load(std::sync::atomic::Ordering::Acquire) {
            let mut alert = Frame::new(Command::Alert, 0);
            alert.data = b"client did not send its settings".to_vec();
            self.write_control(alert).await?;
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "settings required before SYN"));
        }
        let (local, remote) = tokio::io::duplex(64 * 1024);
        {
            let mut streams = self.streams.lock().await;
            if streams.len() >= self.max_streams {
                let mut rejection = Frame::new(Command::SynAck, id);
                rejection.data = b"session stream limit reached".to_vec();
                self.write_control(rejection).await?;
                return Ok(());
            }
            streams.insert(
                id,
                StreamEntry {
                    writer: remote,
                    local_fin: false,
                    closed: Arc::new(Notify::new()),
                },
            );
        }
        let stream = Stream::new(id, Arc::clone(self), local);
        self.incoming_sender
            .as_ref()
            .unwrap()
            .send(stream)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stream receiver closed"))
    }
}

fn valid_stream_id(command: Command, stream_id: u32) -> bool {
    match command {
        Command::Syn | Command::Push | Command::Fin | Command::SynAck => stream_id > 0,
        Command::Waste | Command::Settings | Command::Alert | Command::UpdatePaddingScheme | Command::ServerSettings => stream_id == 0,
        Command::HeartRequest | Command::HeartResponse => stream_id == 0,
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
            self.packet_counter += 1;
            let factory = padding.read().await;
            if self.packet_counter < factory.stop {
                for size in factory.generate_record_payload_sizes(self.packet_counter) {
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
    session: Arc<Session>,
    io: tokio::io::DuplexStream,
    closed: bool,
    handshake_reported: std::sync::atomic::AtomicBool,
}

impl Stream {
    fn new(id: u32, session: Arc<Session>, io: tokio::io::DuplexStream) -> Self {
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

    #[cfg(test)]
    pub(crate) fn session(&self) -> Arc<Session> {
        Arc::clone(&self.session)
    }

    pub async fn read(&mut self, data: &mut [u8]) -> std::io::Result<usize> {
        self.io.read(data).await
    }

    pub async fn write(&self, data: &[u8]) -> std::io::Result<usize> {
        if self.closed {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stream closed"));
        }
        self.session.write_data(self.id, data).await
    }

    pub async fn handshake_success(&self) -> std::io::Result<()> {
        if self.session.peer_version.load(std::sync::atomic::Ordering::Acquire) >= 2
            && self
                .handshake_reported
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok()
        {
            self.session.write_control(Frame::new(Command::SynAck, self.id)).await?;
        }
        Ok(())
    }

    pub async fn handshake_failure(&self, error: &str) -> std::io::Result<()> {
        if self.session.peer_version.load(std::sync::atomic::Ordering::Acquire) >= 2
            && self
                .handshake_reported
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok()
        {
            let mut frame = Frame::new(Command::SynAck, self.id);
            frame.data = error.as_bytes().to_vec();
            self.session.write_control(frame).await?;
        }
        Ok(())
    }

    pub async fn close(&mut self) -> std::io::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.session.close_stream(self.id).await?;
        self.io.shutdown().await
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if !self.closed {
            self.closed = true;
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
        assert_eq!(client.writer.lock().await.packet_counter, 0);

        client_stream.write(b"hello").await.unwrap();
        assert_eq!(client.writer.lock().await.packet_counter, 1);
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
        let _ = server.close().await;
        let _ = client.close().await;
    }
}
