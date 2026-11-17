// SPDX-License-Identifier: AGPL-3.0-only

//! Transport seam between the legacy reqwest websocket and the official
//! libsignal-net chat connection. Not wired into the process loop yet.

#![cfg(feature = "cdsi")]

use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use prost::Message as _;
use reqwest_websocket::{CloseCode, Message, WebSocket};

use crate::messagepipe::WsFrameStamp;
use crate::proto::WebSocketMessage;
use crate::websocket::chat_adapter::{
    ChatEvent, ChatIncoming, OfficialSocket,
};
use crate::websocket::WsWriteError;

pub(crate) enum WsEvent {
    Frame(Bytes, WsFrameStamp),
    #[allow(dead_code)] // consumed once the official listener is wired
    Chat(ChatIncoming),
    Closed(Option<u16>),
    End,
}

#[allow(dead_code)] // official variant is wired in the next commit
pub(crate) enum WsLink {
    Legacy(WebSocket),
    Official(OfficialSocket),
}

#[allow(dead_code)]
impl WsLink {
    pub(crate) fn from_official(socket: OfficialSocket) -> Self {
        Self::Official(socket)
    }

    pub(crate) async fn send_frame(
        &mut self,
        message: WebSocketMessage,
        timeout: Duration,
    ) -> Result<(), WsWriteError> {
        match self {
            Self::Legacy(ws) => {
                let buffer = message.encode_to_vec();
                match tokio::time::timeout(
                    timeout,
                    ws.send(Message::Binary(buffer.into())),
                )
                .await
                {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(_)) => Err(WsWriteError::Failed),
                    Err(_) => Err(WsWriteError::TimedOut),
                }
            }
            Self::Official(socket) => {
                let Some(request) = message.request else {
                    return Err(WsWriteError::Failed);
                };
                socket
                    .send(request, timeout)
                    .await
                    .map(|_| ())
                    .map_err(|_| WsWriteError::Failed)
            }
        }
    }

    pub(crate) async fn close(
        &mut self,
        code: CloseCode,
        reason: &str,
        timeout: Duration,
    ) {
        match self {
            Self::Legacy(ws) => {
                let _ = tokio::time::timeout(timeout, async {
                    let _ = ws
                        .send(Message::Close {
                            code,
                            reason: reason.to_owned(),
                        })
                        .await;
                    let _ = SinkExt::close(ws).await;
                })
                .await;
            }
            Self::Official(socket) => {
                socket.disconnect().await;
            }
        }
    }

    pub(crate) async fn next_event(&mut self) -> WsEvent {
        match self {
            Self::Legacy(ws) => loop {
                match ws.next().await {
                    Some(Ok(Message::Binary(frame))) => {
                        let stamp = WsFrameStamp::capture(frame.len());
                        return WsEvent::Frame(frame, stamp);
                    }
                    Some(Ok(Message::Close { code, .. })) => {
                        return WsEvent::Closed(Some(u16::from(code)));
                    }
                    Some(Ok(Message::Ping(_)))
                    | Some(Ok(Message::Pong(_)))
                    | Some(Ok(Message::Text(_))) => continue,
                    Some(Err(_)) | None => return WsEvent::End,
                }
            },
            Self::Official(socket) => match socket.next().await {
                Some(ChatEvent::Incoming(incoming)) => WsEvent::Chat(incoming),
                Some(ChatEvent::Finished(code)) => WsEvent::Closed(code),
                Some(ChatEvent::Other) | None => WsEvent::End,
            },
        }
    }
}
