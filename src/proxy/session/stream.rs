use crate::core::{Command, Frame};
use crate::proxy::pipe::{PipeReader, PipeWriter, pipe};
use crate::runtime::StreamProtocolHooks;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::{Mutex, mpsc::Sender, oneshot, watch};

pub struct Stream {
    id: u32,
    pipe_reader: PipeReader,
    pipe_writer: PipeWriter,
    frame_tx: Sender<(Frame, Option<tokio::sync::oneshot::Sender<std::io::Result<()>>>)>,
    streams: Weak<Mutex<HashMap<u32, Arc<Stream>>>>,
    idle_state: Weak<watch::Sender<bool>>,
    protocol_hooks: Arc<dyn StreamProtocolHooks>,
    handshake_tx: Mutex<Option<oneshot::Sender<Result<(), String>>>>,
    handshake_rx: Mutex<Option<oneshot::Receiver<Result<(), String>>>>,
    closed: AtomicBool,
}

impl Stream {
    pub(crate) fn new(
        id: u32,
        frame_tx: Sender<(Frame, Option<tokio::sync::oneshot::Sender<std::io::Result<()>>>)>,
        streams: Weak<Mutex<HashMap<u32, Arc<Stream>>>>,
        idle_state: Weak<watch::Sender<bool>>,
        protocol_hooks: Arc<dyn StreamProtocolHooks>,
    ) -> Self {
        let (pipe_reader, pipe_writer) = pipe();
        let (handshake_tx, handshake_rx) = oneshot::channel();
        Self {
            id,
            pipe_reader,
            pipe_writer,
            frame_tx,
            streams,
            idle_state,
            protocol_hooks,
            handshake_tx: Mutex::new(Some(handshake_tx)),
            handshake_rx: Mutex::new(Some(handshake_rx)),
            closed: AtomicBool::new(false),
        }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub async fn is_terminated(&self) -> bool {
        self.closed.load(Ordering::Acquire) || self.frame_tx.is_closed()
    }

    pub async fn terminate(&self) -> std::io::Result<()> {
        self.close().await
    }

    pub async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.pipe_reader.read(buf).await
    }

    pub async fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        if self.closed.load(Ordering::Acquire) {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Stream closed"));
        }

        let frame = Frame::with_data(Command::Psh, self.id, bytes::Bytes::copy_from_slice(buf));
        self.frame_tx
            .send((frame, None))
            .await
            .map(|_| buf.len())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed"))
    }

    pub async fn wait_for_handshake(&self, timeout: std::time::Duration) -> std::io::Result<()> {
        let receiver = self
            .handshake_rx
            .lock()
            .await
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::AlreadyExists, "Handshake already awaited"))?;
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, error)),
            Ok(Err(_)) => Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed")),
            Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "Stream handshake timed out")),
        }
    }

    pub async fn push_data(&self, buf: &[u8]) -> std::io::Result<usize> {
        if self.closed.load(Ordering::Acquire) {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Stream closed"));
        }
        self.pipe_writer.write(buf).await
    }

    pub async fn close(&self) -> std::io::Result<()> {
        if !self.mark_closed() {
            return Ok(());
        }

        self.resolve_handshake("Stream closed".to_string()).await;
        self.remove_from_session().await;
        self.pipe_reader.close_with_error(None);
        self.frame_tx
            .send((Frame::new(Command::Fin, self.id), None))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed"))
    }

    pub(crate) async fn close_from_peer(&self, error: Option<std::io::Error>) {
        self.resolve_handshake(
            error
                .as_ref()
                .map_or_else(|| "Remote stream closed".to_string(), ToString::to_string),
        )
        .await;
        if self.mark_closed() {
            self.pipe_reader.finish_stream(error).await;
        }
    }

    pub(crate) async fn close_from_session(&self, error: Option<std::io::Error>) {
        self.resolve_handshake(error.as_ref().map_or_else(|| "Session closed".to_string(), ToString::to_string))
            .await;
        if self.mark_closed() {
            self.pipe_reader.finish_stream(error).await;
        }
    }

    pub async fn handshake_failure(&self, error: &str) -> std::io::Result<()> {
        self.protocol_hooks.handshake_failure(self.id, error).await
    }

    pub async fn handshake_success(&self) -> std::io::Result<()> {
        self.protocol_hooks.handshake_success(self.id).await
    }

    pub(crate) async fn resolve_handshake(&self, message: String) {
        if let Some(sender) = self.handshake_tx.lock().await.take() {
            let _ = sender.send(if message.is_empty() { Ok(()) } else { Err(message) });
        }
    }

    fn mark_closed(&self) -> bool {
        self.closed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    async fn remove_from_session(&self) {
        let Some(streams) = self.streams.upgrade() else {
            return;
        };

        let is_idle = {
            let mut streams = streams.lock().await;
            streams.remove(&self.id);
            streams.is_empty()
        };

        if is_idle && let Some(idle_state) = self.idle_state.upgrade() {
            idle_state.send_replace(true);
        }
    }
}
