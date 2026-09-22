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
    shutdown_future: Option<Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>>,
    shutdown_complete: bool,
}

impl StreamIo {
    pub fn new(stream: Stream) -> Self {
        Self {
            stream: Arc::new(stream),
            read_future: None,
            write_future: None,
            shutdown_future: None,
            shutdown_complete: false,
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
            shutdown_future: None,
            shutdown_complete: false,
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

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.shutdown_complete {
            return Poll::Ready(Ok(()));
        }
        if self.shutdown_future.is_none() {
            let stream = Arc::clone(&self.stream);
            self.shutdown_future = Some(Box::pin(async move { stream.shutdown_write().await }));
        }
        match self.shutdown_future.as_mut().expect("shutdown future installed").as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.shutdown_future = None;
                if result.is_ok() {
                    self.shutdown_complete = true;
                }
                Poll::Ready(result)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{padding::PaddingFactory, session::Session};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn shutdown_sends_fin_and_peer_observes_eof() {
        let (client_io, server_io) = tokio::io::duplex(128 * 1024);
        let padding = Arc::new(tokio::sync::RwLock::new(
            PaddingFactory::new(crate::padding::DEFAULT_SCHEME).unwrap(),
        ));
        let client = Session::new_client(1, Box::new(client_io), Arc::clone(&padding), 1);
        let server = Session::new_server(1, Box::new(server_io), padding, 1);
        client.run().await.unwrap();
        server.run().await.unwrap();

        let stream = client.open_stream().await.unwrap();
        let mut peer = server.accept_stream().await.unwrap();
        let mut io = StreamIo::new(stream);
        io.write_all(b"payload").await.unwrap();
        io.shutdown().await.unwrap();

        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), async {
            let mut buffer = [0u8; 32];
            loop {
                let size = peer.read(&mut buffer).await.unwrap();
                if size == 0 {
                    break;
                }
                received.extend_from_slice(&buffer[..size]);
            }
        })
        .await
        .expect("peer should observe FIN");
        assert_eq!(received, b"payload");

        peer.write(b"response").await.unwrap();
        peer.close().await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), io.read_to_end(&mut response))
            .await
            .expect("response should still flow after write-side shutdown")
            .unwrap();
        assert_eq!(response, b"response");

        let _ = client.shutdown().await;
        let _ = server.shutdown().await;
    }
}
