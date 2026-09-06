use crate::proxy::pipe::PipeDeadline;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify, mpsc};

enum PipeEvent {
    Data(Vec<u8>),
    StreamEnd(Option<std::io::Error>),
}

pub struct PipeReader {
    pub inner: Arc<Mutex<PipeInner>>,
}

pub struct PipeWriter {
    pub inner: Arc<Mutex<PipeInner>>,
}

pub struct PipeInner {
    read_deadline: PipeDeadline,
    write_deadline: PipeDeadline,
    closed: bool,
    stream_end_queued: bool,
    read_error: Option<std::io::Error>,
    data_sender: Option<mpsc::Sender<PipeEvent>>,
    data_receiver: Option<mpsc::Receiver<PipeEvent>>,
    buffer: Vec<u8>,
    // Notify to wake readers when receiver becomes available or pipe state changes
    read_waiter: Arc<Notify>,
}

impl PipeReader {
    pub async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            // 1) Fast path: if buffer has data, consume it immediately
            {
                let mut inner = self.inner.lock().await;
                if !inner.buffer.is_empty() {
                    let len = inner.buffer.len().min(buf.len());
                    buf[..len].copy_from_slice(&inner.buffer[..len]);
                    inner.buffer.drain(0..len);
                    return Ok(len);
                }

                // If the pipe is closed and no data, return EOF or error
                if inner.closed && inner.data_sender.is_none() {
                    if let Some(err) = inner.read_error.take() {
                        return Err(err);
                    } else {
                        return Ok(0);
                    }
                }

                // If data_receiver is not available, wait until it's available or deadline triggers
                if inner.data_receiver.is_none() {
                    let waiter = inner.read_waiter.clone();
                    let deadline = inner.read_deadline.wait_owned();
                    drop(inner);

                    tokio::select! {
                        _ = waiter.notified() => continue, // try again
                        _ = deadline.notified() => return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "read deadline reached")),
                    }
                }
            }

            // 2) Acquire receiver and await data or deadline
            // Take receiver under lock
            let mut receiver = self
                .inner
                .lock()
                .await
                .data_receiver
                .take()
                .ok_or(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Pipe reader already in use"))?;

            let deadline_notify = self.inner.lock().await.read_deadline.wait_owned();

            // key part: wait for data or deadline
            let res = tokio::select! {
                res = receiver.recv() => res,
                _ = deadline_notify.notified() => None,
            };

            // Restore receiver
            let mut inner = self.inner.lock().await;
            inner.data_receiver = Some(receiver);

            match res {
                Some(PipeEvent::Data(data)) => {
                    let len = data.len().min(buf.len());
                    buf[..len].copy_from_slice(&data[..len]);
                    if len < data.len() {
                        inner.buffer.extend_from_slice(&data[len..]);
                    }
                    return Ok(len);
                }
                Some(PipeEvent::StreamEnd(error)) => {
                    inner.closed = true;
                    inner.data_sender = None;
                    if let Some(err) = error {
                        return Err(err);
                    }
                    return Ok(0);
                }
                None => {
                    // Either sender dropped (EOF) or deadline
                    if let Some(err) = inner.read_error.take() {
                        return Err(err);
                    }

                    if inner.data_sender.is_none() {
                        return Ok(0);
                    } else {
                        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "read deadline reached"));
                    }
                }
            }
        }
    }

    pub fn close_with_error(&self, error: Option<std::io::Error>) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut inner = inner.lock().await;
            inner.read_error = error;
            inner.closed = true;
            inner.data_sender = None;
            // Wake any readers waiting on `read_waiter` so they observe closure/error.
            inner.read_waiter.notify_one();
        });
    }

    pub async fn finish_stream(&self, error: Option<std::io::Error>) {
        let (sender, waiter) = {
            let mut inner = self.inner.lock().await;
            if inner.closed || inner.stream_end_queued {
                return;
            }

            inner.stream_end_queued = true;
            (inner.data_sender.clone(), inner.read_waiter.clone())
        };

        let sent = if let Some(sender) = sender {
            sender.send(PipeEvent::StreamEnd(error)).await.is_ok()
        } else {
            false
        };

        if !sent {
            let mut inner = self.inner.lock().await;
            inner.closed = true;
            inner.data_sender = None;
        }

        waiter.notify_one();
    }

    pub async fn set_read_deadline(&self, deadline: std::time::SystemTime) -> std::io::Result<()> {
        let mut inner = self.inner.lock().await;
        inner.read_deadline.set(deadline);
        Ok(())
    }
}

impl PipeWriter {
    pub async fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        use std::io::{Error, ErrorKind::BrokenPipe};
        let (tx, waiter) = {
            let inner = self.inner.lock().await;
            if inner.closed || inner.stream_end_queued {
                return Err(Error::new(BrokenPipe, "Pipe closed"));
            }
            let tx = inner.data_sender.clone().ok_or_else(|| Error::new(BrokenPipe, "Pipe closed"))?;
            (tx, inner.read_waiter.clone())
        };

        tx.send(PipeEvent::Data(buf.to_vec()))
            .await
            .map_err(|error| Error::new(BrokenPipe, format!("Channel closed: {}", error)))?;
        waiter.notify_one();
        Ok(buf.len())
    }

    pub async fn set_write_deadline(&self, deadline: std::time::SystemTime) -> std::io::Result<()> {
        let mut inner = self.inner.lock().await;
        inner.write_deadline.set(deadline);
        Ok(())
    }
}

pub fn pipe() -> (PipeReader, PipeWriter) {
    const PIPE_QUEUE_CAPACITY: usize = 64;
    let (tx, rx) = mpsc::channel(PIPE_QUEUE_CAPACITY);

    let inner = Arc::new(Mutex::new(PipeInner {
        read_deadline: PipeDeadline::new(),
        write_deadline: PipeDeadline::new(),
        closed: false,
        stream_end_queued: false,
        read_error: None,
        data_sender: Some(tx),
        data_receiver: Some(rx),
        buffer: Vec::new(),
        read_waiter: Arc::new(Notify::new()),
    }));

    (PipeReader { inner: inner.clone() }, PipeWriter { inner })
}

#[cfg(test)]
mod tests {
    use super::pipe;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn finish_stream_drains_queued_data_before_eof() {
        let (reader, writer) = pipe();

        writer.write(b"first").await.unwrap();
        writer.write(b"second").await.unwrap();
        reader.finish_stream(None).await;

        assert_eq!(writer.write(b"after-fin").await.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);

        let mut buffer = [0_u8; 16];

        let first = reader.read(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..first], b"first");

        let second = reader.read(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..second], b"second");

        assert_eq!(reader.read(&mut buffer).await.unwrap(), 0);
        assert_eq!(reader.read(&mut buffer).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn finish_stream_returns_error_after_queued_data() {
        let (reader, writer) = pipe();

        writer.write(b"payload").await.unwrap();
        reader.finish_stream(Some(std::io::Error::other("peer failed"))).await;

        let mut buffer = [0_u8; 16];
        let len = reader.read(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..len], b"payload");

        let error = reader.read(&mut buffer).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(reader.read(&mut buffer).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn writer_applies_backpressure_when_reader_is_slow() {
        let (reader, writer) = pipe();
        for _ in 0..64 {
            writer.write(b"queued").await.expect("queue should accept its capacity");
        }

        let blocked_write = tokio::spawn(async move { writer.write(b"blocked").await });
        assert!(timeout(Duration::from_millis(20), blocked_write).await.is_err());

        let mut buffer = [0_u8; 16];
        assert_eq!(reader.read(&mut buffer).await.expect("reader should drain one item"), 6);
    }
}
