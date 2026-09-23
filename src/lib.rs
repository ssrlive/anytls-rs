pub mod auth;
pub mod client;
pub mod frame;
pub mod padding;
pub mod session;
pub mod stream_io;
pub mod string_map;
pub mod uot;

pub const PROGRAM_VERSION_NAME: &str = "anytls/0.1.0";
pub const PROTOCOL_VERSION: u8 = 2;
