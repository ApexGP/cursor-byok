//! Tracks running Tool executions and coordinates cancellation and cleanup.
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};

use tokio::sync::Mutex;

use crate::{
    cursor::protocol::proto::agent::v1 as pb,
    model::{ModelSpec, ToolCall},
    Error, Result,
};

use super::{edit::EditWrite, subagent_models::SubagentModelRegistry};

#[derive(Clone, Default)]
pub struct CursorToolRuntime {
    next_id: Arc<AtomicU32>,
    execs: Arc<Mutex<HashMap<u32, PendingExec>>>,
    interactions: Arc<Mutex<HashMap<u32, PendingInteraction>>>,
    completed: Arc<Mutex<HashMap<u32, String>>>,
    interrupted: Arc<Mutex<HashSet<u32>>>,
}

pub(crate) struct PendingExec {
    pub call: ToolCall,
    pub context: ExecContext,
    pub started_at_ms: u64,
    pub stdout: String,
    pub stderr: String,
    pub stage: ExecStage,
}

pub(crate) enum ExecStage {
    Direct,
    DynamicMcp(pb::McpToolDefinition),
    EditRead,
    EditWrite(EditWrite),
}

#[derive(Clone, Debug, Default)]
pub struct ExecContext {
    pub conversation_id: String,
    pub root_conversation_id: String,
    pub default_subagent_model: String,
    pub subagent_model: Option<SubagentModel>,
    pub subagent_models: SubagentModelRegistry,
    pub allow_subagents: bool,
    pub subagents_disabled: bool,
    pub terminals_folder: String,
    pub admin_command_denylist: Vec<String>,
    pub mcp_routes: HashMap<(String, String), McpRoute>,
}

#[derive(Clone, Debug)]
pub struct McpRoute {
    pub name: String,
    pub provider_identifier: String,
    pub tool_name: String,
    pub description: String,
}

#[derive(Clone, Debug)]
pub enum SubagentModel {
    Model(ModelSpec),
    Disabled,
}

impl ExecContext {
    pub fn task_disabled(&self, call: &ToolCall) -> bool {
        if !call.name.eq_ignore_ascii_case("Task") {
            return false;
        }
        self.subagents_disabled || matches!(self.subagent_model, Some(SubagentModel::Disabled))
    }

    pub fn prepare_call(&self, call: &ToolCall) -> Result<ToolCall> {
        if !call.name.eq_ignore_ascii_case("Task") {
            return Ok(call.clone());
        }
        let arguments = call
            .arguments
            .as_object()
            .ok_or_else(|| Error::Protocol("Task arguments must be a JSON object".into()))?;
        let subagent_type = arguments
            .get("subagent_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("generalPurpose");
        if self.task_disabled(call) {
            return Ok(call.clone());
        }
        let model = match &self.subagent_model {
            Some(SubagentModel::Model(model)) => model.model_id.clone(),
            Some(SubagentModel::Disabled) => unreachable!("disabled Task returned above"),
            None => arguments
                .get("model")
                .and_then(serde_json::Value::as_str)
                .filter(|model| *model != "inherit")
                .unwrap_or(&self.default_subagent_model)
                .to_string(),
        };
        if model.is_empty() {
            return Err(Error::Protocol(format!(
                "Task subagent type {subagent_type} has no model"
            )));
        }
        let mut prepared = call.clone();
        prepared
            .arguments
            .as_object_mut()
            .expect("Task arguments were validated")
            .insert("model".into(), serde_json::Value::String(model));
        if let Some(SubagentModel::Model(selection)) = &self.subagent_model {
            // The client only receives the model string; keep the selected
            // context/effort/fast variant for the child run keyed by this Task
            // call id. Without this the child falls back to saved defaults.
            self.subagent_models
                .remember(&call.call_id, selection.clone());
        }
        Ok(prepared)
    }
}

pub(crate) struct PendingInteraction {
    pub call: ToolCall,
    pub started_at_ms: u64,
}

impl CursorToolRuntime {
    pub(crate) fn next_run(&self) -> Self {
        Self {
            next_id: self.next_id.clone(),
            execs: Arc::new(Mutex::new(HashMap::new())),
            interactions: Arc::new(Mutex::new(HashMap::new())),
            completed: Arc::new(Mutex::new(HashMap::new())),
            interrupted: self.interrupted.clone(),
        }
    }

