//! Provides the request-scoped input, subscription, and terminal interface.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    cursor::{
        conversation::TransportCommand,
        protocol::{connect, proto::agent::v1 as pb},
        services::observability::CursorTraceRecorder,
    },
    Error, Result,
};

use super::{OutputHub, TransportAdmission, TransportLifecycle};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportParent {
    pub request_id: String,
    pub tool_call_id: String,
}

#[derive(Clone)]
pub struct TransportHandle {
    request_id: String,
    commands: mpsc::Sender<TransportCommand>,
    output: Arc<OutputHub>,
    conversation_id: Arc<OnceLock<String>>,
    parent: Arc<OnceLock<TransportParent>>,
    trace: CursorTraceRecorder,
    lifecycle: TransportLifecycle,
    disconnect: CancellationToken,
    pending_disconnect: Arc<parking_lot::Mutex<Option<(u64, CancellationToken)>>>,
    disconnect_generation: Arc<AtomicU64>,
}

impl TransportHandle {
    pub(crate) fn new(
        request_id: String,
        commands: mpsc::Sender<TransportCommand>,
        output: Arc<OutputHub>,
        trace: CursorTraceRecorder,
    ) -> Self {
        Self {
            request_id,
            commands,
            output,
            conversation_id: Arc::new(OnceLock::new()),
            parent: Arc::new(OnceLock::new()),
            trace,
            lifecycle: TransportLifecycle::new(),
            disconnect: CancellationToken::new(),
            pending_disconnect: Arc::new(parking_lot::Mutex::new(None)),
            disconnect_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn set_conversation_id(&self, conversation_id: &str) -> Result<()> {
        if conversation_id.is_empty() {
            return Err(Error::Protocol("Cursor conversation id is required".into()));
        }
        if self
            .conversation_id
            .get()
            .is_some_and(|current| current != conversation_id)
        {
            return Err(Error::Protocol(format!(
                "conflicting conversation ids for request {}",
                self.request_id
            )));
        }
        let _ = self.conversation_id.set(conversation_id.into());
        Ok(())
    }

    pub fn conversation_id(&self) -> Option<&str> {
        self.conversation_id.get().map(String::as_str)
    }

    pub fn set_parent(&self, parent: TransportParent) -> Result<()> {
        if parent.request_id.is_empty() || parent.tool_call_id.is_empty() {
            return Err(Error::Protocol("Cursor parent ids are required".into()));
        }
        if self.parent.get().is_some_and(|current| current != &parent) {
            return Err(Error::Protocol(format!(
                "conflicting parent ids for request {}",
                self.request_id
            )));
        }
        let _ = self.parent.set(parent);
        Ok(())
    }

    pub fn parent(&self) -> Option<&TransportParent> {
        self.parent.get()
    }

    pub async fn command(&self, command: TransportCommand) -> Result<()> {
        self.commands
            .send(command)
            .await
            .map_err(|_| Error::RunNotFound(self.request_id.clone()))
    }

    pub async fn disconnect(&self) {
        let _ = self.commands.send(TransportCommand::Disconnect).await;
    }

    /// Delays a client disconnect until `grace` elapses. A reconnect calls
    /// `subscribe`, which cancels the pending disconnect; OutputHub replays the
    /// full output history to the new subscriber.
    pub fn schedule_disconnect(&self, grace: Duration) {
        let generation = self
            .disconnect_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let token = CancellationToken::new();
        {
            let mut pending = self.pending_disconnect.lock();
            if let Some((_, previous)) = pending.take() {
                previous.cancel();
            }
            // A new stream may already be live before the old body's Drop runs.
            // Never arm a timer while any subscriber is still attached.
            if self.output.has_live_subscribers() {
                return;
            }
            *pending = Some((generation, token.clone()));
        }
        let handle = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = token.cancelled() => {}
                _ = tokio::time::sleep(grace) => {
                    let should_disconnect = {
                        let mut pending = handle.pending_disconnect.lock();
                        if pending
                            .as_ref()
                            .is_some_and(|(current, _)| *current == generation)
                        {
                            pending.take();
                            !handle.output.has_live_subscribers()
                        } else {
                            false
                        }
                    };
                    if should_disconnect {
                        handle.disconnect().await;
                    }
                }
            }
        });
    }

    pub fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<Bytes> {
        // Hold the pending-disconnect lock across subscription so the grace
        // timer cannot fire between cancelling it and registering the new
        // subscriber.
        let mut pending = self.pending_disconnect.lock();
        if let Some((_, token)) = pending.take() {
            token.cancel();
        }
        let receiver = self.output.subscribe();
        drop(pending);
        receiver
    }

    pub fn emit_frame(&self, frame: Bytes) -> bool {
        self.output.emit(frame)
    }

    pub fn emit(&self, message: &pb::AgentServerMessage) -> Result<()> {
        if self.emit_frame(connect::encode_message(message)?) {
            Ok(())
        } else {
            Err(Error::RunNotFound(self.request_id.clone()))
        }
    }

    pub(crate) fn close_output(&self) -> bool {
        self.output.close()
    }

    pub(crate) fn trace(&self) -> Option<&CursorTraceRecorder> {
        Some(&self.trace)
    }

    pub(crate) fn accepting_appends(&self) -> bool {
        self.lifecycle.is_open()
    }

    pub(crate) fn admit(&self) -> Result<TransportAdmission> {
        self.lifecycle
            .admit()
            .ok_or_else(|| Error::RunNotFound(self.request_id.clone()))
    }

    pub(crate) fn begin_close(&self) {
        self.lifecycle.begin_close();
    }

    pub(crate) fn admissions_drained(&self) -> bool {
        self.lifecycle.admissions_drained()
    }

    pub(crate) async fn wait_admissions_drained(&self) {
        self.lifecycle.wait_admissions_drained().await;
    }

    pub(crate) fn mark_draining(&self) {
        self.lifecycle.mark_draining();
    }

    pub(crate) fn reopen(&self) {
        self.lifecycle.reopen();
    }

    pub(crate) fn close_transport(&self) {
        self.lifecycle.close();
    }

    pub(crate) async fn wait_transport_closed(&self) {
        self.lifecycle.wait_closed().await;
    }

    pub(crate) fn disconnect_token(&self) -> CancellationToken {
        self.disconnect.clone()
    }

    pub(crate) fn mark_disconnected(&self) {
        self.disconnect.cancel();
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tokio::sync::mpsc;

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
        let url = format!("sqlite://{}", directory.path().join("handle.db").display());
        let store = Store::connect(&url).await.unwrap();
        let trace = CursorTraceService::new(store).recorder("request");
        let (sender, receiver) = mpsc::channel(8);
        (
            TransportHandle::new(
                "request".into(),
                sender,
                Arc::new(OutputHub::default()),
                trace,
            ),
            receiver,
            directory,
        )
    }

    #[tokio::test]
    async fn reconnect_cancels_pending_disconnect() {
        let (handle, mut commands, _directory) = test_handle().await;
        handle.schedule_disconnect(Duration::from_millis(30));
        let _subscriber = handle.subscribe();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), commands.recv())
                .await
                .is_err(),
            "reconnect should cancel the grace disconnect"
        );
    }

    #[tokio::test]
    async fn stale_stream_drop_after_reconnect_does_not_disconnect() {
        let (handle, mut commands, _directory) = test_handle().await;
        let _subscriber = handle.subscribe();
        // The replacement stream subscribed first; the old body's Drop runs
        // later and must not cancel the live Run.
        handle.schedule_disconnect(Duration::from_millis(20));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), commands.recv())
                .await
                .is_err(),
            "a live subscriber must suppress the stale disconnect"
        );
    }

    #[tokio::test]
    async fn grace_expiry_disconnects_without_reconnect() {
        let (handle, mut commands, _directory) = test_handle().await;
        handle.schedule_disconnect(Duration::from_millis(20));
        let command = tokio::time::timeout(Duration::from_millis(200), commands.recv())
            .await
            .expect("disconnect should arrive");
        assert!(matches!(command, Some(TransportCommand::Disconnect)));
    }
}
