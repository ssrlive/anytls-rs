#[cfg(any(feature = "client", feature = "server"))]
mod cli;
#[cfg(feature = "client")]
mod client;
#[cfg(all(feature = "client", feature = "relay"))]
pub mod client_app;
#[cfg(feature = "core")]
mod core;
#[cfg(feature = "ffi")]
pub mod ffi;
#[cfg(feature = "client")]
mod log_callback;
#[cfg(feature = "server")]
mod panel_sync;
#[cfg(feature = "relay")]
pub mod relay;
#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "client")]
mod traffic_status;
#[cfg(feature = "uot")]
mod uot;
#[cfg(feature = "server")]
mod url_util;

#[cfg(feature = "uot")]
pub use ::socks5_impl::protocol::Address;
#[cfg(feature = "client")]
pub use cli::ClientArgs;
#[cfg(feature = "server")]
pub use cli::ServerArgs;
#[cfg(feature = "client")]
pub use client::{Client, Dialer};
#[cfg(feature = "core")]
pub use core::{
    AUTH_HEADER_SIZE, CHECK_MARK, Command, DEFAULT_SCHEME, Frame, HEADER_OVERHEAD_SIZE, MAX_FRAME_DATA_SIZE, PASSWORD_DIGEST_SIZE,
    PaddingFactory, StringMap, extract_client_id_from_padding, from_bytes, password_digest, to_bytes,
};
#[cfg(all(feature = "core", feature = "async"))]
pub use core::{read_auth, read_auth_with_client_id, write_auth, write_auth_with_client_id};
#[cfg(feature = "client")]
pub use log_callback::LogLevel;
#[cfg(feature = "server")]
pub use panel_sync::{PanelSyncClient, PanelSyncConfig, TrafficAudit, TrafficAuditPtr};
#[cfg(feature = "runtime")]
pub use runtime::{AsyncReadWrite, BoxTransport, DEFAULT_MAX_SESSION_AGE, Session, Stream, StreamIo, is_peer_disconnect};
#[cfg(feature = "client")]
pub use traffic_status::TrafficStatus;
#[cfg(feature = "uot")]
pub use uot::{
    UotMode, UotRequest, V2_MAGIC_ADDRESS, uot_encode_packet, uot_get_packet_from_stream, uot_get_request_from_stream,
    uot_is_sentinel_destination, uot_sentinel_destination,
};
#[cfg(feature = "server")]
pub use url_util::{args_json_for_public_ip, format_anytls_url, print_args, print_url};

pub const PROGRAM_VERSION_NAME: &str = concat!("anytls(rust)/", env!("CARGO_PKG_VERSION"));
pub const PROTOCOL_VERSION: u8 = 2;
