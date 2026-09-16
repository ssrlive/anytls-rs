use crate::core::{Command, Frame};
use crate::proxy::pipe::{PipeReader, pipe};
use crate::runtime::{DataWrite, FrameWrite, MAX_QUEUED_FRAME_BYTES, MAX_QUEUED_INBOUND_BYTES, StreamProtocolHooks};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::{Mutex, Semaphore, mpsc, watch};

#[derive(Clone)]
pub(crate) enum HandshakeState {
    Pending,
    Succeeded,
    Failed(String),
}

/// A unit of work for a stream's inbound delivery pump. Delivery is decoupled
/// from the session's single receive loop so that one slow or backpressured
/// stream can never stall frame reading for the whole carrier (which would
/// otherwise starve SYNACK and heartbeat frames of unrelated streams).
enum InboundIntake {
    Data { data: Vec<u8> },
    Finish { error: Option<std::io::Error> },
}

pub struct Stream {
    id: u32,
    session_id: u64,
    pipe_reader: PipeReader,
    intake_tx: mpsc::UnboundedSender<InboundIntake>,
    data_enqueue_tx: mpsc::Sender<DataWrite>,
    write_budget: Arc<Semaphore>,
    streams: Weak<Mutex<HashMap<u32, Arc<Stream>>>>,
    idle_state: Weak<watch::Sender<bool>>,
    protocol_hooks: Arc<dyn StreamProtocolHooks>,
    read_closed: AtomicBool,
    write_closed: AtomicBool,
    terminated: AtomicBool,
    handshake: watch::Sender<HandshakeState>,
    aborted: tokio_util::sync::CancellationToken,
}

