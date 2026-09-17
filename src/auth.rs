use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::padding::PaddingFactory;

pub const PASSWORD_DIGEST_SIZE: usize = 32;
pub const AUTH_HEADER_SIZE: usize = PASSWORD_DIGEST_SIZE + 2;

pub fn password_digest(password: &str) -> [u8; PASSWORD_DIGEST_SIZE] {
    let mut digest = Sha256::new();
    digest.update(password.as_bytes());
    digest.finalize().into()
}

pub async fn write_auth<W: AsyncWrite + Unpin>(writer: &mut W, password: &str, padding: &PaddingFactory) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind::InvalidInput};
    let padding_len = padding.generate_record_payload_sizes(0).first().copied().unwrap_or_default();
    let padding_len = u16::try_from(padding_len).map_err(|_| Error::new(InvalidInput, "padding0 exceeds u16"))?;

    writer.write_all(&password_digest(password)).await?;
    writer.write_all(&padding_len.to_be_bytes()).await?;
    if padding_len != 0 {
        writer.write_all(&vec![0; padding_len as usize]).await?;
    }
    writer.flush().await
}

pub async fn read_auth<R: AsyncRead + Unpin>(reader: &mut R, password: &str) -> std::io::Result<()> {
    let mut header = [0u8; AUTH_HEADER_SIZE];
    reader.read_exact(&mut header).await?;
    if header[..PASSWORD_DIGEST_SIZE] != password_digest(password) {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "invalid password"));
    }
    let padding_len = u16::from_be_bytes([header[32], header[33]]) as usize;
    let mut padding = vec![0u8; padding_len];
    reader.read_exact(&mut padding).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::padding::DEFAULT_SCHEME;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn writes_and_reads_go_compatible_authentication() {
        let padding = PaddingFactory::new(DEFAULT_SCHEME).unwrap();
        let mut bytes = Vec::new();
        write_auth(&mut bytes, "password", &padding).await.unwrap();
        assert_eq!(&bytes[..32], &password_digest("password"));
        assert_eq!(&bytes[32..34], &[0, 30]);
        read_auth(&mut BufReader::new(bytes.as_slice()), "password").await.unwrap();
    }

    #[tokio::test]
    async fn rejects_wrong_password() {
        let padding = PaddingFactory::new(DEFAULT_SCHEME).unwrap();
        let mut bytes = Vec::new();
        write_auth(&mut bytes, "password", &padding).await.unwrap();
        let error = read_auth(&mut BufReader::new(bytes.as_slice()), "wrong").await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }
}
