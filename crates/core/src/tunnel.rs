//! Runtime tunnel protocol and transport boundaries.
//!
//! The tunnel layer sits between browser/web routing and [`RuntimeSession`].
//! It defines stable semantic frames without making the PTY actor depend on a
//! concrete transport such as WebSocket, TCP, or gRPC.
//!
//! [`RuntimeSession`]: crate::runtime::RuntimeSession

use std::fmt::{self, Debug, Formatter};

use bytes::Bytes;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::{
    protocol::{HeartbeatSequence, SafeMessage, ServerControlMessage, SessionName, TerminalSize},
    runtime::{ClientId, ClientOutput, RuntimeCommand, ShutdownReason},
};

const TUNNEL_TERMINAL_PAYLOAD_MAX_BYTES: usize = 64 * 1024;

/// Tunnel protocol failure.
#[derive(Debug, Error)]
pub enum TunnelError {
    /// A terminal payload exceeded the configured frame cap.
    #[error("tunnel terminal payload exceeds 65536 bytes")]
    TerminalPayloadTooLarge,
    /// JSON tunnel frame encoding or decoding failed.
    #[error("failed to encode or decode tunnel JSON frame")]
    Json(#[source] serde_json::Error),
    /// The payload kind is not supported by this codec.
    #[error("unsupported tunnel payload for codec")]
    UnsupportedPayload,
}

/// Bounded terminal bytes carried by tunnel frames.
#[derive(Clone, PartialEq, Eq)]
pub struct TunnelTerminalPayload(Bytes);

impl TunnelTerminalPayload {
    /// Creates a bounded terminal payload.
    ///
    /// # Errors
    ///
    /// Returns [`TunnelError::TerminalPayloadTooLarge`] when the payload exceeds
    /// 64 KiB.
    pub fn new(bytes: Bytes) -> Result<Self, TunnelError> {
        if bytes.len() <= TUNNEL_TERMINAL_PAYLOAD_MAX_BYTES {
            Ok(Self(bytes))
        } else {
            Err(TunnelError::TerminalPayloadTooLarge)
        }
    }

    /// Returns the payload length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether the payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the payload bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_ref()
    }

    /// Consumes the payload and returns the underlying bytes.
    #[must_use]
    pub fn into_bytes(self) -> Bytes {
        self.0
    }
}

impl Debug for TunnelTerminalPayload {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TunnelTerminalPayload")
            .field("len", &self.0.len())
            .finish()
    }
}

impl Serialize for TunnelTerminalPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for TunnelTerminalPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        Self::new(Bytes::from(bytes)).map_err(serde::de::Error::custom)
    }
}

/// Transport-level payload passed to codecs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelPayload {
    /// Text transport payload, usually JSON control data.
    Text(String),
    /// Binary transport payload, usually raw terminal bytes with an envelope.
    Binary(Bytes),
}

/// Stable close reason carried through the runtime tunnel.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "reason", content = "detail", rename_all = "camelCase")]
pub enum TunnelCloseReason {
    /// Server supervisor requested shutdown.
    Supervisor,
    /// The browser/client disconnected.
    ClientDisconnect,
    /// A newer browser/client took over the controller role.
    ControllerReplaced,
    /// The child process exited or the PTY reached EOF.
    ChildExit,
    /// Runtime error.
    RuntimeError(SafeMessage),
}

impl TunnelCloseReason {
    fn from_shutdown_reason(reason: ShutdownReason) -> Self {
        match reason {
            ShutdownReason::Supervisor => Self::Supervisor,
            ShutdownReason::ClientDisconnect => Self::ClientDisconnect,
            ShutdownReason::ControllerReplaced => Self::ControllerReplaced,
            ShutdownReason::ChildExit => Self::ChildExit,
            ShutdownReason::RuntimeError(message) => {
                let message = SafeMessage::new(message)
                    .unwrap_or_else(|_error| SafeMessage::from_static("runtime error"));
                Self::RuntimeError(message)
            }
        }
    }
}

/// Runtime-side control frame.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum TunnelRuntimeControl {
    /// Server-to-browser runtime control message.
    Server {
        /// Runtime control message.
        message: ServerControlMessage,
    },
    /// Runtime closed the client stream.
    Closed {
        /// Stable close reason.
        reason: TunnelCloseReason,
    },
}

