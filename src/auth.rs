use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::padding::PaddingFactory;
use uuid::Uuid;

pub const PASSWORD_DIGEST_SIZE: usize = 32;
pub const AUTH_HEADER_SIZE: usize = PASSWORD_DIGEST_SIZE + 2;

pub fn password_digest(password: &str) -> [u8; PASSWORD_DIGEST_SIZE] {
    let mut digest = Sha256::new();
    digest.update(password.as_bytes());
    digest.finalize().into()
}

pub async fn write_auth<W: AsyncWrite + Unpin>(writer: &mut W, password: &str, padding: &PaddingFactory) -> std::io::Result<()> {
    write_auth_with_client_id(writer, password, padding, None).await
}

pub async fn write_auth_with_client_id<W: AsyncWrite + Unpin>(
    writer: &mut W,
    password: &str,
    padding: &PaddingFactory,
    client_id: Option<Uuid>,
) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind::InvalidInput};
    let padding_len = padding
        .generate_record_payload_sizes(0)
        .first()
        .copied()
        .unwrap_or_default()
        .max(client_id.map_or(0, |_| 36));
    let padding_len = u16::try_from(padding_len).map_err(|_| Error::new(InvalidInput, "padding0 exceeds u16"))?;

    writer.write_all(&password_digest(password)).await?;
    writer.write_all(&padding_len.to_be_bytes()).await?;
    if padding_len != 0 {
        let mut padding_data = vec![0; padding_len as usize];
        if let Some(client_id) = client_id {
            padding_data[..36].copy_from_slice(client_id.to_string().as_bytes());
        }
        writer.write_all(&padding_data).await?;
    }
    writer.flush().await
}

pub async fn read_auth<R: AsyncRead + Unpin>(reader: &mut R, password: &str) -> std::io::Result<()> {
    read_auth_with_client_id(reader, password).await.map(|_| ())
}

pub async fn read_auth_with_client_id<R: AsyncRead + Unpin>(reader: &mut R, password: &str) -> std::io::Result<Option<Uuid>> {
    let mut header = [0u8; AUTH_HEADER_SIZE];
    reader.read_exact(&mut header).await?;
    if header[..PASSWORD_DIGEST_SIZE] != password_digest(password) {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "invalid password"));
    }
    let padding_len = u16::from_be_bytes([header[32], header[33]]) as usize;
    let mut padding = vec![0u8; padding_len];
    reader.read_exact(&mut padding).await?;
    Ok(extract_client_id_from_padding(&padding))
}

fn extract_client_id_from_padding(padding: &[u8]) -> Option<Uuid> {
    let candidate = std::str::from_utf8(padding.get(..36)?).ok()?.trim_end_matches('\0').trim();
    Uuid::parse_str(candidate).ok()
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

    #[tokio::test]
    async fn carries_client_id_in_auth_padding() {
        let padding = PaddingFactory::new(DEFAULT_SCHEME).unwrap();
        let client_id = Uuid::parse_str("f2d46ca2-8d6d-4c5c-ae77-80c902ce68d7").unwrap();
        let mut bytes = Vec::new();
        write_auth_with_client_id(&mut bytes, "password", &padding, Some(client_id))
            .await
            .unwrap();

        assert_eq!(u16::from_be_bytes([bytes[32], bytes[33]]), 36);
        assert_eq!(
            read_auth_with_client_id(&mut BufReader::new(bytes.as_slice()), "password")
                .await
                .unwrap(),
            Some(client_id)
        );
    }
}