    pub async fn reserve_exec(&self, call: &ToolCall, context: &ExecContext) -> Result<u32> {
        self.reserve_exec_stage(call, context, ExecStage::Direct, None)
            .await
    }

    pub(crate) async fn reserve_dynamic_mcp(
        &self,
        call: &ToolCall,
        context: &ExecContext,
        definition: &pb::McpToolDefinition,
    ) -> Result<u32> {
        self.reserve_exec_stage(
            call,
            context,
            ExecStage::DynamicMcp(definition.clone()),
            None,
        )
        .await
    }

    pub(crate) async fn reserve_edit_read(
        &self,
        call: &ToolCall,
        context: &ExecContext,
    ) -> Result<u32> {
        self.reserve_exec_stage(call, context, ExecStage::EditRead, None)
            .await
    }

    pub(crate) async fn reserve_edit_write(
        &self,
        call: &ToolCall,
        context: &ExecContext,
        write: EditWrite,
        started_at_ms: u64,
    ) -> Result<u32> {
        self.reserve_exec_stage(
            call,
            context,
            ExecStage::EditWrite(write),
            Some(started_at_ms),
        )
        .await
    }

    async fn reserve_exec_stage(
        &self,
        call: &ToolCall,
        context: &ExecContext,
        stage: ExecStage,
        started_at_ms: Option<u64>,
    ) -> Result<u32> {
        let id = self.next_id()?;
        self.execs.lock().await.insert(
            id,
            PendingExec {
                call: call.clone(),
                context: context.clone(),
                started_at_ms: started_at_ms.unwrap_or_else(now_ms),
                stdout: String::new(),
                stderr: String::new(),
                stage,
            },
        );
        Ok(id)
    }

    pub async fn reserve_interaction(&self, call: &ToolCall) -> Result<u32> {
        let id = self.next_id()?;
        self.interactions.lock().await.insert(
            id,
            PendingInteraction {
                call: call.clone(),
                started_at_ms: now_ms(),
            },
        );
        Ok(id)
    }

    pub async fn exec_call(&self, id: u32) -> Option<ToolCall> {
        self.execs
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.call.clone())
    }

    pub async fn append_stdout(&self, id: u32, data: &str) -> bool {
        let mut entries = self.execs.lock().await;
        let Some(entry) = entries.get_mut(&id) else {
            return false;
        };
        entry.stdout.push_str(data);
        true
    }

    pub async fn append_stderr(&self, id: u32, data: &str) -> bool {
        let mut entries = self.execs.lock().await;
        let Some(entry) = entries.get_mut(&id) else {
            return false;
        };
        entry.stderr.push_str(data);
        true
    }

    pub(crate) async fn take_exec(&self, id: u32) -> Option<PendingExec> {
        let pending = self.execs.lock().await.remove(&id);
        if let Some(pending) = &pending {
            self.completed
                .lock()
                .await
                .insert(id, pending.call.call_id.clone());
        }
        pending
    }

    pub(crate) async fn take_interaction(&self, id: u32) -> Option<PendingInteraction> {
        let pending = self.interactions.lock().await.remove(&id);
        if let Some(pending) = &pending {
            self.completed
                .lock()
                .await
                .insert(id, pending.call.call_id.clone());
        }
        pending
    }

    pub async fn completed_call(&self, id: u32) -> Option<String> {
        self.completed.lock().await.get(&id).cloned()
    }

    pub async fn is_interrupted(&self, id: u32) -> bool {
        self.interrupted.lock().await.contains(&id)
    }

    pub async fn clear_completed(&self) {
        self.completed.lock().await.clear();
    }

    pub async fn discard_exec(&self, id: u32) {
        self.execs.lock().await.remove(&id);
    }

    pub async fn discard_interaction(&self, id: u32) {
        self.interactions.lock().await.remove(&id);
    }

    pub async fn drain_running(&self) -> Vec<u32> {
        let mut entries = self.execs.lock().await;
        let mut ids = entries.drain().map(|(id, _)| id).collect::<Vec<_>>();
        ids.sort_unstable();
        self.interactions.lock().await.clear();
        self.completed.lock().await.clear();
        self.interrupted.lock().await.clear();
        ids
    }

