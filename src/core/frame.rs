use bytes::{Buf, BufMut, Bytes, BytesMut};

pub const HEADER_OVERHEAD_SIZE: usize = 1 + 4 + 2;
pub const MAX_FRAME_DATA_SIZE: usize = u16::MAX as usize;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Waste,
    Syn,
    Psh,
    Fin,
    Settings,
    Alert,
    UpdatePaddingScheme,
    SynAck,
    HeartRequest,
    HeartResponse,
    ServerSettings,
    Unknown(u8),
}

impl From<u8> for Command {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Waste,
            1 => Self::Syn,
            2 => Self::Psh,
            3 => Self::Fin,
            4 => Self::Settings,
            5 => Self::Alert,
            6 => Self::UpdatePaddingScheme,
            7 => Self::SynAck,
            8 => Self::HeartRequest,
            9 => Self::HeartResponse,
            10 => Self::ServerSettings,
            other => Self::Unknown(other),
        }
    }
}

impl From<Command> for u8 {
    fn from(cmd: Command) -> Self {
        match cmd {
            Command::Waste => 0,
            Command::Syn => 1,
            Command::Psh => 2,
            Command::Fin => 3,
            Command::Settings => 4,
            Command::Alert => 5,
            Command::UpdatePaddingScheme => 6,
            Command::SynAck => 7,
            Command::HeartRequest => 8,
            Command::HeartResponse => 9,
            Command::ServerSettings => 10,
            Command::Unknown(value) => value,
        }
    }
}

impl Command {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Waste => "CMD_WASTE",
            Self::Syn => "CMD_SYN",
            Self::Psh => "CMD_PSH",
            Self::Fin => "CMD_FIN",
            Self::Settings => "CMD_SETTINGS",
            Self::Alert => "CMD_ALERT",
            Self::UpdatePaddingScheme => "CMD_UPDATE_PADDING_SCHEME",
            Self::SynAck => "CMD_SYNACK",
            Self::HeartRequest => "CMD_HEART_REQUEST",
            Self::HeartResponse => "CMD_HEART_RESPONSE",
            Self::ServerSettings => "CMD_SERVER_SETTINGS",
            Self::Unknown(_) => "CMD_UNKNOWN",
        }
    }
}

impl std::fmt::Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}({})", self.name(), u8::from(*self))
    }
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub cmd: Command,
    // `sid` indicates the logical stream identifier.
    // Historical protocol design:
    //  - sid == 0 : session-level control frames (settings, alerts, padding updates, etc.)
    //  - sid >= 1  : per-logical-stream data frames (1 was the initial/default stream id)
    pub sid: u32,
    pub data: Bytes,
}

impl std::fmt::Display for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame {{ cmd: {}, sid: {}, data_len: {} }}", self.cmd, self.sid, self.data.len())
    }
}

impl Frame {
    pub fn new(cmd: Command, sid: u32) -> Self {
        Self {
            cmd,
            sid,
            data: Bytes::new(),
        }
    }

    pub fn with_data(cmd: Command, sid: u32, data: Bytes) -> Self {
        Self { cmd, sid, data }
    }

    pub fn to_bytes(&self) -> std::io::Result<Bytes> {
        if self.data.len() > MAX_FRAME_DATA_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "frame payload exceeds protocol limit",
            ));
        }
        let mut buf = BytesMut::with_capacity(HEADER_OVERHEAD_SIZE + self.data.len());
        buf.put_u8(u8::from(self.cmd));
        buf.put_u32(self.sid);
        buf.put_u16(self.data.len() as u16);
        buf.put_slice(&self.data);
        Ok(buf.freeze())
    }

    pub fn from_bytes(mut data: &[u8]) -> Option<Self> {
        let header = RawHeader::from_bytes(data)?;
        let length = header.length as usize;
        data.advance(HEADER_OVERHEAD_SIZE);

        if data.len() < length {
            return None;
        }

        let frame_data = data[..length].to_vec();

        Some(Self {
            cmd: header.cmd,
            sid: header.sid,
            data: frame_data.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Frame, MAX_FRAME_DATA_SIZE};
    use bytes::Bytes;

    #[test]
    fn rejects_oversized_payload_during_serialization() {
        let frame = Frame::with_data(Command::Psh, 1, Bytes::from(vec![0; MAX_FRAME_DATA_SIZE + 1]));
        assert_eq!(frame.to_bytes().unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    }
}

#[derive(Debug, Clone)]
pub struct RawHeader {
    pub cmd: Command,
    pub sid: u32,
    pub length: u16,
}

impl RawHeader {
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < HEADER_OVERHEAD_SIZE {
            return None;
        }

        let mut buf = data;
        let cmd = Command::from(buf.get_u8());
        let sid = buf.get_u32();
        let length = buf.get_u16();

        Some(Self { cmd, sid, length })
    }
}
