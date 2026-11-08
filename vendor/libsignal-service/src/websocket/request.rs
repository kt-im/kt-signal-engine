use std::fmt;

use reqwest::Method;
use serde::Serialize;

#[cfg(feature = "cdsi")]
use bytes::Bytes;
#[cfg(feature = "cdsi")]
use libsignal_net::chat::{Request as ChatRequest, Response as ChatResponse};
#[cfg(feature = "cdsi")]
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    StatusCode,
};

use crate::{
    content::ServiceError,
    proto::{WebSocketRequestMessage, WebSocketResponseMessage},
    websocket::{SignalWebSocket, WebSocketType},
};

#[derive(Debug)]
pub struct WebSocketRequestMessageBuilder {
    request: WebSocketRequestMessage,
}

impl WebSocketRequestMessage {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(method: Method) -> WebSocketRequestMessageBuilder {
        WebSocketRequestMessageBuilder {
            request: WebSocketRequestMessage {
                verb: Some(method.to_string()),
                ..Default::default()
            },
        }
    }
}

impl WebSocketRequestMessageBuilder {
    pub fn id(mut self, id: u64) -> Self {
        self.request.id = Some(id);
        self
    }

    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.request.path = Some(path.into());
        self
    }

    pub fn header(mut self, key: &str, value: impl AsRef<str>) -> Self {
        self.request
            .headers
            .push(format!("{key}:{}", value.as_ref()));
        self
    }

    pub fn json<S: Serialize>(
        mut self,
        value: S,
    ) -> Result<WebSocketRequestMessage, serde_json::Error> {
        self.request.body = Some(serde_json::to_vec(&value)?);
        Ok(self.header("content-type", "application/json").request)
    }

    pub fn build(self) -> WebSocketRequestMessage {
        self.request
    }
}

pub(crate) struct WebSocketRequestBuilder<'a, C: WebSocketType> {
    ws: &'a mut SignalWebSocket<C>,
    message_builder: WebSocketRequestMessageBuilder,
}

impl<C: WebSocketType> SignalWebSocket<C> {
    #[tracing::instrument(skip(self))]
    pub(crate) fn http_request(
        &mut self,
        method: Method,
        path: impl Into<String> + fmt::Debug,
    ) -> Result<WebSocketRequestBuilder<'_, C>, ServiceError> {
        Ok(WebSocketRequestBuilder {
            ws: self,
            message_builder: WebSocketRequestMessage::new(method).path(path),
        })
    }
}

impl<C: WebSocketType> WebSocketRequestBuilder<'_, C> {
    pub(crate) fn registration_auth_header(
        mut self,
        phonenumber: libsignal_core::E164,
        password: &str,
    ) -> Self {
        use base64::engine::{general_purpose::STANDARD, Engine};
        let encoded =
            STANDARD.encode(format!("{phonenumber}:{password}").as_bytes());
        self.message_builder = self
            .message_builder
            .header("Authorization", format!("Basic {encoded}"));
        self
    }

    pub(crate) async fn send_json<B: Serialize>(
        self,
        value: B,
    ) -> Result<WebSocketResponseMessage, ServiceError> {
        let request = self.message_builder.json(value)?;
        self.ws.request(request).await
    }

    pub(crate) async fn send(
        self,
    ) -> Result<WebSocketResponseMessage, ServiceError> {
        let request = self.message_builder.build();
        self.ws.request(request).await
    }
}

/// Why a protobuf websocket message could not be converted to or from the
/// official `libsignal_net::chat` types.
#[cfg(feature = "cdsi")]
#[derive(Debug, Copy, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChatMessageConversionError {
    #[error("websocket request message has no verb")]
    MissingVerb,
    #[error("request verb is not a valid HTTP method")]
    InvalidMethod,
    #[error("request path is missing or not a valid path and query")]
    InvalidPath,
    #[error("header is not a valid `name:value` pair")]
    InvalidHeader,
    #[error("response status is missing or not a valid HTTP status")]
    InvalidStatus,
    #[error("header value is not valid UTF-8")]
    NonUtf8HeaderValue,
}

/// Splits `name:value` protobuf headers into a `HeaderMap`, trimming the
/// value as the official conversion does.
#[cfg(feature = "cdsi")]
fn header_map_from_proto(
    headers: Vec<String>,
) -> Result<HeaderMap, ChatMessageConversionError> {
    headers.into_iter().try_fold(
        HeaderMap::new(),
        |mut map, header| -> Result<HeaderMap, ChatMessageConversionError> {
            let (name, value) = header
                .split_once(':')
                .ok_or(ChatMessageConversionError::InvalidHeader)?;
            let name = HeaderName::try_from(name)
                .map_err(|_| ChatMessageConversionError::InvalidHeader)?;
            let value = HeaderValue::from_str(value.trim())
                .map_err(|_| ChatMessageConversionError::InvalidHeader)?;
            map.append(name, value);
            Ok(map)
        },
    )
}