impl Stream {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: u32,
        session_id: u64,
        data_enqueue_tx: mpsc::Sender<DataWrite>,
        write_budget: Arc<Semaphore>,
        inbound_budget: Arc<Semaphore>,
        streams: Weak<Mutex<HashMap<u32, Arc<Stream>>>>,
        idle_state: Weak<watch::Sender<bool>>,
        protocol_hooks: Arc<dyn StreamProtocolHooks>,
    ) -> Self {
        let (pipe_reader, pipe_writer) = pipe();
        let (handshake, _) = watch::channel(HandshakeState::Pending);
        let aborted = tokio_util::sync::CancellationToken::new();
        let (intake_tx, mut intake_rx) = mpsc::unbounded_channel::<InboundIntake>();
        {
            // Per-stream delivery pump: drains inbound frames into the pipe
            // without ever blocking the session receive loop. Each queued item
            // carries its shared inbound-budget permit, so total buffered memory
            // stays bounded even though the intake channel itself is unbounded.
            let aborted = aborted.clone();
            let inbound_budget = inbound_budget.clone();
            tokio::spawn(async move {
                loop {
                    let item = tokio::select! {
                        biased;
                        _ = aborted.cancelled() => break,
                        item = intake_rx.recv() => item,
                    };
                    let Some(item) = item else { break };
                    match item {
                        InboundIntake::Data { data } => {
                            let permits = data.len().clamp(1, MAX_QUEUED_INBOUND_BYTES) as u32;
                            let permit = tokio::select! {
                                biased;
                                _ = aborted.cancelled() => break,
                                result = inbound_budget.clone().acquire_many_owned(permits) => {
                                    match result {
                                        Ok(permit) => permit,
                                        Err(_) => break,
                                    }
                                }
                            };
                            let write = pipe_writer.write_with_permit(&data, permit);
                            tokio::pin!(write);
                            tokio::select! {
                                biased;
                                _ = aborted.cancelled() => break,
                                result = &mut write => {
                                    if result.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                        InboundIntake::Finish { error } => {
                            pipe_writer.finish(error).await;
                            break;
                        }
                    }
                }
            });
        }
        Self {
            id,
            session_id,
            pipe_reader,
            intake_tx,
            data_enqueue_tx,
            write_budget,
            streams,
            idle_state,
            protocol_hooks,
            read_closed: AtomicBool::new(false),
            write_closed: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            handshake,
            aborted,
        }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn session_id(&self) -> u64 {
        self.session_id
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

    pub async fn wait_for_abort(&self) {
        self.aborted.cancelled().await;
    }

    pub async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.pipe_reader.read(buf).await
    }

    pub async fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        if self.write_closed.load(Ordering::Acquire) {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Stream closed"));
        }

        let writing = async {
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
        };
        tokio::select! {
            biased;
            _ = self.aborted.cancelled() => Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Stream aborted")),
            result = writing => result,
        }
    }

    pub async fn push_data(&self, buf: &[u8]) -> std::io::Result<usize> {
        if self.read_closed.load(Ordering::Acquire) {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Stream closed"));
        }
        let delivering = async {
            // Hand off to the per-stream pump without blocking the receive loop
            // on this stream's reader; the pump applies the shared budget before
            // writing to the pipe, preserving frame order without dropping data.
            self.intake_tx
                .send(InboundIntake::Data { data: buf.to_vec() })
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Stream closed"))?;
            Ok(buf.len())
        };
        tokio::select! {
            biased;
            _ = self.aborted.cancelled() => Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Stream aborted")),
            result = delivering => result,
        }
    }

    pub async fn close(&self) -> std::io::Result<()> {
        self.aborted.cancel();
        self.read_closed.store(true, Ordering::Release);
        self.pipe_reader.abort(None).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), self.shutdown_write())
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "Stream FIN enqueue timed out"))?
    }

    pub async fn shutdown_write(&self) -> std::io::Result<()> {
        if !self.mark_write_closed() {
            self.maybe_finalize().await;
            return Ok(());
        }

        self.maybe_finalize().await;
        let result = self
            .data_enqueue_tx
            .send(DataWrite {
                sid: self.id,
                frame: FrameWrite::new(Frame::new(Command::Fin, self.id), None, None),
            })
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
            // Route the end-of-stream through the same intake queue so it is
            // ordered strictly after any already-received PSH data, preserving
            // graceful half-close semantics. Fall back to a direct finish if the
            // pump has already stopped.
            if let Err(mpsc::error::SendError(InboundIntake::Finish { error })) = self.intake_tx.send(InboundIntake::Finish { error }) {
                self.pipe_reader.finish_stream(error).await;
            }
        }
        self.maybe_finalize().await;
    }

    pub(crate) async fn close_from_session(&self, error: Option<std::io::Error>) {
        if !self.terminated.swap(true, Ordering::AcqRel) {
            self.aborted.cancel();
            self.read_closed.store(true, Ordering::Release);
            self.write_closed.store(true, Ordering::Release);
            self.handshake.send_replace(HandshakeState::Failed(
                error
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "Session closed before SYNACK".to_string()),
            ));
            self.pipe_reader.abort(error).await;
        }
    }

    pub(crate) fn resolve_handshake(&self, error: Option<String>) -> bool {
        self.handshake.send_if_modified(|state| {
            if !matches!(state, HandshakeState::Pending) {
                return false;
            }
            *state = match error {
                Some(error) => HandshakeState::Failed(error),
                None => HandshakeState::Succeeded,
            };
            true
        })
    }

    pub async fn handshake_failure(&self, error: &str) -> std::io::Result<()> {
        log::debug!("session={} stream={} stage=synack_failure_submit", self.session_id, self.id);
        let result = self.protocol_hooks.handshake_failure(self.id, error).await;
        log::debug!(
            "session={} stream={} stage=synack_failure_complete result={result:?}",
            self.session_id,
            self.id
        );
        result
    }

    pub async fn handshake_success(&self) -> std::io::Result<()> {
        log::debug!("session={} stream={} stage=synack_success_submit", self.session_id, self.id);
        let result = self.protocol_hooks.handshake_success(self.id).await;
        log::debug!(
            "session={} stream={} stage=synack_success_complete result={result:?}",
            self.session_id,
            self.id
        );
        result
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
        self.acquire_budget(&self.write_budget, len, MAX_QUEUED_FRAME_BYTES).await
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
}

impl Drop for Stream {
    fn drop(&mut self) {
        // Guarantee the per-stream delivery pump wakes and exits even if the
        // stream is dropped without an explicit close while its pipe is full.
        self.aborted.cancel();
    }
}
