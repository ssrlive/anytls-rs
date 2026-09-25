use tokio::io::{AsyncRead, AsyncWrite};

pub(crate) mod session;
mod stream_io;

pub use session::{Session, Stream, is_peer_disconnect};

pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncReadWrite for T {}

pub type BoxTransport = Box<dyn AsyncReadWrite>;

pub use stream_io::StreamIo;