/// Flattens a `HeaderMap` back into `name:value` protobuf headers. Names
/// come back lowercase (`HeaderMap` normalises them); Signal treats header
/// names case-insensitively. Values that are not UTF-8 cannot be represented
/// in the protobuf field.
#[cfg(feature = "cdsi")]
fn header_map_to_proto(
    headers: &HeaderMap,
) -> Result<Vec<String>, ChatMessageConversionError> {
    headers
        .iter()
        .map(|(name, value)| {
            let value = value
                .to_str()
                .map_err(|_| ChatMessageConversionError::NonUtf8HeaderValue)?;
            Ok(format!("{}:{}", name.as_str(), value))
        })
        .collect()
}

/// The protobuf `id` is dropped: `libsignal_net::chat` assigns request ids
/// when framing (`OutgoingRequest::make_message` in `chat::ws`), so the id
/// carried by a protobuf message is not the one the chat layer will use.
#[cfg(feature = "cdsi")]
impl TryFrom<WebSocketRequestMessage> for ChatRequest {
    type Error = ChatMessageConversionError;

    fn try_from(message: WebSocketRequestMessage) -> Result<Self, Self::Error> {
        let WebSocketRequestMessage {
            verb,
            path,
            body,
            headers,
            id: _,
        } = message;
        Ok(ChatRequest {
            method: Method::from_bytes(
                verb.ok_or(ChatMessageConversionError::MissingVerb)?
                    .as_bytes(),
            )
            .map_err(|_| ChatMessageConversionError::InvalidMethod)?,
            path: path
                .ok_or(ChatMessageConversionError::InvalidPath)?
                .parse()
                .map_err(|_| ChatMessageConversionError::InvalidPath)?,
            headers: header_map_from_proto(headers)?,
            body: body.map(Bytes::from),
        })
    }
}

/// The request id is left unset for the same reason: the chat layer assigns
/// it, see `TryFrom<WebSocketRequestMessage> for ChatRequest`.
#[cfg(feature = "cdsi")]
impl TryFrom<ChatRequest> for WebSocketRequestMessage {
    type Error = ChatMessageConversionError;

    fn try_from(request: ChatRequest) -> Result<Self, Self::Error> {
        let ChatRequest {
            method,
            path,
            headers,
            body,
        } = request;
        Ok(WebSocketRequestMessage {
            id: None,
            verb: Some(method.to_string()),
            path: Some(path.to_string()),
            body: body.map(|body| body.to_vec()),
            headers: header_map_to_proto(&headers)?,
        })
    }
}

/// The response id is dropped; responses are framed with the id of the
/// request they answer.
#[cfg(feature = "cdsi")]
impl TryFrom<WebSocketResponseMessage> for ChatResponse {
    type Error = ChatMessageConversionError;

    fn try_from(
        message: WebSocketResponseMessage,
    ) -> Result<Self, Self::Error> {
        let WebSocketResponseMessage {
            id: _,
            status,
            message,
            headers,
            body,
        } = message;
        let status = u16::try_from(status.unwrap_or_default())
            .ok()
            .and_then(|status| StatusCode::from_u16(status).ok())
            .ok_or(ChatMessageConversionError::InvalidStatus)?;
        Ok(ChatResponse {
            status,
            message,
            headers: header_map_from_proto(headers)?,
            body: body.map(Bytes::from),
        })
    }
}

/// The response id is left unset; see
/// `TryFrom<WebSocketResponseMessage> for ChatResponse`.
#[cfg(feature = "cdsi")]
impl TryFrom<ChatResponse> for WebSocketResponseMessage {
    type Error = ChatMessageConversionError;

    fn try_from(response: ChatResponse) -> Result<Self, Self::Error> {
        let ChatResponse {
            status,
            message,
            headers,
            body,
        } = response;
        Ok(WebSocketResponseMessage {
            id: None,
            status: Some(u32::from(status.as_u16())),
            message,
            headers: header_map_to_proto(&headers)?,
            body: body.map(|body| body.to_vec()),
        })
    }
}

#[cfg(all(test, feature = "cdsi"))]
mod tests {
    use super::*;

