#[cfg(any(feature = "client", feature = "server"))]
use crate::AsyncReadWrite;
#[cfg(any(feature = "client", feature = "server"))]
use crate::MIN_PROTOCOL_VERSION;
#[cfg(any(feature = "client", feature = "server"))]
use crate::core::CHECK_MARK;
#[cfg(any(feature = "client", feature = "server"))]
use crate::core::Engine;
#[cfg(any(feature = "client", feature = "server"))]
use crate::core::PaddingFactory;
#[cfg(any(feature = "client", feature = "server"))]
use crate::core::ProtocolAction;
#[cfg(any(feature = "client", feature = "server"))]
use crate::core::State;
#[cfg(any(feature = "client", feature = "server"))]
use crate::core::{Command, Frame, HEADER_OVERHEAD_SIZE};
#[cfg(any(feature = "client", feature = "server"))]
use crate::proxy::session::Session;
#[cfg(feature = "server")]
use crate::proxy::session::Stream;
#[cfg(any(feature = "client", feature = "server"))]
use async_trait::async_trait;
#[cfg(any(feature = "client", feature = "server"))]
use bytes::Bytes;
#[cfg(any(feature = "client", feature = "server"))]
use parking_lot::Mutex as BlockingMutex;
#[cfg(any(feature = "client", feature = "server"))]
use std::collections::{HashMap, VecDeque};
#[cfg(any(feature = "client", feature = "server"))]
use std::sync::Arc;
#[cfg(any(feature = "client", feature = "server"))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(any(feature = "client", feature = "server"))]
use tokio::io::AsyncWriteExt;
#[cfg(any(feature = "client", feature = "server"))]
use tokio::sync::Mutex;
#[cfg(any(feature = "client", feature = "server"))]
use tokio::sync::RwLock;
#[cfg(any(feature = "client", feature = "server"))]
use tokio::sync::mpsc::{Receiver, Sender};

pub mod host;
pub mod padding;

pub use host::ProtocolHost;
pub use padding::DefaultPaddingFactory;

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) const MAX_QUEUED_FRAME_BYTES: usize = 4 * 1024 * 1024;

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) struct FrameWrite {
    pub(crate) frame: Frame,
    pub(crate) ack: Option<tokio::sync::oneshot::Sender<std::io::Result<()>>>,
    pub(crate) budget: Option<tokio::sync::OwnedSemaphorePermit>,
}

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) struct DataWrite {
    pub(crate) sid: u32,
    pub(crate) frame: FrameWrite,
}

