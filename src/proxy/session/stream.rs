use crate::core::{Command, Frame};
use crate::proxy::pipe::{PipeReader, PipeWriter, pipe};
use crate::runtime::{DataWrite, FrameWrite, MAX_QUEUED_FRAME_BYTES, StreamProtocolHooks};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::{Mutex, Semaphore, mpsc::Sender, watch};

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
    control_tx: Sender<FrameWrite>,
    data_enqueue_tx: Sender<DataWrite>,
    write_budget: Arc<Semaphore>,
    streams: Weak<Mutex<HashMap<u32, Arc<Stream>>>>,
    idle_state: Weak<watch::Sender<bool>>,
    protocol_hooks: Arc<dyn StreamProtocolHooks>,
    read_closed: AtomicBool,
    write_closed: AtomicBool,
    terminated: AtomicBool,
    handshake: watch::Sender<HandshakeState>,
}

impl Stream {
    pub(crate) fn new(
        id: u32,
        control_tx: Sender<FrameWrite>,
        data_enqueue_tx: Sender<DataWrite>,
        write_budget: Arc<Semaphore>,
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
            control_tx,
            data_enqueue_tx,
            write_budget,
            streams,
            idle_state,
            protocol_hooks,
            read_closed: AtomicBool::new(false),
            write_closed: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            handshake,
        }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn is_closed(&self) -> bool {
        self.terminated.load(Ordering::Acquire)
    }

    pub fn is_read_closed(&self) -> bool {
        self.read_closed.load(Ordering::Acquire)
    }

    pub fn is_write_closed(&self) -> bool {
        self.write_closed.load(Ordering::Acquire)
    }

    pub async fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::Acquire) || self.data_enqueue_tx.is_closed()
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
        if self.write_closed.load(Ordering::Acquire) {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Stream closed"));
        }

        for chunk in buf.chunks(crate::core::MAX_FRAME_DATA_SIZE) {
            let frame = Frame::with_data(Command::Psh, self.id, bytes::Bytes::copy_from_slice(chunk));
            let budget = self.acquire_write_budget(chunk.len()).await?;
            self.data_enqueue_tx
                .send(DataWrite {
                    sid: self.id,
                    frame: FrameWrite::new(frame, None, Some(budget)),
                })
                .await
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed"))?;
        }

        Ok(buf.len())
    }

    pub async fn push_data(&self, buf: &[u8]) -> std::io::Result<usize> {
        if self.read_closed.load(Ordering::Acquire) {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Stream closed"));
        }
        self.pipe_writer.write(buf).await
    }

    pub async fn close(&self) -> std::io::Result<()> {
        self.read_closed.store(true, Ordering::Release);
        self.pipe_reader.close_with_error(None);
        self.shutdown_write().await
    }

    pub async fn shutdown_write(&self) -> std::io::Result<()> {
        if !self.mark_write_closed() {
            self.maybe_finalize().await;
            return Ok(());
        }

        let result = self
            .control_tx
            .send(FrameWrite::new(
                Frame::new(Command::Fin, self.id),
                None,
                Some(self.acquire_write_budget(0).await?),
            ))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed"));
        self.maybe_finalize().await;
        result
    }

    pub(crate) async fn close_from_peer(&self, error: Option<std::io::Error>) {
        if self.mark_read_closed() {
            let handshake_error = error
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "Stream closed before SYNACK".to_string());
            self.handshake.send_if_modified(|state| {
                if matches!(state, HandshakeState::Pending) {
                    *state = HandshakeState::Failed(handshake_error);
                    true
                } else {
                    false
                }
            });
            self.pipe_reader.finish_stream(error).await;
        }
        self.maybe_finalize().await;
    }

    pub(crate) async fn close_from_session(&self, error: Option<std::io::Error>) {
        if !self.terminated.swap(true, Ordering::AcqRel) {
            self.read_closed.store(true, Ordering::Release);
            self.write_closed.store(true, Ordering::Release);
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

    fn mark_read_closed(&self) -> bool {
        self.read_closed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn mark_write_closed(&self) -> bool {
        self.write_closed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    async fn maybe_finalize(&self) {
        if !self.read_closed.load(Ordering::Acquire) || !self.write_closed.load(Ordering::Acquire) {
            return;
        }
        if self.terminated.swap(true, Ordering::AcqRel) {
            return;
        }

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

    async fn acquire_write_budget(&self, len: usize) -> std::io::Result<tokio::sync::OwnedSemaphorePermit> {
        let permits = len.clamp(1, MAX_QUEUED_FRAME_BYTES) as u32;
        self.write_budget
            .clone()
            .acquire_many_owned(permits)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session write budget closed"))
    }
}
