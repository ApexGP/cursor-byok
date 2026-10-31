//! Subscribes Cursor RunSSE clients to replayable Transport output.
use axum::{
    body::Body,
    http::{header, HeaderValue, Response, StatusCode},
};
use bytes::Bytes;
use std::{convert::Infallible, time::Duration};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::{
    cursor::{
        protocol::connect::{self, END_STREAM_FLAG},
        services::observability::CursorTraceRecorder,
        transport::{TransportHandle, TransportRegistry},
    },
    Result,
};

const RUN_DISCONNECT_GRACE: Duration = Duration::from_secs(15);

pub async fn stream(registry: &TransportRegistry, request_id: &str) -> Result<Response<Body>> {
    let handle = registry.get_or_create(request_id).await?;
    let receiver = handle.subscribe();
    let trace = handle.trace().cloned();
    if let Some(trace) = &trace {
        trace.response_started(StatusCode::OK.as_u16());
    }
    let body_stream = local_body_stream(receiver, handle, trace);
    let mut response = Response::new(Body::from_stream(body_stream));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert("connect-protocol-version", HeaderValue::from_static("1"));
    Ok(response)
}

fn local_body_stream(
    receiver: mpsc::UnboundedReceiver<Bytes>,
    handle: TransportHandle,
    trace: Option<CursorTraceRecorder>,
) -> impl tokio_stream::Stream<Item = std::result::Result<Bytes, Infallible>> {
    async_stream::stream! {
        let mut guard = LocalRunGuard::new(handle, receiver);
        let mut trace = TraceStreamSink::new(trace, "byok_server");
        while let Some(chunk) = guard.receiver_mut().recv().await {
            let terminal = is_end_stream_frame(&chunk);
            trace.chunk(&chunk);
            if terminal {
                guard.complete();
                trace.finish(end_stream_error(&chunk));
            }
            yield Ok::<Bytes, Infallible>(chunk);
            if terminal {
                return;
            }
        }
        guard.complete();
        trace.finish(None);
    }
}

fn is_end_stream_frame(frame: &Bytes) -> bool {
    frame
        .first()
        .is_some_and(|flags| flags & END_STREAM_FLAG != 0)
}

fn end_stream_error(frame: &Bytes) -> Option<String> {
    connect::decode_frames(frame)
        .ok()?
        .into_iter()
        .find_map(|(flags, payload)| {
            if flags & END_STREAM_FLAG == 0 {
                return None;
            }
            let value = serde_json::from_slice::<serde_json::Value>(&payload).ok()?;
            let error = value.get("error")?;
            let code = error.get("code").and_then(serde_json::Value::as_str);
            let message = error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .filter(|message| !message.is_empty());
            Some(match (code, message) {
                (Some(code), Some(message)) => format!("{code}: {message}"),
                (Some(code), None) => code.to_string(),
                (None, Some(message)) => message.to_string(),
                (None, None) => error.to_string(),
            })
        })
}

struct LocalRunGuard {
    handle: TransportHandle,
    receiver: Option<mpsc::UnboundedReceiver<Bytes>>,
    grace: Duration,
    completed: bool,
}

impl LocalRunGuard {
    fn new(handle: TransportHandle, receiver: mpsc::UnboundedReceiver<Bytes>) -> Self {
        Self {
            handle,
            receiver: Some(receiver),
            grace: RUN_DISCONNECT_GRACE,
            completed: false,
        }
    }

    #[cfg(test)]
    fn new_with_grace(
        handle: TransportHandle,
        receiver: mpsc::UnboundedReceiver<Bytes>,
        grace: Duration,
    ) -> Self {
        Self {
            handle,
            receiver: Some(receiver),
            grace,
            completed: false,
        }
    }

