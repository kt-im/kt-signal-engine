use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::{
    channel::{
        mpsc::{self, Sender},
        oneshot,
    },
    prelude::*,
};
use prost::Message;

pub use crate::{
    configuration::ServiceCredentials,
    proto::{
        web_socket_message, Envelope, WebSocketMessage,
        WebSocketRequestMessage, WebSocketResponseMessage,
    },
};

use crate::{
    push_service::ServiceError,
    websocket::{self, SignalWebSocket},
};

/// Receive-side evidence for one inbound websocket frame.
///
/// Captured where the transport hands a frame to the receive stack, carried
/// with the envelope, and read again when the receive loop sees it. This is
/// deliberately a plain copyable value: the probe must not add work or failure
/// modes to the receive path.
#[derive(Clone, Copy, Debug)]
pub struct WsFrameStamp {
    received_at: Instant,
    seq: u64,
    frame_bytes: usize,
}

impl WsFrameStamp {
    /// Stamp one frame and give it a process-wide monotonic sequence number.
    pub fn capture(frame_bytes: usize) -> Self {
        static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);
        Self {
            received_at: Instant::now(),
            seq: NEXT_SEQ.fetch_add(1, Ordering::Relaxed),
            frame_bytes,
        }
    }

    /// Build a stamp for tests or for callers that already timed the frame.
    pub fn from_parts(
        received_at: Instant,
        seq: u64,
        frame_bytes: usize,
    ) -> Self {
        Self {
            received_at,
            seq,
            frame_bytes,
        }
    }

    pub fn received_at(&self) -> Instant {
        self.received_at
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    /// Milliseconds from frame receipt to now (saturating, never panics).
    pub fn delay_ms_until(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.received_at).as_millis() as u64
    }
}

/// Frames logged unconditionally after a stream starts (cold-start evidence).
pub const LATENCY_PROBE_WARMUP_FRAMES: u64 = 50;
/// Minimum gap between sampled logs once the warmup window is over.
pub const LATENCY_PROBE_MIN_INTERVAL: Duration = Duration::from_millis(200);
/// Only a frame-to-loop delay at least this large escapes the steady-state
/// filter (this is the local-queueing signal the probe exists for).
pub const LATENCY_PROBE_DELAY_THRESHOLD_MS: u64 = 200;

/// Sampling policy shared by the frame-received and frame-to-loop log sites.
///
/// Steady state stays quiet: the first LATENCY_PROBE_WARMUP_FRAMES frames
/// are always logged, then at most one line per LATENCY_PROBE_MIN_INTERVAL.
/// The loop-stage gate additionally requires
/// LATENCY_PROBE_DELAY_THRESHOLD_MS so ordinary sub-200ms batching does not
/// flood the log.
#[derive(Debug, Default)]
pub struct FrameLatencySampler {
    observed: u64,
    last_emit: Option<Instant>,
}

impl FrameLatencySampler {
    pub fn new() -> Self {
        Self::default()
    }

    fn warmup(&mut self, now: Instant) -> bool {
        self.observed = self.observed.saturating_add(1);
        if self.observed <= LATENCY_PROBE_WARMUP_FRAMES {
            self.last_emit = Some(now);
            return true;
        }
        false
    }

    fn interval_elapsed(&mut self, now: Instant) -> bool {
        if self.last_emit.is_none_or(|last| {
            now.saturating_duration_since(last) >= LATENCY_PROBE_MIN_INTERVAL
        }) {
            self.last_emit = Some(now);
            true
        } else {
            false
        }
    }

    /// Whether to log ws_frame_received for the next frame.
    pub fn should_log_received(&mut self, now: Instant) -> bool {
        if self.warmup(now) {
            return true;
        }
        self.interval_elapsed(now)
    }

