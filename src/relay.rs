use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Endpoint {
    Peer,
    Tunnel,
}

impl Endpoint {
    fn label(self) -> &'static str {
        match self {
            Self::Peer => "peer",
            Self::Tunnel => "AnyTLS tunnel",
        }
    }
}

#[derive(Debug)]
pub struct RelayError {
    endpoint: Endpoint,
    error: std::io::Error,
}

impl RelayError {
    pub fn is_peer_disconnect(&self) -> bool {
        self.endpoint == Endpoint::Peer && is_peer_disconnect(&self.error)
    }

    fn kind(&self) -> std::io::ErrorKind {
        self.error.kind()
    }
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} relay: {}", self.endpoint.label(), self.error)
    }
}

impl std::error::Error for RelayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl From<RelayError> for std::io::Error {
    fn from(error: RelayError) -> Self {
        let kind = error.kind();
        Self::new(kind, error)
    }
}

pub fn is_peer_disconnect(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset};
    matches!(error.kind(), ConnectionReset | ConnectionAborted | BrokenPipe)
}

pub async fn copy_bidirectional<A, B>(peer: &mut A, tunnel: &mut B) -> Result<(u64, u64), RelayError>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut peer_reader, mut peer_writer) = tokio::io::split(peer);
    let (mut tunnel_reader, mut tunnel_writer) = tokio::io::split(tunnel);
    tokio::try_join!(
        copy_direction(&mut peer_reader, &mut tunnel_writer, Endpoint::Peer, Endpoint::Tunnel),
        copy_direction(&mut tunnel_reader, &mut peer_writer, Endpoint::Tunnel, Endpoint::Peer),
    )
}

async fn copy_direction<R, W>(reader: &mut R, writer: &mut W, read_endpoint: Endpoint, write_endpoint: Endpoint) -> Result<u64, RelayError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = [0u8; 16 * 1024];
    let mut copied = 0u64;
    loop {
        let size = reader.read(&mut buffer).await.map_err(|error| RelayError {
            endpoint: read_endpoint,
            error,
        })?;
        if size == 0 {
            break;
        }
        writer.write_all(&buffer[..size]).await.map_err(|error| RelayError {
            endpoint: write_endpoint,
            error,
        })?;
        copied += size as u64;
    }
    writer.shutdown().await.map_err(|error| RelayError {
        endpoint: write_endpoint,
        error,
    })?;
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::{Endpoint, RelayError, is_peer_disconnect};
    use std::io::ErrorKind;

    #[test]
    fn classifies_peer_disconnect_errors() {
        for kind in [ErrorKind::ConnectionReset, ErrorKind::ConnectionAborted, ErrorKind::BrokenPipe] {
            assert!(is_peer_disconnect(&std::io::Error::from(kind)));
        }
        for kind in [
            ErrorKind::UnexpectedEof,
            ErrorKind::NotConnected,
            ErrorKind::TimedOut,
            ErrorKind::InvalidData,
        ] {
            assert!(!is_peer_disconnect(&std::io::Error::from(kind)));
        }
    }

    #[test]
    fn only_downgrades_disconnects_from_the_local_peer() {
        let peer_error = RelayError {
            endpoint: Endpoint::Peer,
            error: std::io::Error::from(ErrorKind::ConnectionReset),
        };
        let tunnel_error = RelayError {
            endpoint: Endpoint::Tunnel,
            error: std::io::Error::from(ErrorKind::ConnectionReset),
        };

        assert!(peer_error.is_peer_disconnect());
        assert!(!tunnel_error.is_peer_disconnect());
    }
}
