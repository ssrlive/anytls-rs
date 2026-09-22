use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::session::Stream;

pub struct StreamIo {
    stream: Arc<Stream>,
    #[allow(clippy::type_complexity)]
    read_future: Option<Pin<Box<dyn Future<Output = (std::io::Result<usize>, Vec<u8>)> + Send>>>,
    write_future: Option<Pin<Box<dyn Future<Output = std::io::Result<usize>> + Send>>>,
}

impl StreamIo {
    pub fn new(stream: Stream) -> Self {
        Self {
            stream: Arc::new(stream),
            read_future: None,
            write_future: None,
        }
    }

    pub async fn handshake_success(&mut self) -> std::io::Result<()> {
        self.stream.handshake_success().await
    }

    pub async fn handshake_failure(&mut self, error: &str) -> std::io::Result<()> {
        self.stream.handshake_failure(error).await
    }
}

impl Clone for StreamIo {
    fn clone(&self) -> Self {
        Self {
            stream: Arc::clone(&self.stream),
            read_future: None,
            write_future: None,
        }
    }
}

impl AsyncRead for StreamIo {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        if self.read_future.is_none() {
            let stream = Arc::clone(&self.stream);
            let capacity = buf.remaining();
            self.read_future = Some(Box::pin(async move {
                let mut data = vec![0u8; capacity];
                let result = stream.read(&mut data).await;
                (result, data)
            }));
        }

        match self.read_future.as_mut().expect("read future installed").as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready((Ok(size), data)) => {
                self.read_future = None;
                buf.put_slice(&data[..size]);
                Poll::Ready(Ok(()))
            }
            Poll::Ready((Err(error), _)) => {
                self.read_future = None;
                Poll::Ready(Err(error))
            }
        }
    }
}

impl AsyncWrite for StreamIo {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.write_future.is_none() {
            let stream = Arc::clone(&this.stream);
            let data = data.to_vec();
            this.write_future = Some(Box::pin(async move { stream.write(&data).await }));
        }
        match this.write_future.as_mut().expect("write future installed").as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                this.write_future = None;
                Poll::Ready(result)
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
