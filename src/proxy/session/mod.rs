#[cfg(feature = "client")]
pub mod client;
pub mod inner;
pub mod stream;

#[cfg(any(feature = "client", feature = "server"))]
use crate::AsyncReadWrite;
#[cfg(any(feature = "client", feature = "server"))]
use crate::core::PaddingFactory;
#[cfg(any(feature = "client", feature = "server"))]
use std::sync::Arc;
#[cfg(any(feature = "client", feature = "server"))]
use tokio::sync::RwLock;

#[cfg(feature = "client")]
pub use client::Client;
pub use inner::Session;
pub use stream::Stream;

#[cfg(feature = "client")]
pub async fn new_client_session(conn: Box<dyn AsyncReadWrite>, padding: Arc<RwLock<PaddingFactory>>) -> Session {
    crate::runtime::new_client_session(conn, padding).await
}

#[cfg(feature = "server")]
pub async fn new_server_session(
    conn: Box<dyn AsyncReadWrite>,
    on_new_stream: Box<dyn Fn(Arc<Stream>) + Send + Sync>,
    padding: Arc<RwLock<PaddingFactory>>,
    max_streams: usize,
) -> Session {
    crate::runtime::new_server_session(conn, on_new_stream, padding, max_streams).await
}
