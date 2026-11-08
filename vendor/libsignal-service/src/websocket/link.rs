// SPDX-License-Identifier: AGPL-3.0-only

//! Transport seam between the legacy reqwest websocket and the official
//! libsignal-net chat connection. Not wired into the process loop yet.

#![cfg(feature = "cdsi")]

use bytes::Bytes;
use futures::StreamExt;
use reqwest_websocket::{Message, WebSocket};

use crate::websocket::chat_adapter::{
    ChatEvent, ChatIncoming, OfficialSocket,
};

pub enum WsEvent {
    Frame(Bytes),
    Chat(ChatIncoming),
    Closed(Option<u16>),
    End,
}

pub enum WsLink {
    Legacy(WebSocket),
    Official(OfficialSocket),
}

impl WsLink {
    pub async fn next_event(&mut self) -> WsEvent {
        match self {
            Self::Legacy(ws) => match ws.next().await {
                Some(Ok(Message::Binary(frame))) => WsEvent::Frame(frame),
                Some(Ok(Message::Close { code, .. })) => {
                    WsEvent::Closed(Some(u16::from(code)))
                }
                Some(Ok(_)) => WsEvent::End,
                Some(Err(_)) => WsEvent::End,
                None => WsEvent::End,
            },
            Self::Official(socket) => match socket.next().await {
                Some(ChatEvent::Incoming(incoming)) => WsEvent::Chat(incoming),
                Some(ChatEvent::Finished(code)) => WsEvent::Closed(code),
                Some(ChatEvent::Other) | None => WsEvent::End,
            },
        }
    }
}
