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
            "Engine::on_session_start {} side, client_name={} peer_version={}",
            if is_client { "client" } else { "server" },
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
        use std::io::{Error, ErrorKind::InvalidData};
        let mut actions = Vec::new();

        log::debug!("Engine::on_frame is_client={} {}", is_client, frame);

        match frame.cmd {
            Command::Psh | Command::Syn | Command::Fin | Command::SynAck if frame.sid == 0 => {
                return Err(Error::new(InvalidData, format!("{} cannot use control sid 0", frame.cmd)));
            }
            Command::Waste if frame.sid != 0 => {
                return Err(Error::new(InvalidData, format!("{} must use control sid 0", frame.cmd)));
            }
            Command::Settings
            | Command::Alert
            | Command::UpdatePaddingScheme
            | Command::HeartRequest
            | Command::HeartResponse
            | Command::ServerSettings
                if frame.sid != 0 =>
            {
                return Err(Error::new(InvalidData, format!("{} must use control sid 0", frame.cmd)));
            }
            _ => {}
        }

        let payload_forbidden = matches!(
            frame.cmd,
            Command::Syn | Command::Fin | Command::HeartRequest | Command::HeartResponse
        );
        if payload_forbidden && !frame.data.is_empty() {
            return Err(Error::new(InvalidData, format!("{} cannot carry a payload", frame.cmd)));
        }
        if matches!(
            frame.cmd,
            Command::Settings | Command::ServerSettings | Command::UpdatePaddingScheme
        ) && frame.data.is_empty()
        {
            return Err(Error::new(InvalidData, format!("{} requires a payload", frame.cmd)));
        }

        // client side handling of incoming frames
        if is_client {
            match frame.cmd {
                Command::Waste | Command::HeartResponse => {}
                Command::Psh => {
                    if !frame.data.is_empty() {
                        actions.push(ProtocolAction::PushStreamData {
                            sid: frame.sid,
                            data: frame.data.clone(),
                        });
                    }
                }
                Command::Fin => {
                    actions.push(ProtocolAction::CloseLocalStream { sid: frame.sid });
                }
                Command::UpdatePaddingScheme => {
                    if let Some(factory) = PaddingFactory::new(frame.data.as_ref()) {
                        state.set_padding(factory);
                    }
                }
                Command::ServerSettings => {
                    let settings = StringMap::from_bytes(frame.data.as_ref());
                    if let Some(version) = settings.get("v").and_then(|value| value.parse::<u8>().ok()) {
                        state.set_peer_version(version);
                    }
                }
                Command::HeartRequest => {
                    actions.push(ProtocolAction::SendFrame(Frame::new(Command::HeartResponse, frame.sid)));
                }
                Command::SynAck => {
                    // A SYNACK without data means success. A payload reports a
                    // stream-scoped handshake failure to the client.
                    if !frame.data.is_empty() {
                        actions.push(ProtocolAction::CloseRemoteStream {
                            sid: frame.sid,
                            message: String::from_utf8_lossy(frame.data.as_ref()).to_string(),
                        });
                    }
                }
                Command::Alert => {
                    // Alerts are handled by the session layer so their message
                    // can be logged before the session is terminated.
                }
                Command::Syn | Command::Settings => {
                    return Err(Error::new(InvalidData, format!("{} is not valid from the server", frame.cmd)));
                }
                Command::Unknown(_) => log::warn!("Received unexpected frame: {}", frame),
            }
            return Ok(actions);
        }

        // server side handling of incoming frames
        match frame.cmd {
            Command::Waste | Command::HeartResponse => {}
            Command::Psh => {
                if !frame.data.is_empty() {
                    actions.push(ProtocolAction::PushStreamData {
                        sid: frame.sid,
                        data: frame.data.clone(),
                    });
                }
            }
            Command::Syn => {
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
            Command::Settings => {
                if state.received_settings_from_client() {
                    actions.push(ProtocolAction::AlertAndFail {
                        message: "duplicate client settings".to_string(),
                    });
                    return Ok(actions);
                }
                let settings = StringMap::from_bytes(frame.data.as_ref());
                let Some(version) = settings.get("v").and_then(|value| value.parse::<u8>().ok()) else {
                    actions.push(ProtocolAction::AlertAndFail {
                        message: "client settings missing a valid protocol version".to_string(),
                    });
                    return Ok(actions);
                };
                if version < crate::MIN_PROTOCOL_VERSION {
                    actions.push(ProtocolAction::AlertAndFail {
                        message: format!("unsupported protocol version: {version}"),
                    });
                    return Ok(actions);
                }
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
            Command::HeartRequest => {
                actions.push(ProtocolAction::SendFrame(Frame::new(Command::HeartResponse, frame.sid)));
            }
            Command::Alert | Command::UpdatePaddingScheme | Command::ServerSettings | Command::SynAck => {
                return Err(Error::new(InvalidData, format!("{} is not valid from the client", frame.cmd)));
            }
            Command::Unknown(_) => log::warn!("Received unexpected frame: {}", frame),
        }

        Ok(actions)
    }
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

    #[test]
    fn rejects_payload_on_control_sid() {
        let state = State::new(PaddingFactory::default());
        let error = Engine::on_frame(&state, false, &Frame::new(Command::Psh, 0)).expect_err("control SID must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_waste_on_stream_sid() {
        let state = State::new(PaddingFactory::default());
        let error = Engine::on_frame(&state, false, &Frame::new(Command::Waste, 1)).expect_err("WASTE must use control SID");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_heartbeat_on_stream_sid() {
        let state = State::new(PaddingFactory::default());
        let error = Engine::on_frame(&state, false, &Frame::new(Command::HeartRequest, 1)).expect_err("heartbeat must use control SID");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_payload_on_fin() {
        let state = State::new(PaddingFactory::default());
        let frame = Frame::with_data(Command::Fin, 1, bytes::Bytes::from_static(b"unexpected"));
        let error = Engine::on_frame(&state, false, &frame).expect_err("FIN must not carry a payload");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_settings_without_protocol_version() {
        let state = State::new(PaddingFactory::default());
        let frame = Frame::with_data(Command::Settings, 0, bytes::Bytes::from_static(b"client=test"));
        let actions = Engine::on_frame(&state, false, &frame).expect("invalid settings should produce an alert action");

        assert!(matches!(actions.first(), Some(crate::core::ProtocolAction::AlertAndFail { .. })));
        assert!(!state.received_settings_from_client());
    }

    #[test]
    fn rejects_duplicate_settings() {
        let state = State::new(PaddingFactory::default());
        state.mark_received_settings_from_client();
        let frame = Frame::with_data(Command::Settings, 0, bytes::Bytes::from_static(b"v=2"));
        let actions = Engine::on_frame(&state, false, &frame).expect("duplicate settings should produce an alert action");

        assert!(matches!(actions.first(), Some(crate::core::ProtocolAction::AlertAndFail { .. })));
    }

    #[test]
    fn rejects_commands_from_wrong_direction() {
        let state = State::new(PaddingFactory::default());
        for frame in [
            Frame::new(Command::Syn, 1),
            Frame::with_data(Command::Settings, 0, bytes::Bytes::from_static(b"v=2")),
        ] {
            let error = Engine::on_frame(&state, true, &frame).expect_err("client must reject server-only commands");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        }

        for frame in [
            Frame::new(Command::SynAck, 1),
            Frame::with_data(Command::ServerSettings, 0, bytes::Bytes::from_static(b"v=2")),
            Frame::with_data(Command::UpdatePaddingScheme, 0, bytes::Bytes::from_static(b"stop=1")),
            Frame::with_data(Command::Alert, 0, bytes::Bytes::from_static(b"unexpected")),
        ] {
            let error = Engine::on_frame(&state, false, &frame).expect_err("server must reject client-only commands");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        }
    }
}
