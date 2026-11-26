// SPDX-License-Identifier: AGPL-3.0-only

//! Compile-time spike for the official `libsignal-net` chat transport.
//!
//! This module only builds a `ChatConnection`; it is not wired into
//! `SignalWebSocket` yet.

#![cfg(feature = "cdsi")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use prost::Message as _;
use libsignal_net::chat::{
    ChatConnection, ChatHeaders, RECOMMENDED_CHAT_WS_CONFIG,
};
use libsignal_net::chat::ConnectError;
use libsignal_net::connect_state::{
    ConnectionResources, ServiceName, WebSocketTransportConnectorFactory,
};
use libsignal_net::env::UserAgent;
use libsignal_net_infra::route::{
    RouteProvider, TransportRoute, UnresolvedHttpsServiceRoute, UsePreconnect,
};

/// Tighter keepalive and idle-disconnect thresholds for the official chat WS.
///
/// Upstream `RECOMMENDED_CHAT_WS_CONFIG` uses `local_idle_timeout = 31s` and
/// `remote_idle_timeout = 45s`, which never sends an RFC6455 Ping between 30s
/// watchdog probes and waits `15s + RTT` before closing a half-open socket.
/// `20s / 30s` probes idle links before the 30s probe cadence while keeping a
/// 10s Pong window.
pub const OFFICIAL_CHAT_WS_CONFIG: libsignal_net::chat::ws::Config =
    libsignal_net::chat::ws::Config {
        local_idle_timeout: Duration::from_secs(20),
        post_request_interface_check_timeout: RECOMMENDED_CHAT_WS_CONFIG
            .post_request_interface_check_timeout,
        remote_idle_timeout: Duration::from_secs(30),
        initial_request_id: RECOMMENDED_CHAT_WS_CONFIG.initial_request_id,
    };

/// Starts an authenticated official chat connection and returns it with a
/// no-op listener. The caller is expected to supply the real listener.
// The arguments mirror the official constructor one-to-one.
#[allow(clippy::too_many_arguments)]
pub async fn connect<TC, R>(
    resources: ConnectionResources<'_, TC>,
    service: ServiceName,
    route: R,
    user_agent: &UserAgent,
    headers: ChatHeaders,
) -> Result<ChatConnection, ConnectError>
where
    TC: WebSocketTransportConnectorFactory<UsePreconnect<TransportRoute>>,
    R: RouteProvider<Route = UnresolvedHttpsServiceRoute>,
{
    let pending = ChatConnection::start_connect_with(
        resources,
        service,
        route,
        "/v1/websocket/",
        user_agent,
        OFFICIAL_CHAT_WS_CONFIG,
        Some(headers),
        "kt-signal-chat",
    )
    .await?;

    Ok(ChatConnection::finish_connect(
        tokio::runtime::Handle::current(),
        pending,
        HashMap::new(),
        Box::new(|_| {}),
    ))
}

use std::time::Duration;

use libsignal_net::chat::RequestProto;
use libsignal_net::chat::ws::Responder;
use reqwest::StatusCode;

use crate::messagepipe::WsFrameStamp;
use crate::proto::WebSocketRequestMessage;
use crate::websocket::request::ChatMessageConversionError;

/// One incoming chat request, kept in the shape the existing message pipe
/// already consumes.
pub struct ChatIncoming {
    pub message: WebSocketRequestMessage,
    pub responder: Responder,
    /// Receive-side timing for the frame that carried this request.
    pub frame_stamp: WsFrameStamp,
}

/// Converts an official incoming request without touching its body.
pub fn map_request(request: RequestProto) -> WebSocketRequestMessage {
    WebSocketRequestMessage {
        id: request.id,
        verb: request.verb,
        path: request.path,
        body: request.body.map(|body| body.to_vec()),
        headers: request.headers,
    }
}

pub fn map_incoming(request: RequestProto, responder: Responder) -> ChatIncoming {
    // The official listener exposes the decoded request, not the raw frame;
    // its protobuf length is the closest wire-size evidence available here.
    let frame_bytes = request.encoded_len();
    ChatIncoming {
        message: map_request(request),
        responder,
        frame_stamp: WsFrameStamp::capture(frame_bytes),
    }
}

/// Acknowledges one incoming request. Returns whether the response was
/// accepted by the chat task.
pub fn ack_ok(responder: Responder) -> bool {
    ack_status(responder, StatusCode::OK)
}

pub fn ack_status(responder: Responder, status: StatusCode) -> bool {
    responder.send_response(status).is_ok()
}

