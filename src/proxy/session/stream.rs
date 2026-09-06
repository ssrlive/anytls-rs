use crate::core::{Command, Frame};
use crate::proxy::pipe::{PipeReader, PipeWriter, pipe};
use crate::runtime::StreamProtocolHooks;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::{Mutex, mpsc::Sender, watch};

#[derive(Clone)]
pub(crate) enum HandshakeState {
    Pending,
    Succeeded,
    Failed(String),
}

pub struct Stream {
    id: u32,
    pipe_reader: PipeReader,
    pipe_writer: PipeWriter,
    frame_tx: Sender<(Frame, Option<tokio::sync::oneshot::Sender<std::io::Result<()>>>)>,
    streams: Weak<Mutex<HashMap<u32, Arc<Stream>>>>,
    idle_state: Weak<watch::Sender<bool>>,
    protocol_hooks: Arc<dyn StreamProtocolHooks>,
    closed: AtomicBool,
    handshake: watch::Sender<HandshakeState>,
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
        let (handshake, _) = watch::channel(HandshakeState::Pending);
        Self {
            id,
            pipe_reader,
            pipe_writer,
            frame_tx,
            streams,
            idle_state,
            protocol_hooks,
            closed: AtomicBool::new(false),
            handshake,
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

    pub async fn wait_for_handshake(&self) -> std::io::Result<()> {
        let mut handshake = self.handshake.subscribe();
        loop {
            let state = handshake.borrow().clone();
            match state {
                HandshakeState::Succeeded => return Ok(()),
                HandshakeState::Failed(error) => return Err(std::io::Error::other(error)),
                HandshakeState::Pending if self.is_terminated().await => {
                    return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Stream closed before SYNACK"));
                }
                HandshakeState::Pending => {}
            }

            if handshake.changed().await.is_err() {
                return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Stream closed before SYNACK"));
            }
        }
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

        for chunk in buf.chunks(crate::core::MAX_FRAME_DATA_SIZE) {
            let frame = Frame::with_data(Command::Psh, self.id, bytes::Bytes::copy_from_slice(chunk));
            self.frame_tx
                .send((frame, None))
                .await
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed"))?;
        }

        Ok(buf.len())
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

        self.remove_from_session().await;
        self.pipe_reader.close_with_error(None);
        self.frame_tx
            .send((Frame::new(Command::Fin, self.id), None))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed"))
    }

    pub(crate) async fn close_from_peer(&self, error: Option<std::io::Error>) {
        if self.mark_closed() {
            self.handshake.send_replace(HandshakeState::Failed(
                error
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "Stream closed before SYNACK".to_string()),
            ));
            self.pipe_reader.finish_stream(error).await;
        }
    }

    pub(crate) async fn close_from_session(&self, error: Option<std::io::Error>) {
        if self.mark_closed() {
            self.handshake.send_replace(HandshakeState::Failed(
                error
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "Session closed before SYNACK".to_string()),
            ));
            self.pipe_reader.finish_stream(error).await;
        }
    }

    pub(crate) fn resolve_handshake(&self, error: Option<String>) {
        self.handshake.send_replace(match error {
            Some(error) => HandshakeState::Failed(error),
            None => HandshakeState::Succeeded,
        });
    }

    pub async fn handshake_failure(&self, error: &str) -> std::io::Result<()> {
        self.protocol_hooks.handshake_failure(self.id, error).await
    }

    pub async fn handshake_success(&self) -> std::io::Result<()> {
        self.protocol_hooks.handshake_success(self.id).await
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
