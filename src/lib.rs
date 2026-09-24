#[cfg(any(feature = "client", feature = "server"))]
mod cli;
#[cfg(feature = "core")]
mod core;
#[cfg(feature = "server")]
mod panel_sync;
#[cfg(feature = "runtime")]
mod proxy;
#[cfg(feature = "runtime")]
mod runtime;
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
#[cfg(feature = "core")]
pub use core::{
    AUTH_HEADER_SIZE, CHECK_MARK, Command, DEFAULT_SCHEME, Frame, HEADER_OVERHEAD_SIZE, MAX_FRAME_DATA_SIZE, PASSWORD_DIGEST_SIZE,
    PaddingFactory, StringMap, extract_client_id_from_padding, from_bytes, password_digest, read_auth, read_auth_with_client_id, to_bytes,
    write_auth, write_auth_with_client_id,
};
#[cfg(feature = "server")]
pub use panel_sync::{PanelSyncClient, PanelSyncConfig, TrafficAudit, TrafficAuditPtr};
#[cfg(feature = "client")]
pub use proxy::{Client, Dialer};
#[cfg(feature = "runtime")]
pub use proxy::{Session, Stream, is_peer_disconnect};
#[cfg(feature = "runtime")]
pub use runtime::{AsyncReadWrite, BoxTransport, StreamIo};
#[cfg(feature = "uot")]
pub use uot::{
    UotMode, UotRequest, V2_MAGIC_ADDRESS, uot_encode_packet, uot_get_packet_from_stream, uot_get_request_from_stream,
    uot_is_sentinel_destination, uot_sentinel_destination,
};
#[cfg(feature = "server")]
pub use url_util::{args_json_for_public_ip, format_anytls_url, print_args, print_url};

pub const PROGRAM_VERSION_NAME: &str = "anytls/0.1.0";
pub const PROTOCOL_VERSION: u8 = 2;