    pub async fn interrupt_for_run_replacement(&self) -> Vec<u32> {
        let mut execs = self.execs.lock().await;
        let mut abort_ids = execs.keys().copied().collect::<Vec<_>>();
        let mut interrupted_ids = abort_ids.clone();
        execs.clear();
        drop(execs);

        let mut interactions = self.interactions.lock().await;
        interrupted_ids.extend(interactions.keys().copied());
        interactions.clear();
        drop(interactions);

        self.completed.lock().await.clear();
        self.interrupted.lock().await.extend(interrupted_ids);
        abort_ids.sort_unstable();
        abort_ids
    }

    pub async fn interrupt_for_message(&self) -> Vec<u32> {
        let (abort_ids, interrupted_ids) = {
            let mut entries = self.execs.lock().await;
            let mut abort_ids = Vec::new();
            let mut interrupted_ids = Vec::new();
            entries.retain(|id, entry| {
                interrupted_ids.push(*id);
                let keep_running = entry.call.name.eq_ignore_ascii_case("Task");
                if !keep_running {
                    abort_ids.push(*id);
                }
                keep_running
            });
            (abort_ids, interrupted_ids)
        };
        let interaction_ids = {
            let mut interactions = self.interactions.lock().await;
            let ids = interactions.keys().copied().collect::<Vec<_>>();
            interactions.clear();
            ids
        };
        let mut interrupted = self.interrupted.lock().await;
        interrupted.extend(interrupted_ids);
        interrupted.extend(interaction_ids);
        let mut abort_ids = abort_ids;
        abort_ids.sort_unstable();
        abort_ids
    }

    pub async fn running_exec_ids(&self) -> Vec<u32> {
        let mut ids = self.execs.lock().await.keys().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    pub async fn running_task_exec_id(&self, call_id: &str) -> Option<u32> {
        self.execs
            .lock()
            .await
            .iter()
            .filter_map(|(id, entry)| {
                (entry.call.call_id == call_id && entry.call.name.eq_ignore_ascii_case("Task"))
                    .then_some(*id)
            })
            .min()
    }

    fn next_id(&self) -> Result<u32> {
        self.next_id
            .fetch_add(1, Ordering::Relaxed)
            .checked_add(1)
            .ok_or_else(|| Error::Protocol("Cursor message id space exhausted".into()))
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod subagent_model_tests {
    use std::collections::HashMap;

    use super::*;
    use crate::model::ModelLatency;

    fn context(subagent_model: Option<SubagentModel>) -> ExecContext {
        ExecContext {
            conversation_id: "conversation".into(),
            root_conversation_id: "conversation".into(),
            default_subagent_model: "parent-model".into(),
            subagent_model,
            subagent_models: Default::default(),
            allow_subagents: true,
            subagents_disabled: false,
            terminals_folder: String::new(),
            admin_command_denylist: Vec::new(),
            mcp_routes: HashMap::new(),
        }
    }

    fn task_call() -> ToolCall {
        ToolCall {
            index: 0,
            call_id: "call-1".into(),
            model_call_id: "model-call".into(),
            name: "Task".into(),
            arguments_text: String::new(),
            arguments: serde_json::json!({
                "prompt": "do work",
                "description": "work",
                "subagent_type": "generalPurpose",
            }),
            argument_error: None,
        }
    }

    #[test]
    fn task_call_remembers_the_selected_variant_for_the_child() {
        let mut selection = ModelSpec::new("model-hash");
        selection.context_window_tokens = Some(1_000_000);
        selection.reasoning.effort = Some("max".into());
        selection.reasoning.enabled = true;
        selection.latency = ModelLatency::Fast;
        let context = context(Some(SubagentModel::Model(selection)));
        let call = task_call();

        let prepared = context.prepare_call(&call).expect("prepare Task");

        assert_eq!(prepared.arguments["model"], "model-hash");
        let remembered = context
            .subagent_models
            .get("call-1")
            .expect("variant registered for the child");
        assert_eq!(remembered.context_window_tokens, Some(1_000_000));
        assert_eq!(remembered.reasoning.effort.as_deref(), Some("max"));
        assert_eq!(remembered.latency, ModelLatency::Fast);
    }
}
