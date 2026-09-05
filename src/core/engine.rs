use crate::core::action::ProtocolAction;
use crate::core::padding::PaddingFactory;
use crate::core::state::State;
use crate::core::string_map::{StringMap, StringMapExt};
use crate::core::{Command, Frame};
use bytes::Bytes;
use std::sync::Arc;

pub struct Engine;

impl Engine {
    pub fn on_session_start(state: &Arc<State>, is_client: bool, client_name: &str) -> std::io::Result<Vec<ProtocolAction>> {
        log::debug!(
            "Engine::on_session_start is_client={} client_name={} peer_version={}",
            is_client,
            client_name,
            state.peer_version()
        );

        if !is_client {
            log::trace!("Engine::on_session_start: server side, nothing to send");
            return Ok(Vec::new());
        }

        let mut settings = StringMap::new();
        settings.insert("v".to_string(), crate::PROTOCOL_VERSION.to_string());
        settings.insert("client".to_string(), client_name.to_string());
        settings.insert("padding-md5".to_string(), state.padding().md5().to_string());

        Ok(vec![ProtocolAction::SendFrame(Frame::with_data(
            Command::Settings,
            0,
            settings.to_bytes().into(),
        ))])
    }

    pub fn on_frame(state: &Arc<State>, is_client: bool, frame: &Frame) -> std::io::Result<Vec<ProtocolAction>> {
        let mut actions = Vec::new();

        log::debug!("Engine::on_frame is_client={} {}", is_client, frame);

        match frame.cmd {
            Command::Waste | Command::HeartResponse => {}
            Command::Psh if !frame.data.is_empty() => {
                actions.push(ProtocolAction::PushStreamData {
                    sid: frame.sid,
                    data: frame.data.clone(),
                });
            }
            Command::Syn if !is_client => {
                log::debug!(
                    "Server received SYN sid={} settings={} peer_version={}",
                    frame.sid,
                    state.received_settings_from_client(),
                    state.peer_version()
                );
                if !state.received_settings_from_client() {
                    actions.push(ProtocolAction::AlertAndFail {
                        message: "client did not send its settings".to_string(),
                    });
                } else {
                    actions.push(ProtocolAction::EnsureIncomingStream { sid: frame.sid });
                }
            }
            Command::Fin => {
                actions.push(ProtocolAction::CloseLocalStream { sid: frame.sid });
            }
            Command::Settings if !is_client && !frame.data.is_empty() => {
                let settings = StringMap::from_bytes(frame.data.as_ref());
                state.mark_received_settings_from_client();

                let padding = state.padding();
                if settings.get("padding-md5").map(String::as_str) != Some(padding.md5()) {
                    log::info!(
                        "Peer padding-md5 mismatch: peer={} local={}",
                        settings.get("padding-md5").unwrap_or(&"<none>".to_string()),
                        padding.md5()
                    );
                    actions.push(ProtocolAction::SendFrameSync(Frame::with_data(
                        Command::UpdatePaddingScheme,
                        0,
                        Bytes::copy_from_slice(padding.raw_scheme()),
                    )));
                }

                if let Some(version) = settings.get("v").and_then(|value| value.parse::<u8>().ok())
                    && version >= crate::MIN_PROTOCOL_VERSION
                {
                    // Accept peer versions >= MIN_PROTOCOL_VERSION for
                    // backwards compatibility. Record the peer's declared
                    // version and echo it back in ServerSettings so both
                    // sides agree on the negotiated version.
                    state.set_peer_version(version);
                    let mut server_settings = StringMap::new();
                    server_settings.insert("v".to_string(), version.to_string());
                    actions.push(ProtocolAction::SendFrameSync(Frame::with_data(
                        Command::ServerSettings,
                        0,
                        server_settings.to_bytes().into(),
                    )));
                }
            }
            Command::UpdatePaddingScheme if !frame.data.is_empty() && is_client => {
                if let Some(factory) = PaddingFactory::new(frame.data.as_ref()) {
                    state.set_padding(factory);
                }
            }
            Command::HeartRequest => {
                actions.push(ProtocolAction::SendFrame(Frame::new(Command::HeartResponse, frame.sid)));
            }
            Command::ServerSettings if !frame.data.is_empty() && is_client => {
                let settings = StringMap::from_bytes(frame.data.as_ref());
                if let Some(version) = settings.get("v").and_then(|value| value.parse::<u8>().ok()) {
                    state.set_peer_version(version);
                }
            }
            Command::SynAck => {
                // SynAck received: nothing to cancel now that synack timeouts
                // are no longer tracked via ProtocolAction. If SynAck carries
                // data, close the remote stream with the message below.
                if !frame.data.is_empty() {
                    actions.push(ProtocolAction::CloseRemoteStream {
                        sid: frame.sid,
                        message: String::from_utf8_lossy(frame.data.as_ref()).to_string(),
                    });
                }
            }
            _ => log::warn!("Received unexpected frame: {}", frame),
        }

        Ok(actions)
    }

    // `on_open_stream` removed — stream opening is handled locally by Session.
}

#[cfg(test)]
mod tests {
    use super::Engine;
    use crate::core::{Command, Frame, PaddingFactory, State};

    #[test]
    fn server_accepts_syn_after_settings() {
        let state = State::new(PaddingFactory::default());
        state.mark_received_settings_from_client();

        let actions = Engine::on_frame(&state, false, &Frame::new(Command::Syn, 7)).expect("server SYN should be accepted");

        assert!(matches!(
            actions.first(),
            Some(crate::core::ProtocolAction::EnsureIncomingStream { sid: 7 })
        ));
    }

    #[test]
    fn server_rejects_syn_before_settings() {
        let state = State::new(PaddingFactory::default());

        let actions = Engine::on_frame(&state, false, &Frame::new(Command::Syn, 7)).expect("server SYN should be accepted");

        assert!(matches!(actions.first(), Some(crate::core::ProtocolAction::AlertAndFail { .. })));
    }
}