#[cfg(any(feature = "client", feature = "server"))]
impl FrameWrite {
    pub(crate) fn new(
        frame: Frame,
        ack: Option<tokio::sync::oneshot::Sender<std::io::Result<()>>>,
        budget: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> Self {
        Self { frame, ack, budget }
    }
}

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) struct WriterRuntimeState {
    send_padding: Arc<Mutex<bool>>,
    buffering: Arc<Mutex<bool>>,
    buffer: Arc<Mutex<Vec<u8>>>,
    pkt_counter: Arc<Mutex<u32>>,
    failed: Arc<AtomicBool>,
    failure_notify: Arc<tokio::sync::Notify>,
}

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) fn spawn_data_scheduler(mut input: Receiver<DataWrite>, output: Sender<FrameWrite>) {
    tokio::spawn(async move {
        let mut queues: HashMap<u32, VecDeque<FrameWrite>> = HashMap::new();
        let mut ready = VecDeque::new();
        let mut input_open = true;

        while input_open || !ready.is_empty() {
            for _ in 0..32 {
                if !input_open {
                    break;
                }
                match input.try_recv() {
                    Ok(DataWrite { sid, frame }) => {
                        let queue = queues.entry(sid).or_default();
                        if queue.is_empty() {
                            ready.push_back(sid);
                        }
                        queue.push_back(frame);
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => input_open = false,
                }
            }

            if let Some(sid) = ready.pop_front() {
                let Some(queue) = queues.get_mut(&sid) else {
                    continue;
                };
                let frame = queue.pop_front().expect("ready data queue must not be empty");
                if queue.is_empty() {
                    queues.remove(&sid);
                } else {
                    ready.push_back(sid);
                }
                if output.send(frame).await.is_err() {
                    return;
                }
                continue;
            }

            match input.recv().await {
                Some(DataWrite { sid, frame }) => {
                    let queue = queues.entry(sid).or_default();
                    if queue.is_empty() {
                        ready.push_back(sid);
                    }
                    queue.push_back(frame);
                }
                None => input_open = false,
            }
        }
    });
}

#[cfg(any(feature = "client", feature = "server"))]
impl WriterRuntimeState {
    pub(crate) fn new(is_client: bool) -> Arc<Self> {
        Arc::new(Self {
            send_padding: Arc::new(Mutex::new(is_client)),
            buffering: Arc::new(Mutex::new(false)),
            buffer: Arc::new(Mutex::new(Vec::new())),
            pkt_counter: Arc::new(Mutex::new(0)),
            failed: Arc::new(AtomicBool::new(false)),
            failure_notify: Arc::new(tokio::sync::Notify::new()),
        })
    }

    pub(crate) async fn is_send_padding_enabled(&self) -> bool {
        *self.send_padding.lock().await
    }

    pub(crate) async fn disable_send_padding(&self) {
        *self.send_padding.lock().await = false;
    }

    pub(crate) async fn is_buffering(&self) -> bool {
        *self.buffering.lock().await
    }

    pub(crate) async fn set_buffering(&self, enabled: bool) {
        *self.buffering.lock().await = enabled;
    }

    pub(crate) async fn append_buffered_bytes(&self, bytes: &[u8]) {
        self.buffer.lock().await.extend_from_slice(bytes);
    }

    pub(crate) async fn take_buffered_bytes(&self) -> Vec<u8> {
        let mut pending = self.buffer.lock().await;
        std::mem::take(&mut *pending)
    }

    pub(crate) async fn next_packet_counter(&self) -> u32 {
        let mut counter = self.pkt_counter.lock().await;
        *counter += 1;
        *counter
    }

    pub(crate) fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub(crate) fn failure_notified(&self) -> Arc<tokio::sync::Notify> {
        self.failure_notify.clone()
    }

    pub(crate) fn mark_failed(&self) {
        if !self.failed.swap(true, Ordering::AcqRel) {
            self.failure_notify.notify_waiters();
        }
    }
}

#[cfg(feature = "client")]
pub(crate) async fn new_client_session(conn: Box<dyn AsyncReadWrite>, padding: Arc<RwLock<PaddingFactory>>) -> Session {
    let protocol: Arc<dyn Protocol> = Arc::new(AnyTlsProtocol);
    let protocol_state = State::new(padding.read().await.clone());
    let writer_state = WriterRuntimeState::new(true);
    Session::new_with_protocol(conn, true, None, protocol, protocol_state, writer_state)
}

#[cfg(feature = "server")]
pub(crate) async fn new_server_session(
    conn: Box<dyn AsyncReadWrite>,
    on_new_stream: Box<dyn Fn(Arc<Stream>) + Send + Sync>,
    padding: Arc<RwLock<PaddingFactory>>,
    max_streams: usize,
) -> Session {
    let protocol: Arc<dyn Protocol> = Arc::new(AnyTlsProtocol);
    let protocol_state = State::new(padding.read().await.clone());
    let writer_state = WriterRuntimeState::new(false);
    let session = Session::new_with_protocol(conn, false, Some(on_new_stream), protocol, protocol_state, writer_state);
    session.set_max_incoming_streams(max_streams);
    session
}

#[cfg(any(feature = "client", feature = "server"))]
#[async_trait]
pub(crate) trait StreamProtocolHooks: Send + Sync {
    async fn handshake_failure(&self, sid: u32, error: &str) -> std::io::Result<()>;

    async fn handshake_success(&self, sid: u32) -> std::io::Result<()>;
}

#[cfg(any(feature = "client", feature = "server"))]
#[async_trait]
pub(crate) trait Protocol: Send + Sync {
    fn spawn_writer_task(
        &self,
        writer: tokio::io::WriteHalf<Box<dyn AsyncReadWrite>>,
        control_rx: Receiver<FrameWrite>,
        data_rx: Receiver<FrameWrite>,
        state: Arc<State>,
        writer_state: Arc<WriterRuntimeState>,
    );

    fn make_stream_protocol_hooks(&self, control_tx: Sender<FrameWrite>, state: Arc<State>) -> Arc<dyn StreamProtocolHooks>;

    async fn on_session_start(&self, host: &dyn ProtocolHost) -> std::io::Result<()>;

    async fn handle_frame(&self, host: &dyn ProtocolHost, frame: Frame) -> std::io::Result<()>;
}

#[cfg(any(feature = "client", feature = "server"))]
#[derive(Default)]
pub(crate) struct AnyTlsProtocol;

#[cfg(any(feature = "client", feature = "server"))]
struct AnyTlsStreamProtocolHooks {
    control_tx: Sender<FrameWrite>,
    peer_version: Arc<BlockingMutex<u8>>,
}

#[async_trait]
#[cfg(any(feature = "client", feature = "server"))]
impl StreamProtocolHooks for AnyTlsStreamProtocolHooks {
    async fn handshake_failure(&self, sid: u32, error: &str) -> std::io::Result<()> {
        if *self.peer_version.lock() >= MIN_PROTOCOL_VERSION {
            let frame = Frame::with_data(Command::SynAck, sid, bytes::Bytes::copy_from_slice(error.as_bytes()));
            match self.control_tx.send(FrameWrite::new(frame, None, None)).await {
                Ok(_) => {}
                Err(_) => return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed")),
            }
        }

        Ok(())
    }

    async fn handshake_success(&self, sid: u32) -> std::io::Result<()> {
        if *self.peer_version.lock() >= MIN_PROTOCOL_VERSION {
            let frame = Frame::new(Command::SynAck, sid);
            match self.control_tx.send(FrameWrite::new(frame, None, None)).await {
                Ok(_) => {}
                Err(_) => return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Session closed")),
            }
        }

        Ok(())
    }
}

#[cfg(any(feature = "client", feature = "server"))]
impl AnyTlsProtocol {
    async fn write_conn(
        writer: &mut tokio::io::WriteHalf<Box<dyn AsyncReadWrite>>,
        mut bytes: Vec<u8>,
        state: &Arc<State>,
        writer_state: &Arc<WriterRuntimeState>,
    ) -> std::io::Result<usize> {
        if writer_state.is_buffering().await {
            writer_state.append_buffered_bytes(&bytes).await;
            return Ok(bytes.len());
        }

        {
            let pending = writer_state.take_buffered_bytes().await;
            if !pending.is_empty() {
                let mut combined = Vec::with_capacity(pending.len() + bytes.len());
                combined.extend_from_slice(&pending);
                combined.extend_from_slice(&bytes);
                bytes = combined;
            }
        }

        let payload_len = bytes.len();

        if writer_state.is_send_padding_enabled().await {
            let pkt = writer_state.next_packet_counter().await;

            let padding_factory = state.padding();
            if pkt < padding_factory.stop() {
                for spec in padding_factory.generate_record_payload_sizes(pkt) {
                    let remain_payload_len = bytes.len();

                    if spec == CHECK_MARK {
                        if remain_payload_len == 0 {
                            break;
                        }
                        continue;
                    }

                    let frame_len = spec.max(0) as usize;
                    if remain_payload_len > frame_len {
                        writer.write_all(&bytes[..frame_len]).await?;
                        bytes.drain(0..frame_len);
                    } else if remain_payload_len > 0 {
                        let padding_len = frame_len.saturating_sub(remain_payload_len).saturating_sub(HEADER_OVERHEAD_SIZE);
                        if padding_len > 0 {
                            let mut padding_frame = vec![0u8; HEADER_OVERHEAD_SIZE + padding_len];
                            padding_frame[0] = Command::Waste.into();
                            padding_frame[5..7].copy_from_slice(&(padding_len as u16).to_be_bytes());
                            bytes.extend_from_slice(&padding_frame);
                        }
                        writer.write_all(&bytes).await?;
                        bytes.clear();
                    } else {
                        let mut padding_frame = vec![0u8; HEADER_OVERHEAD_SIZE + frame_len];
                        padding_frame[0] = Command::Waste.into();
                        padding_frame[5..7].copy_from_slice(&(frame_len as u16).to_be_bytes());
                        writer.write_all(&padding_frame).await?;
                    }
                }

                if bytes.is_empty() {
                    return Ok(payload_len);
                }
            } else {
                writer_state.disable_send_padding().await;
            }
        }

        writer.write_all(&bytes).await?;
        Ok(payload_len)
    }

    async fn apply_actions(&self, host: &dyn ProtocolHost, actions: Vec<ProtocolAction>) -> std::io::Result<()> {
        for action in actions {
            match action {
                ProtocolAction::SendFrame(frame) => {
                    log::debug!("apply_actions: SendFrame {}", frame);
                    host.send_frame(frame).await?;
                }
                ProtocolAction::SendFrameSync(frame) => {
                    log::debug!("apply_actions: SendFrameSync {}", frame);
                    host.send_frame_sync(frame).await?;
                }
                ProtocolAction::PushStreamData { sid, data } => {
                    log::debug!("apply_actions: PushStreamData sid={} len={}", sid, data.len());
                    host.push_stream_data(sid, data).await?;
                }
                ProtocolAction::EnsureIncomingStream { sid } => {
                    log::debug!("apply_actions: EnsureIncomingStream sid={}", sid);
                    if let Err(error) = host.ensure_incoming_stream(sid).await {
                        if error.kind() == std::io::ErrorKind::WouldBlock {
                            let frame = Frame::with_data(Command::SynAck, sid, Bytes::copy_from_slice(b"session stream limit reached"));
                            host.send_frame(frame).await?;
                        } else {
                            return Err(error);
                        }
                    }
                }
                ProtocolAction::CloseLocalStream { sid } => {
                    log::debug!("apply_actions: CloseLocalStream sid={}", sid);
                    host.close_logical_stream(sid).await?;
                }
                ProtocolAction::CloseRemoteStream { sid, message } => {
                    log::debug!("apply_actions: CloseRemoteStream sid={} message={}", sid, message);
                    host.terminate_session(sid, Some(message)).await?;
                }
                // synack timeout actions removed — no-op
                ProtocolAction::ReleaseWriteBuffering => {
                    log::debug!("apply_actions: ReleaseWriteBuffering");
                    host.release_write_buffering().await;
                }
                ProtocolAction::AlertAndFail { message } => {
                    log::debug!("apply_actions: AlertAndFail message={}", message);
                    let frame = Frame::with_data(Command::Alert, 0, bytes::Bytes::copy_from_slice(message.as_bytes()));
                    let _ = host.send_frame_sync(frame).await;
                    return Err(std::io::Error::other(message));
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
#[cfg(any(feature = "client", feature = "server"))]
impl Protocol for AnyTlsProtocol {
    fn spawn_writer_task(
        &self,
        mut writer: tokio::io::WriteHalf<Box<dyn AsyncReadWrite>>,
        mut control_rx: Receiver<FrameWrite>,
        mut data_rx: Receiver<FrameWrite>,
        state: Arc<State>,
        writer_state: Arc<WriterRuntimeState>,
    ) {
        let writer_state_for_task = writer_state.clone();
        tokio::spawn(async move {
            let mut control_open = true;
            let mut data_open = true;
            while control_open || data_open {
                let next = tokio::select! {
                    biased;
                    frame = control_rx.recv(), if control_open => {
                        if frame.is_none() { control_open = false; }
                        frame
                    }
                    frame = data_rx.recv(), if data_open => {
                        if frame.is_none() { data_open = false; }
                        frame
                    }
                };
                let Some(FrameWrite {
                    frame,
                    ack,
                    budget: _budget,
                }) = next
                else {
                    continue;
                };
                let res = async {
                    if frame.cmd == Command::SynAck {
                        log::debug!("Writing SYNACK frame sid={}", frame.sid);
                    }
                    Self::write_conn(&mut writer, frame.to_bytes().to_vec(), &state, &writer_state).await?;
                    writer.flush().await
                }
                .await;

                if let Some(ack_tx) = ack {
                    let _ = ack_tx.send(if res.is_ok() {
                        Ok(())
                    } else {
                        Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Write failed"))
                    });
                }

                if let Err(error) = res {
                    log::warn!("Failed to write frame to peer: {error}");
                    writer_state_for_task.mark_failed();
                    break;
                }
            }
            writer_state_for_task.mark_failed();
            log::debug!("Session writer task exiting (writer loop ended)");
        });
    }

    fn make_stream_protocol_hooks(&self, control_tx: Sender<FrameWrite>, state: Arc<State>) -> Arc<dyn StreamProtocolHooks> {
        Arc::new(AnyTlsStreamProtocolHooks {
            control_tx,
            peer_version: state.peer_version_handle(),
        })
    }

    async fn on_session_start(&self, host: &dyn ProtocolHost) -> std::io::Result<()> {
        let actions = Engine::on_session_start(&host.protocol_state(), host.is_client(), crate::PROGRAM_VERSION_NAME)?;
        self.apply_actions(host, actions).await
    }

    async fn handle_frame(&self, host: &dyn ProtocolHost, frame: Frame) -> std::io::Result<()> {
        let should_warn = matches!(frame.cmd, Command::Unknown(_));

        if frame.cmd == Command::Alert {
            if !frame.data.is_empty() {
                let message = String::from_utf8_lossy(frame.data.as_ref());
                log::error!("Alert from server: {}", message);
            }
            return Err(std::io::Error::other("Alert received"));
        }

        if host.is_client() && frame.cmd == Command::SynAck {
            log::debug!("Received SYNACK frame sid={} len={}", frame.sid, frame.data.len());
            let message = String::from_utf8_lossy(frame.data.as_ref()).to_string();
            host.resolve_stream_handshake(frame.sid, message.clone()).await?;
            return Ok(());
        }

        if should_warn {
            log::warn!(
                "Session received unexpected command: cmd={}, sid={}, len={}",
                frame.cmd,
                frame.sid,
                frame.data.len()
            );
        }

        let actions = Engine::on_frame(&host.protocol_state(), host.is_client(), &frame)?;
        self.apply_actions(host, actions).await
    }
}

#[cfg(all(test, any(feature = "client", feature = "server")))]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn data_scheduler_round_robins_active_streams() {
        let (input_tx, input_rx) = mpsc::channel(8);
        let (output_tx, mut output_rx) = mpsc::channel(8);
        spawn_data_scheduler(input_rx, output_tx);

        for sid in [1, 1, 1, 2] {
            input_tx
                .send(DataWrite {
                    sid,
                    frame: FrameWrite::new(Frame::new(Command::Psh, sid), None, None),
                })
                .await
                .unwrap();
        }
        drop(input_tx);

        let mut sent_sids = Vec::new();
        while let Some(frame) = output_rx.recv().await {
            sent_sids.push(frame.frame.sid);
        }

        assert_eq!(sent_sids, vec![1, 2, 1, 1]);
    }
}