    /// Whether to log ws_frame_to_loop for an envelope that took delay_ms.
    pub fn should_log_loop(&mut self, delay_ms: u64, now: Instant) -> bool {
        if self.warmup(now) {
            return true;
        }
        if delay_ms < LATENCY_PROBE_DELAY_THRESHOLD_MS {
            return false;
        }
        self.interval_elapsed(now)
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Incoming {
    /// The ack is deferred: the [`EnvelopeAck`] handle must be completed by the
    /// consumer after processing (Java: ack after "you handled it", by the caller).
    Envelope(Envelope, EnvelopeAck),
    QueueEmpty,
}

/// Deferred ack handle for one envelope. The 200 response is only written
/// when [`EnvelopeAck::ack`] is called; dropping the handle without acking
/// leaves the request unanswered so the server redelivers it later.
#[derive(Debug)]
pub struct EnvelopeAck {
    response: WebSocketResponseMessage,
    responder: oneshot::Sender<WebSocketResponseMessage>,
    /// Receive time of the websocket frame that carried this envelope.
    frame_stamp: Option<WsFrameStamp>,
    #[cfg(feature = "cdsi")]
    chat_responder: Option<libsignal_net::chat::ws::Responder>,
}

impl EnvelopeAck {
    /// Frame receipt evidence carried alongside the envelope, when known.
    pub fn frame_stamp(&self) -> Option<WsFrameStamp> {
        self.frame_stamp
    }

    /// Send the 200 ack. Returns false when the socket is already gone.
    pub fn ack(self) -> bool {
        #[cfg(feature = "cdsi")]
        if let Some(responder) = self.chat_responder {
            return crate::websocket::chat_adapter::ack_ok(responder);
        }
        self.responder.send(self.response).is_ok()
    }
}

pub struct MessagePipe {
    ws: SignalWebSocket<websocket::Identified>,
    socket_id: u32,
}

impl MessagePipe {
    pub fn from_socket(ws: SignalWebSocket<websocket::Identified>) -> Self {
        let socket_id = ws.socket_id();
        tracing::warn!(
            target: "signal.pipe",
            stage = "pipe_attached",
            socket_id,
            "message pipe attached to socket"
        );
        MessagePipe { ws, socket_id }
    }

    /// Return a SignalWebSocket for sending messages and other purposes beyond receiving messages.
    pub fn ws(&self) -> SignalWebSocket<websocket::Identified> {
        self.ws.clone()
    }

    /// Worker task that processes the websocket into Envelopes
    async fn run(
        mut self,
        sink: mpsc::UnboundedSender<Result<Incoming, ServiceError>>,
    ) {
        let mut ws = self.ws.clone();
        let mut stream = ws
            .take_request_stream()
            .expect("web socket request handler not in use");

        loop {
            let wait_started = std::time::Instant::now();
            let Some((request, responder, frame_stamp)) = stream.next().await else {
                break;
            };
            let waited_ms = wait_started.elapsed().as_millis() as u64;
            tracing::warn!(
                target: "signal.pipe",
                stage = "pipe_event_wait",
                socket_id = self.socket_id,
                waited_ms,
                "pipeline item dequeued from socket"
            );
            let decode_started = std::time::Instant::now();
            // WebsocketConnection::onMessage(ByteString)
            if let Some(env) = self
                .process_request(request, responder, frame_stamp)
                .await
                .transpose()
            {
                let decode_ms = decode_started.elapsed().as_millis() as u64;
                let send_started = std::time::Instant::now();
                if sink.unbounded_send(env).is_err() {
                    break;
                }
                let blocked_ms = send_started.elapsed().as_millis() as u64;
                if decode_ms >= 200 {
                    tracing::warn!(
                        target: "signal.pipe",
                        stage = "pipe_decode",
                        decode_ms,
                        "pipeline item decoded"
                    );
                }
                if blocked_ms >= 200 {
                    tracing::warn!(
                        target: "signal.pipe",
                        stage = "pipe_send",
                        blocked_ms,
                        "pipeline item handed to consumer"
                    );
                }
            } else {
                tracing::trace!("got empty message in websocket");
            }
        }

        ws.return_request_stream(stream);
    }

    async fn process_request(
        &mut self,
        request: WebSocketRequestMessage,
        responder: oneshot::Sender<WebSocketResponseMessage>,
        frame_stamp: WsFrameStamp,
    ) -> Result<Option<Incoming>, ServiceError> {
        // Java: MessagePipe::read
        let response = WebSocketResponseMessage::from_request(&request);

        // XXX Change the signature of this method to yield an enum of Envelope and EndOfQueue
        if request.is_signal_service_envelope() {
            let body = if let Some(body) = request.body.as_ref() {
                body
            } else {
                return Err(ServiceError::InvalidFrame {
                    reason: "request without body.",
                });
            };
            // Envelopes yield unacked; the consumer acks via the handle once the
            // envelope is decrypted, stored and projected (Java: MessagePipe::read
            // acks after "you handled it").
            let envelope = Envelope::decode(body.as_slice())?;
            return Ok(Some(Incoming::Envelope(
                envelope,
                EnvelopeAck {
                    response,
                    responder,
                    frame_stamp: Some(frame_stamp),
                    #[cfg(feature = "cdsi")]
                    chat_responder: None,
                },
            )));
        }

        let result = if request.is_queue_empty() {
            Some(Incoming::QueueEmpty)
        } else {
            None
        };

        responder
            .send(response)
            .map_err(|_| ServiceError::WsClosing {
                reason: "could not respond to message pipe request",
            })?;

        Ok(result)
    }

    /// Processes one request received from the official chat listener.
    ///
    /// This is the additive path; the legacy `run` loop is unchanged until
    /// the transport switch lands.
    #[cfg(feature = "cdsi")]
    pub fn process_chat_incoming(
        &mut self,
        incoming: crate::websocket::chat_adapter::ChatIncoming,
    ) -> Result<Option<Incoming>, ServiceError> {
        decode_chat_incoming(incoming)
    }

    /// Returns the stream of `Envelope`s
    ///
    /// Envelopes yield an [`EnvelopeAck`] handle; the consumer acks after
    /// processing. Queue markers ack immediately.
    pub fn stream(self) -> impl Stream<Item = Result<Incoming, ServiceError>> {
        // Unbounded relay driven by tokio::select so the consumer's waker is
        // registered on the receiver (the previous select-composition lost it).
        let (sink, rx) = mpsc::unbounded();
        let runner = Box::pin(self.run(sink));
        futures::stream::unfold((runner, rx), |(mut runner, mut rx)| async move {
            tokio::select! {
                item = rx.next() => item.map(|item| (item, (runner, rx))),
                _ = &mut runner => None,
            }
        })
    }
}

/// Decodes one official incoming request into the existing `Incoming` shape.
#[cfg(feature = "cdsi")]
pub fn decode_chat_incoming(
    incoming: crate::websocket::chat_adapter::ChatIncoming,
) -> Result<Option<Incoming>, ServiceError> {
    let request = incoming.message;
    let responder = incoming.responder;
    let frame_stamp = incoming.frame_stamp;

    if request.is_signal_service_envelope() {
        let Some(body) = request.body.as_ref() else {
            crate::websocket::chat_adapter::ack_ok(responder);
            return Err(ServiceError::InvalidFrame {
                reason: "request without body.",
            });
        };
        let envelope = match Envelope::decode(body.as_slice()) {
            Ok(envelope) => envelope,
            Err(error) => {
                crate::websocket::chat_adapter::ack_ok(responder);
                return Err(error.into());
            }
        };
        let (responder_tx, _responder_rx) = oneshot::channel();
        return Ok(Some(Incoming::Envelope(
            envelope,
            EnvelopeAck {
                response: WebSocketResponseMessage::from_request(&request),
                responder: responder_tx,
                frame_stamp: Some(frame_stamp),
                chat_responder: Some(responder),
            },
        )));
    }

    if request.is_queue_empty() {
        crate::websocket::chat_adapter::ack_ok(responder);
        return Ok(Some(Incoming::QueueEmpty));
    }

    crate::websocket::chat_adapter::ack_status(
        responder,
        reqwest::StatusCode::BAD_REQUEST,
    );
    Ok(None)
}

/// Consumes listener events from the official chat connection.
#[cfg(feature = "cdsi")]
pub fn stream_from_chat(
    mut events: tokio::sync::mpsc::UnboundedReceiver<
        crate::websocket::chat_adapter::ChatEvent,
    >,
) -> impl Stream<Item = Result<Incoming, ServiceError>> {
    use crate::websocket::chat_adapter::ChatEvent;

    let (mut sink, stream) = mpsc::channel::<Result<Incoming, ServiceError>>(64);
    let runner = async move {
        loop {
            let wait_started = std::time::Instant::now();
            let Some(event) = events.recv().await else {
                break;
            };
            tracing::warn!(
                target: "signal.pipe",
                stage = "pipe_event_wait",
                waited_ms = wait_started.elapsed().as_millis() as u64,
                "pipeline event dequeued"
            );
            match event {
                ChatEvent::Incoming(incoming) => {
                    match decode_chat_incoming(incoming) {
                        Ok(Some(item)) => {
                            let send_started = std::time::Instant::now();
                            let send_failed = sink.send(Ok(item)).await.is_err();
                            tracing::warn!(
                                target: "signal.pipe",
                                stage = "pipe_send",
                                blocked_ms = send_started.elapsed().as_millis() as u64,
                                "pipeline item handed to consumer"
                            );
                            if send_failed {
                                break;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let send_started = std::time::Instant::now();
                            let send_failed = sink.send(Err(error)).await.is_err();
                            tracing::warn!(
                                target: "signal.pipe",
                                stage = "pipe_send",
                                blocked_ms = send_started.elapsed().as_millis() as u64,
                                "pipeline error handed to consumer"
                            );
                            if send_failed {
                                break;
                            }
                        }
                    }
                }
                ChatEvent::Finished(_) => break,
                ChatEvent::Other => {}
            }
        }
    };

    let stream = stream.map(Some);
    let combined = futures::stream::select(
        stream,
        runner
            .into_stream()
            .map(|_| None::<Result<Incoming, ServiceError>>),
    );
    combined.filter_map(|x| async { x })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp_ack(received_at: Instant) -> EnvelopeAck {
        let (responder, _receiver) = oneshot::channel();
        EnvelopeAck {
            response: WebSocketResponseMessage::default(),
            responder,
            frame_stamp: Some(WsFrameStamp::from_parts(received_at, 7, 1234)),
            #[cfg(feature = "cdsi")]
            chat_responder: None,
        }
    }

    #[test]
    fn envelope_ack_writes_the_response_exactly_once() {
        let (responder, mut receiver) = oneshot::channel();
        let ack = EnvelopeAck {
            response: WebSocketResponseMessage::default(),
            responder,
            frame_stamp: None,
            #[cfg(feature = "cdsi")]
            chat_responder: None,
        };

        assert!(ack.ack());
        assert!(receiver.try_recv().is_ok());
    }

    #[test]
    fn envelope_ack_reports_false_once_the_socket_is_gone() {
        let (responder, receiver) = oneshot::channel();
        let ack = EnvelopeAck {
            response: WebSocketResponseMessage::default(),
            responder,
            frame_stamp: None,
            #[cfg(feature = "cdsi")]
            chat_responder: None,
        };
        drop(receiver);

        assert!(!ack.ack());
    }

    #[test]
    fn stamped_envelope_computes_delay_from_capture_instant() {
        let now = Instant::now();
        let received_at = now
            .checked_sub(Duration::from_millis(250))
            .expect("clock is far enough from the epoch");
        let ack = stamp_ack(received_at);

        let stamp = ack.frame_stamp().expect("stamped envelope");
        assert_eq!(stamp.seq(), 7);
        assert_eq!(stamp.frame_bytes(), 1234);
        assert_eq!(stamp.delay_ms_until(now), 250);
        // Saturating: a clock reading earlier than receipt yields 0, not panic.
        assert_eq!(stamp.delay_ms_until(received_at), 0);
    }

    #[test]
    fn frame_sampler_warmup_logs_first_fifty_frames_then_throttles() {
        let mut sampler = FrameLatencySampler::new();
        let start = Instant::now();

        for _ in 0..LATENCY_PROBE_WARMUP_FRAMES {
            assert!(sampler.should_log_received(start));
        }
        assert!(!sampler.should_log_received(start + Duration::from_millis(199)));
        assert!(sampler.should_log_received(start + Duration::from_millis(200)));
    }

    #[test]
    fn frame_sampler_loop_gate_requires_delay_and_interval() {
        let mut sampler = FrameLatencySampler::new();
        let start = Instant::now();

        for _ in 0..LATENCY_PROBE_WARMUP_FRAMES {
            assert!(sampler.should_log_loop(0, start));
        }
        // Warmup is over: sub-threshold delay is dropped outright.
        assert!(!sampler.should_log_loop(199, start));
        // At/above the threshold it logs, then the interval throttles repeats.
        assert!(sampler.should_log_loop(200, start + Duration::from_millis(200)));
        assert!(!sampler.should_log_loop(200, start + Duration::from_millis(399)));
        assert!(sampler.should_log_loop(200, start + Duration::from_millis(400)));
    }

    #[test]
    fn incoming_envelope_carries_the_frame_stamp() {
        let now = Instant::now();
        let received_at = now
            .checked_sub(Duration::from_millis(250))
            .expect("clock is far enough from the epoch");
        let incoming = Incoming::Envelope(Envelope::default(), stamp_ack(received_at));

        match incoming {
            Incoming::Envelope(_, ack) => {
                let stamp = ack.frame_stamp().expect("stamped envelope");
                assert_eq!(stamp.delay_ms_until(now), 250);
                assert_eq!(stamp.seq(), 7);
            },
            Incoming::QueueEmpty => panic!("expected an envelope item"),
        }
    }

    #[test]
    fn frame_stamps_are_monotonic() {
        let first = WsFrameStamp::capture(10);
        let second = WsFrameStamp::capture(20);
        assert!(second.seq() > first.seq());
        assert_eq!(second.frame_bytes(), 20);
    }
}
