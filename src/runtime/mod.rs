use tokio::io::{AsyncRead, AsyncWrite};

mod stream_io;

pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncReadWrite for T {}

pub type BoxTransport = Box<dyn AsyncReadWrite>;

pub use stream_io::StreamIo;
