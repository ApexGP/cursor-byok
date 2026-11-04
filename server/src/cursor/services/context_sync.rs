//! Hydrates request context blobs supplied by Cursor.
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use parking_lot::Mutex;
use prost::Message;
use tokio::sync::oneshot;

use crate::{
    cursor::{protocol::proto::agent::v1 as pb, transport::TransportHandle},
    store::{BlobId, Store},
    Error, Result,
};

type ContextSender = oneshot::Sender<Result<pb::RequestContext>>;

struct PendingContext {
    id: u64,
    sender: ContextSender,
}

struct PendingGuard {
    pending: Arc<Mutex<Option<PendingContext>>>,
    id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let mut pending = self.pending.lock();
        if pending
            .as_ref()
            .is_some_and(|current| current.id == self.id)
        {
            *pending = None;
        }
    }
}

#[derive(Clone)]
pub(crate) struct RequestContextSynchronizer {
    handle: TransportHandle,
    store: Store,
    pending: Arc<Mutex<Option<PendingContext>>>,
    next_id: Arc<AtomicU64>,
}

impl RequestContextSynchronizer {
    pub(crate) fn new(handle: TransportHandle, store: Store) -> Self {
        Self {
            handle,
            store,
            pending: Arc::new(Mutex::new(None)),
            next_id: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) async fn refresh_if_missing(
        &self,
        references: &pb::RequestContextPartReferences,
        conversation_id: &str,
    ) -> Result<Option<pb::RequestContext>> {
        if !self.has_missing_part(references).await? {
            return Ok(None);
        }
        let context = self.load(conversation_id).await?;
        self.cache_parts(&context).await?;
        Ok(Some(context))
    }

    pub(crate) async fn get(&self, id: &BlobId) -> Result<Option<Vec<u8>>> {
        self.store.get_blob(id).await
    }

    pub(crate) async fn load(&self, conversation_id: &str) -> Result<pb::RequestContext> {
        let (sender, receiver) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut pending = self.pending.lock();
            if pending.is_some() {
                return Err(Error::Protocol(
                    "Cursor request context is already being loaded".into(),
                ));
            }
            *pending = Some(PendingContext { id, sender });
        }
        // The prepare future can be dropped when Cursor switches models or
        // supersedes a run. A Drop guard clears only this load's slot so the
        // next run is not rejected with "already being loaded".
        let _guard = PendingGuard {
            pending: self.pending.clone(),
            id,
        };

        tracing::info!(
            request_id = self.handle.request_id(),
            conversation_id,
            "requesting uncached Cursor context"
        );

        self.handle.emit(&pb::AgentServerMessage {
            ttft_breakdown: None,
            message: Some(pb::agent_server_message::Message::ExecServerMessage(
                pb::ExecServerMessage {
                    id: 0,
                    message: Some(pb::exec_server_message::Message::RequestContextArgs(
                        pb::RequestContextArgs {
                            notes_session_id: Some(conversation_id.into()),
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                },
            )),
        })?;

        let cancellation = self.handle.disconnect_token();
        tokio::select! {
            result = receiver => result.map_err(|_| Error::Protocol("request context response channel closed".into()))?,
            _ = cancellation.cancelled() => Err(Error::Cancelled),
            _ = tokio::time::sleep(Duration::from_secs(60)) => Err(Error::Protocol("request context timed out".into())),
        }
    }

    pub(crate) async fn handle_client(&self, message: &pb::ExecClientMessage) -> bool {
        if message.id != 0 {
            return false;
        }
        let Some(pb::exec_client_message::Message::RequestContextResult(result)) =
            message.message.as_ref()
        else {
            return false;
        };
        let Some(pending) = self.pending.lock().take() else {
            tracing::warn!(
                request_id = self.handle.request_id(),
                "unexpected Cursor request context result"
            );
            return true;
        };
        let sender = pending.sender;
        use pb::request_context_result::Result as ContextResult;
        let result = match result.result.as_ref() {
            Some(ContextResult::Success(success)) => success
                .request_context
                .clone()
                .ok_or_else(|| Error::Protocol("Cursor returned empty request context".into())),
            Some(ContextResult::Error(error)) => Err(Error::Protocol(format!(
                "Cursor request context failed: {}",
                error.error
            ))),
            Some(ContextResult::Rejected(rejected)) => Err(Error::Protocol(format!(
                "Cursor rejected request context: {}",
                rejected.reason
            ))),
            None => Err(Error::Protocol(
                "Cursor returned no request context result".into(),
            )),
        };
        let _ = sender.send(result);
        true
    }

    pub(crate) async fn handle_stream_close(&self, id: u32) -> bool {
        id == 0 && self.pending.lock().is_some()
    }

    pub(crate) async fn handle_throw(&self, id: u32, message: String) -> bool {
        let sender = if id == 0 {
            self.pending.lock().take().map(|pending| pending.sender)
        } else {
            None
        };
        let Some(sender) = sender else { return false };
        let _ = sender.send(Err(Error::Protocol(message)));
        true
    }

    async fn has_missing_part(&self, parts: &pb::RequestContextPartReferences) -> Result<bool> {
        for raw_id in [
            parts.rules_blob_id.as_slice(),
            parts.skills_blob_id.as_slice(),
            parts.subagents_blob_id.as_slice(),
            parts.mcps_blob_id.as_slice(),
        ] {
            if raw_id.is_empty() {
                continue;
            }
            let id = BlobId::from_bytes(raw_id)?;
            if self.store.get_blob(&id).await?.is_none() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn cache_parts(&self, context: &pb::RequestContext) -> Result<()> {
        self.cache_part(&pb::RequestContextRulesPart {
            rules: context.rules.clone(),
            non_file_rules: context.non_file_rules.clone(),
            cloud_rule: context.cloud_rule.clone(),
        })
        .await?;
        self.cache_part(&pb::RequestContextSkillsPart {
            agent_skills: context.agent_skills.clone(),
            skill_options: context.skill_options.clone(),
        })
        .await?;
        self.cache_part(&pb::RequestContextSubagentsPart {
            custom_subagents: context.custom_subagents.clone(),
        })
        .await?;
        self.cache_part(&pb::RequestContextMcpsPart {
            tools: context.tools.clone(),
            mcp_instructions: context.mcp_instructions.clone(),
            mcp_file_system_options: context.mcp_file_system_options.clone(),
            mcp_meta_tool_options: context.mcp_meta_tool_options.clone(),
        })
        .await
    }

    async fn cache_part<T: Message>(&self, part: &T) -> Result<()> {
        let data = part.encode_to_vec();
        self.store.put_blob(&data, &[]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        cursor::{
            services::observability::CursorTraceService,
            transport::{OutputHub, TransportHandle},
        },
        store::Store,
    };

    async fn setup() -> (
        RequestContextSynchronizer,
        TransportHandle,
        tempfile::TempDir,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}",
            directory.path().join("context-sync.db").display()
        );
        let store = Store::connect(&url).await.unwrap();
        let trace = CursorTraceService::new(store.clone()).recorder("request");
        let (commands, _receiver) = mpsc::channel(8);
        let handle = TransportHandle::new(
            "request".into(),
            commands,
            Arc::new(OutputHub::default()),
            trace,
        );
        (
            RequestContextSynchronizer::new(handle.clone(), store),
            handle,
            directory,
        )
    }

    #[tokio::test]
    async fn dropping_a_load_does_not_poison_the_next_load() {
        let (sync, handle, _directory) = setup().await;
        let first = tokio::spawn({
            let sync = sync.clone();
            async move { sync.load("conversation").await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        first.abort();
        let _ = first.await;

        let second = sync.load("conversation");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), second)
                .await
                .is_err(),
            "second load must wait for Cursor instead of being rejected"
        );
        handle.mark_disconnected();
    }
}
