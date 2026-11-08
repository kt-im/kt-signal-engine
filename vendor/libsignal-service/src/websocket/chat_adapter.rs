// SPDX-License-Identifier: AGPL-3.0-only

//! Compile-time spike for the official `libsignal-net` chat transport.
//!
//! This module only builds a `ChatConnection`; it is not wired into
//! `SignalWebSocket` yet.

#![cfg(feature = "cdsi")]

use std::collections::HashMap;

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
        RECOMMENDED_CHAT_WS_CONFIG,
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

use crate::proto::WebSocketRequestMessage;
use crate::websocket::request::ChatMessageConversionError;

/// One incoming chat request, kept in the shape the existing message pipe
/// already consumes.
pub struct ChatIncoming {
    pub message: WebSocketRequestMessage,
    pub responder: Responder,
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
    ChatIncoming {
        message: map_request(request),
        responder,
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

/// Builds the official listener closure. It only classifies and forwards;
/// no decryption, database work or network calls are allowed here.
pub fn listener(sender: UnboundedSender<ChatEvent>) -> EventListener {
    Box::new(move |event| {
        let mapped = match event {
            ListenerEvent::ReceivedMessage(request, responder) => {
                ChatEvent::Incoming(map_incoming(request, responder))
            }
            ListenerEvent::Finished(result) => {
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
    chat: ChatConnection,
    events: tokio::sync::mpsc::UnboundedReceiver<ChatEvent>,
}

impl OfficialSocket {
    pub fn new(
        chat: ChatConnection,
        events: tokio::sync::mpsc::UnboundedReceiver<ChatEvent>,
    ) -> Self {
        Self { chat, events }
    }

    pub async fn send(
        &self,
        message: WebSocketRequestMessage,
        timeout: Duration,
    ) -> Result<crate::proto::WebSocketResponseMessage, ChatTransportError> {
        send_request(&self.chat, message, timeout).await
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
    fn incoming_request_keeps_wire_shape() {
        let mapped = map_request(request_proto());
        assert_eq!(mapped.id, Some(7));
        assert_eq!(mapped.verb.as_deref(), Some("PUT"));
        assert_eq!(mapped.path.as_deref(), Some("/api/v1/message"));
        assert_eq!(mapped.body.as_deref(), Some(&[1, 2, 3][..]));
        assert_eq!(mapped.headers, vec!["x-test:yes".to_owned()]);
    }
}