    fn message(verb: &str, path: &str) -> WebSocketRequestMessage {
        WebSocketRequestMessage::new(
            Method::from_bytes(verb.as_bytes()).unwrap(),
        )
        .path(path)
        .build()
    }

    #[test]
    fn get_request_without_body_keeps_method_and_path() {
        let request =
            ChatRequest::try_from(message("GET", "/v1/keepalive")).unwrap();
        assert_eq!(request.method, Method::GET);
        assert_eq!(request.path, "/v1/keepalive");
        assert!(request.body.is_none());
        assert!(request.headers.is_empty());

        let back = WebSocketRequestMessage::try_from(request).unwrap();
        assert_eq!(back.verb.as_deref(), Some("GET"));
        assert_eq!(back.path.as_deref(), Some("/v1/keepalive"));
        assert_eq!(back.id, None);
        assert!(back.body.is_none());
        assert!(back.headers.is_empty());
    }

    #[test]
    fn put_request_json_body_survives_both_directions() {
        let payload = serde_json::json!({
            "destination": "+15551234567",
            "timestamp": 1_700_000_000_000u64,
        });
        let proto = WebSocketRequestMessage::new(Method::PUT)
            .path("/v1/messages")
            .json(&payload)
            .unwrap();
        let expected = serde_json::to_vec(&payload).unwrap();

        let request = ChatRequest::try_from(proto).unwrap();
        assert_eq!(request.method, Method::PUT);
        assert_eq!(request.path, "/v1/messages");
        assert_eq!(request.body.as_deref(), Some(expected.as_slice()));

        let back = WebSocketRequestMessage::try_from(request).unwrap();
        assert_eq!(back.verb.as_deref(), Some("PUT"));
        assert_eq!(back.body, Some(expected));
    }

    #[test]
    fn headers_convert_both_ways() {
        let mut proto = message("PUT", "/v1/messages");
        proto.headers.push("Authorization: Basic abc".to_string());
        proto
            .headers
            .push("x-signal-receive-stories:true".to_string());

        let request = ChatRequest::try_from(proto).unwrap();
        assert_eq!(request.headers.get("authorization").unwrap(), "Basic abc");
        assert_eq!(
            request.headers.get("x-signal-receive-stories").unwrap(),
            "true"
        );

        let back = WebSocketRequestMessage::try_from(request).unwrap();
        let mut headers = back.headers;
        headers.sort();
        assert_eq!(
            headers,
            vec![
                "authorization:Basic abc".to_string(),
                "x-signal-receive-stories:true".to_string(),
            ]
        );
    }

    #[test]
    fn empty_header_is_rejected() {
        let mut proto = message("GET", "/v1/keepalive");
        proto.headers.push(String::new());
        assert_eq!(
            ChatRequest::try_from(proto).unwrap_err(),
            ChatMessageConversionError::InvalidHeader
        );
    }

    #[test]
    fn malformed_header_is_rejected() {
        for header in ["no-colon", "bad name: value", "x-bad: a\u{1}b"] {
            let mut proto = message("GET", "/v1/keepalive");
            proto.headers.push(header.to_string());
            assert_eq!(
                ChatRequest::try_from(proto).unwrap_err(),
                ChatMessageConversionError::InvalidHeader,
                "header {header:?} should be rejected"
            );
        }
    }

    #[test]
    fn response_status_and_body_convert_both_ways() {
        let proto = WebSocketResponseMessage {
            id: Some(7),
            status: Some(404),
            message: Some("Not Found".to_string()),
            headers: vec!["content-type: application/json".to_string()],
            body: Some(br#"{"error":"not found"}"#.to_vec()),
        };

        let response = ChatResponse::try_from(proto).unwrap();
        assert_eq!(response.status, StatusCode::NOT_FOUND);
        assert_eq!(response.message.as_deref(), Some("Not Found"));
        assert_eq!(
            response.headers.get("content-type").unwrap(),
            "application/json"
        );
        assert_eq!(
            response.body.as_deref(),
            Some(br#"{"error":"not found"}"#.as_slice())
        );

        let back = WebSocketResponseMessage::try_from(response).unwrap();
        assert_eq!(back.id, None);
        assert_eq!(back.status, Some(404));
        assert_eq!(back.message.as_deref(), Some("Not Found"));
        assert_eq!(back.headers, vec!["content-type:application/json"]);
        assert_eq!(back.body, Some(br#"{"error":"not found"}"#.to_vec()));
    }

    #[test]
    fn response_without_status_is_rejected() {
        assert_eq!(
            ChatResponse::try_from(WebSocketResponseMessage::default())
                .unwrap_err(),
            ChatMessageConversionError::InvalidStatus
        );
    }
}
