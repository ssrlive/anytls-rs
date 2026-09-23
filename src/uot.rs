use bytes::{BufMut, BytesMut};
use socks5_impl::protocol::{Address, AsyncStreamOperation, StreamOperation};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const V2_MAGIC_ADDRESS: &str = "sp.v2.udp-over-tcp.arpa";

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UotMode {
    Datagram = 0,
    Connected = 1,
}

impl TryFrom<u8> for UotMode {
    type Error = std::io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Datagram),
            1 => Ok(Self::Connected),
            _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid UOT mode")),
        }
    }
}

impl From<UotMode> for u8 {
    fn from(value: UotMode) -> Self {
        value as u8
    }
}

#[derive(Clone, Debug)]
pub struct UotRequest {
    pub mode: UotMode,
    pub destination: Address,
}

impl UotRequest {
    pub fn new(mode: UotMode, destination: Address) -> Self {
        Self { mode, destination }
    }
}

impl From<UotRequest> for Vec<u8> {
    fn from(request: UotRequest) -> Self {
        let mut bytes = BytesMut::with_capacity(1 + request.destination.len());
        bytes.put_u8(request.mode.into());
        request.destination.write_to_buf(&mut bytes);
        bytes.to_vec()
    }
}

pub fn uot_sentinel_destination() -> Address {
    Address::DomainAddress(V2_MAGIC_ADDRESS.into(), 0)
}

pub fn uot_is_sentinel_destination(address: &Address) -> bool {
    matches!(address, Address::DomainAddress(domain, _) if &**domain == V2_MAGIC_ADDRESS)
}

pub async fn uot_get_request_from_stream<R>(reader: &mut R) -> std::io::Result<UotRequest>
where
    R: AsyncRead + Unpin + Send + ?Sized,
{
    let mode = UotMode::try_from(reader.read_u8().await?)?;
    let destination = Address::retrieve_from_async_stream(reader).await?;
    Ok(UotRequest::new(mode, destination))
}

pub fn uot_encode_packet(mode: UotMode, destination: Option<&Address>, payload: &[u8]) -> std::io::Result<Vec<u8>> {
    if payload.len() > u16::MAX as usize {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "UOT packet too large"));
    }

    let mut bytes = BytesMut::new();
    if mode == UotMode::Datagram {
        destination
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "datagram destination required"))?
            .write_to_buf(&mut bytes);
    } else if destination.is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "connected packet cannot include a destination",
        ));
    }
    bytes.put_u16(payload.len() as u16);
    bytes.extend_from_slice(payload);
    Ok(bytes.to_vec())
}

pub async fn uot_get_packet_from_stream<R>(mode: UotMode, reader: &mut R) -> std::io::Result<(Option<Address>, Vec<u8>)>
where
    R: AsyncRead + Unpin + Send + ?Sized,
{
    let destination = if mode == UotMode::Datagram {
        Some(Address::retrieve_from_async_stream(reader).await?)
    } else {
        None
    };
    let payload_len = reader.read_u16().await? as usize;
    let mut payload = vec![0; payload_len];
    reader.read_exact(&mut payload).await?;
    Ok((destination, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn datagram_packet_round_trip_preserves_address_and_payload() {
        let destination = Address::DomainAddress("example.com".into(), 443);
        let payload = b"uot-payload";
        let frame = uot_encode_packet(UotMode::Datagram, Some(&destination), payload).unwrap();
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        writer.write_all(&frame).await.unwrap();
        let (decoded_destination, decoded_payload) = uot_get_packet_from_stream(UotMode::Datagram, &mut reader).await.unwrap();

        assert_eq!(decoded_destination.unwrap().to_string(), destination.to_string());
        assert_eq!(decoded_payload, payload);
    }

    #[tokio::test]
    async fn connected_request_and_packet_round_trip() {
        let request = UotRequest::new(UotMode::Connected, Address::DomainAddress("dns.example".into(), 53));
        let request_bytes: Vec<u8> = request.clone().into();
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        writer.write_all(&request_bytes).await.unwrap();
        let decoded_request = uot_get_request_from_stream(&mut reader).await.unwrap();
        assert_eq!(decoded_request.mode, UotMode::Connected);
        assert_eq!(decoded_request.destination.to_string(), request.destination.to_string());

        let payload = b"connected-uot-payload";
        let frame = uot_encode_packet(UotMode::Connected, None, payload).unwrap();
        writer.write_all(&frame).await.unwrap();
        let (destination, decoded_payload) = uot_get_packet_from_stream(UotMode::Connected, &mut reader).await.unwrap();
        assert!(destination.is_none());
        assert_eq!(decoded_payload, payload);
    }

    #[test]
    fn sentinel_matches_only_uot_magic_destination() {
        assert!(uot_is_sentinel_destination(&uot_sentinel_destination()));
        assert!(!uot_is_sentinel_destination(&Address::DomainAddress("example.com".into(), 443)));
    }
}