/// Stable semantic frame exchanged between web side and runtime side.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum TunnelFrame {
    /// Runtime side announces a session.
    RegisterSession {
        /// Runtime session id.
        session: SessionName,
        /// Command or shell display text.
        command_display: SafeMessage,
        /// Initial runtime terminal size.
        size: TerminalSize,
    },
    /// Web side attaches a browser client to the runtime side.
    AttachBrowser {
        /// Browser client id.
        client_id: ClientId,
    },
    /// Web side detaches a browser client from the runtime side.
    DetachBrowser {
        /// Browser client id.
        client_id: ClientId,
    },
    /// Web side forwards browser terminal input bytes to the runtime side.
    BrowserInput {
        /// Browser client id.
        client_id: ClientId,
        /// Terminal bytes.
        bytes: TunnelTerminalPayload,
    },
    /// Web side forwards browser terminal dimensions to the runtime side.
    BrowserResize {
        /// Browser client id.
        client_id: ClientId,
        /// Proposed terminal size.
        size: TerminalSize,
    },
    /// Runtime side forwards PTY output bytes to the web side.
    PtyOutput {
        /// Terminal bytes.
        bytes: TunnelTerminalPayload,
    },
    /// Runtime side forwards runtime control to the web side.
    RuntimeControl {
        /// Browser client id when the control is scoped to one client.
        client_id: Option<ClientId>,
        /// Runtime control message.
        control: TunnelRuntimeControl,
    },
    /// Liveness heartbeat.
    Heartbeat {
        /// Heartbeat sequence.
        sequence: HeartbeatSequence,
    },
}

/// Codec for converting semantic tunnel frames to transport payloads.
pub trait TunnelCodec: Debug + Send + Sync {
    /// Encodes a semantic tunnel frame.
    ///
    /// # Errors
    ///
    /// Returns [`TunnelError`] when the frame cannot be encoded.
    fn encode(&self, frame: &TunnelFrame) -> Result<TunnelPayload, TunnelError>;

    /// Decodes a semantic tunnel frame.
    ///
    /// # Errors
    ///
    /// Returns [`TunnelError`] when the payload cannot be decoded.
    fn decode(&self, payload: TunnelPayload) -> Result<TunnelFrame, TunnelError>;
}

/// JSON codec for tunnel control frames and test fixtures.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonTunnelCodec;

impl TunnelCodec for JsonTunnelCodec {
    fn encode(&self, frame: &TunnelFrame) -> Result<TunnelPayload, TunnelError> {
        serde_json::to_string(frame)
            .map(TunnelPayload::Text)
            .map_err(TunnelError::Json)
    }

    fn decode(&self, payload: TunnelPayload) -> Result<TunnelFrame, TunnelError> {
        match payload {
            TunnelPayload::Text(text) => serde_json::from_str(&text).map_err(TunnelError::Json),
            TunnelPayload::Binary(_bytes) => Err(TunnelError::UnsupportedPayload),
        }
    }
}

/// Synchronous boundary for concrete tunnel transports.
///
/// Async transports can implement this boundary with internal tasks and bounded
/// channels while keeping runtime code independent from socket implementations.
pub trait TunnelTransport: Debug + Send {
    /// Sends one semantic frame through the transport.
    ///
    /// # Errors
    ///
    /// Returns [`TunnelError`] when the transport cannot send the frame.
    fn send_frame(&mut self, frame: TunnelFrame) -> Result<(), TunnelError>;

    /// Receives one semantic frame from the transport.
    ///
    /// # Errors
    ///
    /// Returns [`TunnelError`] when the transport cannot decode or receive a
    /// frame.
    fn receive_frame(&mut self) -> Result<Option<TunnelFrame>, TunnelError>;
}

/// Runtime-side action produced from a tunnel frame.
#[derive(Debug)]
pub enum RuntimeTunnelAction {
    /// Attach a browser. The caller must create the runtime output mailbox.
    AttachBrowser {
        /// Browser client id.
        client_id: ClientId,
    },
    /// Send a runtime command directly.
    Command(RuntimeCommand),
    /// No runtime command is needed.
    None,
}

/// Bridge mapping between tunnel frames and runtime commands/output.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeTunnelBridge;

impl RuntimeTunnelBridge {
    /// Converts a tunnel frame into a runtime-side action.
    #[must_use]
    pub fn action_from_frame(frame: TunnelFrame) -> RuntimeTunnelAction {
        match frame {
            TunnelFrame::AttachBrowser { client_id } => {
                RuntimeTunnelAction::AttachBrowser { client_id }
            }
            TunnelFrame::DetachBrowser { client_id } => {
                RuntimeTunnelAction::Command(RuntimeCommand::DetachClient { client_id })
            }
            TunnelFrame::BrowserInput { client_id, bytes } => {
                RuntimeTunnelAction::Command(RuntimeCommand::Input {
                    client_id,
                    bytes: bytes.into_bytes(),
                })
            }
            TunnelFrame::BrowserResize { client_id, size } => {
                RuntimeTunnelAction::Command(RuntimeCommand::BrowserResize { client_id, size })
            }
            TunnelFrame::RegisterSession { .. }
            | TunnelFrame::PtyOutput { .. }
            | TunnelFrame::RuntimeControl { .. }
            | TunnelFrame::Heartbeat { .. } => RuntimeTunnelAction::None,
        }
    }

