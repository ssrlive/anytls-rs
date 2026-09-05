#[cfg_attr(feature = "core", doc = "Core module")]
pub mod core;

#[cfg(feature = "runtime")]
pub mod proxy;
#[cfg(feature = "runtime")]
pub mod runtime;
#[cfg(feature = "uot")]
pub mod uot;
#[cfg(feature = "runtime")]
use futures::future::BoxFuture;
#[cfg(feature = "runtime")]
use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(feature = "runtime")]
pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
#[cfg(feature = "runtime")]
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
#[cfg(feature = "runtime")]
pub type DialOutFunc = Box<dyn Fn() -> BoxFuture<'static, std::io::Result<Box<dyn AsyncReadWrite>>> + Send + Sync>;

#[cfg(any(feature = "server", feature = "client"))]
mod util;

#[cfg(any(feature = "server", feature = "client"))]
pub use util::parse_url::ClientRuntimeConfig;

#[cfg(any(feature = "server", feature = "client"))]
pub use ::socks5_impl::protocol::Address;

#[cfg(feature = "server")]
pub mod panel_sync;

#[cfg(any(feature = "server", feature = "client"))]
pub use util::*;

#[cfg(feature = "client_runner")]
mod client_runner;
#[cfg(feature = "client_runner")]
pub use client_runner::{ClientArgs, resolve_client_config, runner_execute};

#[cfg(feature = "client_runner")]
pub use ::clap::Parser as ClapParser;

#[cfg(feature = "client_runner")]
pub use ::socks5_impl::protocol::{ProxyParameters, ProxyType, UserKey};
#[cfg(feature = "client_runner")]
pub use ::tokio_util::sync::CancellationToken;

pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

pub const PROGRAM_VERSION_NAME: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

// Protocol version number (exported). Increment when making incompatible
// protocol changes that affect the "v" settings field used during session
// negotiation.
pub const PROTOCOL_VERSION: u8 = 2;
// Minimum peer version required for the version-2 protocol features.
pub const MIN_PROTOCOL_VERSION: u8 = 2;