    fn receiver_mut(&mut self) -> &mut mpsc::UnboundedReceiver<Bytes> {
        self.receiver
            .as_mut()
            .expect("receiver is present until the guard is dropped")
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for LocalRunGuard {
    fn drop(&mut self) {
        // Drop our own subscription before the liveness check. The disconnecting
        // stream's receiver would otherwise make `has_live_subscribers` return
        // true forever and suppress the grace timer.
        drop(self.receiver.take());
        if !self.completed {
            self.handle.schedule_disconnect(self.grace);
        }
    }
}

pub async fn upstream(
    registry: TransportRegistry,
    request_id: String,
    generation: u64,
    response: Response<Body>,
    trace: Option<CursorTraceRecorder>,
) -> Response<Body> {
    let (parts, body) = response.into_parts();
    if let Some(trace) = &trace {
        trace.response_started(parts.status.as_u16());
    }
    let stream = async_stream::stream! {
        let _guard = UpstreamRunGuard {
            registry,
            request_id,
            generation,
        };
        let mut trace = TraceStreamSink::new(trace, "cursor_official");
        let mut body = body.into_data_stream();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(chunk) => {
                    trace.chunk(&chunk);
                    yield Ok::<Bytes, axum::Error>(chunk);
                }
                Err(error) => {
                    trace.finish(Some(error.to_string()));
                    yield Err(error);
                    return;
                }
            }
        }
        trace.finish(None);
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

enum TraceStreamEvent {
    Chunk(Bytes),
    Finish(Option<String>),
}

struct TraceStreamSink {
    sender: Option<mpsc::UnboundedSender<TraceStreamEvent>>,
}

impl TraceStreamSink {
    fn new(trace: Option<CursorTraceRecorder>, source: &'static str) -> Self {
        let Some(trace) = trace else {
            return Self { sender: None };
        };
        let (sender, mut receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                match event {
                    TraceStreamEvent::Chunk(chunk) => {
                        trace.response_chunk(source, chunk);
                    }
                    TraceStreamEvent::Finish(error) => {
                        trace.finish(error.as_deref());
                        return;
                    }
                }
            }
            trace.finish(None);
        });
        Self {
            sender: Some(sender),
        }
    }

    fn chunk(&self, chunk: &Bytes) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(TraceStreamEvent::Chunk(chunk.clone()));
        }
    }

    fn finish(&mut self, error: Option<String>) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(TraceStreamEvent::Finish(error));
        }
    }
}

impl Drop for TraceStreamSink {
    fn drop(&mut self) {
        if self.sender.is_some() {
            self.finish(Some(
                "response stream dropped before completion".to_string(),
            ));
        }
    }
}

struct UpstreamRunGuard {
    registry: TransportRegistry,
    request_id: String,
    generation: u64,
}

impl Drop for UpstreamRunGuard {
    fn drop(&mut self) {
        self.registry
            .finish_upstream(self.request_id.clone(), self.generation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cursor::{conversation::TransportCommand, services::observability::CursorTraceService},
        store::Store,
    };

    async fn test_handle() -> (
        TransportHandle,
        mpsc::Receiver<TransportCommand>,
        tempfile::TempDir,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("run_sse.db").display());
        let store = Store::connect(&url).await.unwrap();
        let trace = CursorTraceService::new(store).recorder("request");
        let (sender, receiver) = mpsc::channel(8);
        (
            TransportHandle::new(
                "request".into(),
                sender,
                std::sync::Arc::new(crate::cursor::transport::OutputHub::default()),
                trace,
            ),
            receiver,
            directory,
        )
    }

    #[tokio::test]
    async fn dropping_guard_releases_receiver_before_scheduling_disconnect() {
        let (handle, mut commands, _directory) = test_handle().await;
        let receiver = handle.subscribe();
        let guard = LocalRunGuard::new_with_grace(handle, receiver, Duration::from_millis(20));
        drop(guard);
        let command = tokio::time::timeout(Duration::from_millis(200), commands.recv())
            .await
            .expect("guard should schedule a grace disconnect");
        assert!(matches!(command, Some(TransportCommand::Disconnect)));
    }
}