#[derive(Debug, thiserror::Error)]
pub enum ChatTransportError {
    #[error(transparent)]
    Conversion(#[from] ChatMessageConversionError),
    #[error("chat send failed: {0}")]
    Send(String),
}

/// Sends one existing request over the official chat connection.
pub async fn send_request(
    chat: &ChatConnection,
    message: WebSocketRequestMessage,
    timeout: Duration,
) -> Result<crate::proto::WebSocketResponseMessage, ChatTransportError> {
    let request = libsignal_net::chat::Request::try_from(message)?;
    let response = chat
        .send(request, timeout)
        .await
        .map_err(|error| ChatTransportError::Send(format!("{error:?}")))?;
    Ok(response.try_into()?)
}


use libsignal_net::chat::ws::{
    EventListener, FinishError, FinishReason, ListenerEvent, TaskExitError,
};
use libsignal_net::infra::ws::connection::NextEventError;
use tokio::sync::mpsc::UnboundedSender;

/// Events emitted by the official chat listener.
pub enum ChatEvent {
    Incoming(ChatIncoming),
    Finished(Option<u16>),
    Other,
}

/// Extracts the server close code from an official finish result.
pub fn finish_close_code(
    result: &Result<FinishReason, FinishError>,
) -> Option<u16> {
    match result {
        Err(FinishError::Error(TaskExitError::WebsocketError(
            NextEventError::AbnormalServerClose { code, .. },
        ))) => Some(u16::from(*code)),
        _ => None,
    }
}

/// Classifies an official finish result into a sanitized static label for
/// structured disconnect logs.
pub fn finish_reason_label(
    result: &Result<FinishReason, FinishError>,
) -> &'static str {
    match result {
        Ok(FinishReason::LocalDisconnect) => "local_disconnect",
        Ok(FinishReason::RemoteDisconnect) => "remote_disconnect",
        Err(FinishError::Unknown) => "unknown",
        Err(FinishError::Error(TaskExitError::SendIo(_))) => "send_io",
        Err(FinishError::Error(TaskExitError::SendTooLarge { .. })) => {
            "send_too_large"
        }
        Err(FinishError::Error(TaskExitError::SendProtocol(_))) => {
            "send_protocol"
        }
        Err(FinishError::Error(TaskExitError::WebsocketError(error))) => {
            match error {
                NextEventError::PingFailed(_) => "ping_failed",
                NextEventError::CloseFailed(_) => "close_failed",
                NextEventError::AbnormalServerClose { .. } => {
                    "abnormal_server_close"
                }
                NextEventError::ReceiveError(_) => "receive_error",
                NextEventError::ServerIdleTimeout(_) => "server_idle_timeout",
                NextEventError::UnexpectedConnectionClose => {
                    "unexpected_connection_close"
                }
            }
        }
    }
}

/// Builds the official listener closure. It only classifies and forwards;
/// no decryption, database work or network calls are allowed here.
pub fn listener(sender: UnboundedSender<ChatEvent>) -> EventListener {
    listener_with_reason(sender, None)
}

fn listener_with_reason(
    sender: UnboundedSender<ChatEvent>,
    finish_reason: Option<Arc<Mutex<Option<&'static str>>>>,
) -> EventListener {
    Box::new(move |event| {
        let mapped = match event {
            ListenerEvent::ReceivedMessage(request, responder) => {
                ChatEvent::Incoming(map_incoming(request, responder))
            }
            ListenerEvent::Finished(result) => {
                if let Some(slot) = &finish_reason {
                    if let Ok(mut guard) = slot.lock() {
                        *guard = Some(finish_reason_label(&result));
                    }
                }
                ChatEvent::Finished(finish_close_code(&result))
            }
            _ => ChatEvent::Other,
        };
        let _ = sender.send(mapped);
    })
}



/// Owns the official chat connection and the listener event stream.
///
/// This is the boundary the main loop will consume; it deliberately does not
/// know about SignalWebSocket yet.
pub struct OfficialSocket {
    chat: std::sync::Arc<ChatConnection>,
    events: tokio::sync::mpsc::UnboundedReceiver<ChatEvent>,
    finish_reason: Arc<Mutex<Option<&'static str>>>,
}

impl OfficialSocket {
    pub fn new(
        chat: ChatConnection,
        events: tokio::sync::mpsc::UnboundedReceiver<ChatEvent>,
    ) -> Self {
        Self::new_with_finish_reason(chat, events, Arc::new(Mutex::new(None)))
    }

    fn new_with_finish_reason(
        chat: ChatConnection,
        events: tokio::sync::mpsc::UnboundedReceiver<ChatEvent>,
        finish_reason: Arc<Mutex<Option<&'static str>>>,
    ) -> Self {
        Self {
            chat: std::sync::Arc::new(chat),
            events,
            finish_reason,
        }
    }

    pub fn chat(&self) -> std::sync::Arc<ChatConnection> {
        std::sync::Arc::clone(&self.chat)
    }

    pub fn last_finish_reason(&self) -> Option<&'static str> {
        self.finish_reason.lock().ok().and_then(|guard| *guard)
    }

    pub async fn send(
        &self,
        message: WebSocketRequestMessage,
        timeout: Duration,
    ) -> Result<crate::proto::WebSocketResponseMessage, ChatTransportError> {
        send_request(&self.chat, message, timeout).await
    }

    pub async fn disconnect(&self) {
        self.chat.disconnect().await;
    }

    pub async fn next(&mut self) -> Option<ChatEvent> {
        self.events.recv().await
    }

    pub fn events(
        self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<ChatEvent> {
        self.events
    }
}



