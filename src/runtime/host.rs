use crate::core::{Frame, State};
use async_trait::async_trait;
use bytes::Bytes;
use std::sync::Arc;

#[async_trait]
pub trait ProtocolHost: Send + Sync {
    fn is_client(&self) -> bool;

    fn protocol_state(&self) -> Arc<State>;

    async fn send_frame(&self, frame: Frame) -> std::io::Result<usize>;

    async fn send_frame_sync(&self, frame: Frame) -> std::io::Result<usize>;

    async fn push_stream_data(&self, sid: u32, data: Bytes) -> std::io::Result<()>;

    async fn ensure_incoming_stream(&self, sid: u32) -> std::io::Result<()>;
    // Notify that the logical stream identified by `sid` has been closed
    // locally (i.e. peer sent FIN). This indicates the logical flow is idle
    // but the underlying session may remain active and reusable.
    async fn close_logical_stream(&self, sid: u32) -> std::io::Result<()>;

    // Terminate the entire session. This is used when the remote peer has
    // indicated a protocol-level termination or when an unrecoverable error
    // occurs. `message` may contain an optional reason provided by the peer.
    async fn terminate_session(&self, sid: u32, message: Option<String>) -> std::io::Result<()>;

    async fn resolve_stream_handshake(&self, sid: u32, message: String) -> std::io::Result<()>;
    async fn release_write_buffering(&self);
}