    /// Creates an attach command once the bridge has allocated an output
    /// mailbox.
    #[must_use]
    pub fn attach_command(
        client_id: ClientId,
        output: crate::runtime::ClientOutputTx,
    ) -> RuntimeCommand {
        RuntimeCommand::AttachClient { client_id, output }
    }

    /// Converts runtime output into a tunnel frame.
    ///
    /// # Errors
    ///
    /// Returns [`TunnelError::TerminalPayloadTooLarge`] if a runtime byte chunk
    /// exceeds the tunnel frame cap.
    pub fn frame_from_output(
        client_id: Option<ClientId>,
        output: ClientOutput,
    ) -> Result<TunnelFrame, TunnelError> {
        match output {
            ClientOutput::Bytes(bytes) => Ok(TunnelFrame::PtyOutput {
                bytes: TunnelTerminalPayload::new(bytes)?,
            }),
            ClientOutput::Control(message) => Ok(TunnelFrame::RuntimeControl {
                client_id,
                control: TunnelRuntimeControl::Server { message },
            }),
            ClientOutput::Closed(reason) => Ok(TunnelFrame::RuntimeControl {
                client_id,
                control: TunnelRuntimeControl::Closed {
                    reason: TunnelCloseReason::from_shutdown_reason(reason),
                },
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{LeaseOwner, ServerControlMessage};

    #[test]
    fn test_should_round_trip_json_tunnel_frame() -> anyhow::Result<()> {
        let codec = JsonTunnelCodec;
        let frame = TunnelFrame::BrowserResize {
            client_id: ClientId::new(7),
            size: TerminalSize::new(120, 40)?,
        };

        let payload = codec.encode(&frame)?;
        let decoded = codec.decode(payload)?;

        assert_eq!(decoded, frame);
        Ok(())
    }

    #[test]
    fn test_should_reject_oversized_terminal_payload() {
        let bytes = Bytes::from(vec![b'x'; TUNNEL_TERMINAL_PAYLOAD_MAX_BYTES + 1]);

        assert!(matches!(
            TunnelTerminalPayload::new(bytes),
            Err(TunnelError::TerminalPayloadTooLarge)
        ));
    }

    #[test]
    fn test_should_map_browser_input_to_runtime_command() -> anyhow::Result<()> {
        let client_id = ClientId::new(3);
        let frame = TunnelFrame::BrowserInput {
            client_id,
            bytes: TunnelTerminalPayload::new(Bytes::from_static(b"pwd\n"))?,
        };

        let RuntimeTunnelAction::Command(RuntimeCommand::Input {
            client_id: id,
            bytes,
        }) = RuntimeTunnelBridge::action_from_frame(frame)
        else {
            panic!("expected runtime input command");
        };

        assert_eq!(id, client_id);
        assert_eq!(bytes, Bytes::from_static(b"pwd\n"));
        Ok(())
    }

    #[test]
    fn test_should_map_runtime_control_to_tunnel_frame() -> anyhow::Result<()> {
        let frame = RuntimeTunnelBridge::frame_from_output(
            Some(ClientId::new(9)),
            ClientOutput::Control(ServerControlMessage::LeaseChanged {
                owner: LeaseOwner::Browser,
                epoch: 4,
            }),
        )?;

        assert_eq!(
            frame,
            TunnelFrame::RuntimeControl {
                client_id: Some(ClientId::new(9)),
                control: TunnelRuntimeControl::Server {
                    message: ServerControlMessage::LeaseChanged {
                        owner: LeaseOwner::Browser,
                        epoch: 4,
                    },
                },
            }
        );
        Ok(())
    }

    #[test]
    fn test_should_map_runtime_close_to_safe_tunnel_reason() -> anyhow::Result<()> {
        let frame = RuntimeTunnelBridge::frame_from_output(
            None,
            ClientOutput::Closed(ShutdownReason::RuntimeError("x".repeat(600))),
        )?;

        assert_eq!(
            frame,
            TunnelFrame::RuntimeControl {
                client_id: None,
                control: TunnelRuntimeControl::Closed {
                    reason: TunnelCloseReason::RuntimeError(SafeMessage::from_static(
                        "runtime error"
                    )),
                },
            }
        );
        Ok(())
    }
}