/// Send-only half of the official connection, used by SignalWebSocket
/// without changing the receive path yet.
pub struct OfficialRequester {
    chat: std::sync::Arc<ChatConnection>,
    alive: std::sync::atomic::AtomicBool,
}

impl OfficialRequester {
    pub fn new(chat: ChatConnection) -> Self {
        Self {
            chat: std::sync::Arc::new(chat),
            alive: std::sync::atomic::AtomicBool::new(true),
        }
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn request(
        &self,
        message: WebSocketRequestMessage,
        timeout: Duration,
    ) -> Result<crate::proto::WebSocketResponseMessage, ChatTransportError> {
        send_request(&self.chat, message, timeout).await
    }

    pub fn close(&self) {
        self.alive
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}



/// Connects and returns an official socket with a real listener.
pub async fn connect_socket<TC, R>(
    resources: ConnectionResources<'_, TC>,
    service: ServiceName,
    route: R,
    user_agent: &UserAgent,
    headers: ChatHeaders,
) -> Result<OfficialSocket, ConnectError>
where
    TC: WebSocketTransportConnectorFactory<UsePreconnect<TransportRoute>>,
    R: RouteProvider<Route = UnresolvedHttpsServiceRoute>,
{
    let (sender, events) = tokio::sync::mpsc::unbounded_channel();
    let finish_reason = Arc::new(Mutex::new(None));
    let pending = ChatConnection::start_connect_with(
        resources,
        service,
        route,
        "/v1/websocket/",
        user_agent,
        OFFICIAL_CHAT_WS_CONFIG,
        Some(headers),
        "kt-signal-chat",
    )
    .await?;

    let chat = ChatConnection::finish_connect(
        tokio::runtime::Handle::current(),
        pending,
        HashMap::new(),
        listener_with_reason(sender, Some(Arc::clone(&finish_reason))),
    );
    Ok(OfficialSocket::new_with_finish_reason(
        chat,
        events,
        finish_reason,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_proto() -> RequestProto {
        RequestProto {
            id: Some(7),
            verb: Some("PUT".to_owned()),
            path: Some("/api/v1/message".to_owned()),
            body: Some(vec![1, 2, 3].into()),
            headers: vec!["x-test:yes".to_owned()],
        }
    }

    #[test]
    fn finish_without_close_code_stays_none() {
        assert_eq!(
            finish_close_code(&Ok(FinishReason::RemoteDisconnect)),
            None
        );
        assert_eq!(finish_close_code(&Err(FinishError::Unknown)), None);
    }

    #[test]
    fn official_chat_ws_config_probes_before_thirty_second_watchdog() {
        assert_eq!(
            OFFICIAL_CHAT_WS_CONFIG.local_idle_timeout,
            Duration::from_secs(20)
        );
        assert_eq!(
            OFFICIAL_CHAT_WS_CONFIG.remote_idle_timeout,
            Duration::from_secs(30)
        );
        assert!(
            OFFICIAL_CHAT_WS_CONFIG.local_idle_timeout
                < OFFICIAL_CHAT_WS_CONFIG.remote_idle_timeout
        );
        assert!(
            OFFICIAL_CHAT_WS_CONFIG.remote_idle_timeout
                - OFFICIAL_CHAT_WS_CONFIG.local_idle_timeout
                >= Duration::from_secs(10)
        );
        assert!(
            OFFICIAL_CHAT_WS_CONFIG.remote_idle_timeout
                < RECOMMENDED_CHAT_WS_CONFIG.remote_idle_timeout
        );
    }

    #[test]
    fn finish_reason_label_classifies_idle_timeout_and_transport_errors() {
        assert_eq!(
            finish_reason_label(&Ok(FinishReason::LocalDisconnect)),
            "local_disconnect"
        );
        assert_eq!(
            finish_reason_label(&Ok(FinishReason::RemoteDisconnect)),
            "remote_disconnect"
        );
        assert_eq!(finish_reason_label(&Err(FinishError::Unknown)), "unknown");
        assert_eq!(
            finish_reason_label(&Err(FinishError::Error(
                TaskExitError::WebsocketError(
                    NextEventError::ServerIdleTimeout(Duration::from_secs(30)),
                ),
            ))),
            "server_idle_timeout"
        );
        assert_eq!(
            finish_reason_label(&Err(FinishError::Error(
                TaskExitError::WebsocketError(
                    NextEventError::UnexpectedConnectionClose,
                ),
            ))),
            "unexpected_connection_close"
        );
        assert_eq!(
            finish_reason_label(&Err(FinishError::Error(
                TaskExitError::SendIo(std::io::ErrorKind::BrokenPipe),
            ))),
            "send_io"
        );
    }

    #[test]
    fn incoming_request_keeps_wire_shape() {
        let mapped = map_request(request_proto());
        assert_eq!(mapped.id, Some(7));
        assert_eq!(mapped.verb.as_deref(), Some("PUT"));
        assert_eq!(mapped.path.as_deref(), Some("/api/v1/message"));
        assert_eq!(mapped.body.as_deref(), Some(&[1, 2, 3][..]));
        assert_eq!(mapped.headers, vec!["x-test:yes".to_owned()]);
    }
}

