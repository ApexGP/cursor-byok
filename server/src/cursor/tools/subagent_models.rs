//! Remembers parent-selected subagent model variants until the child starts.
//!
//! Cursor's Task tool only sends a model string to the client. The selected
//! variant (context window, reasoning effort, fast mode) arrives separately in
//! `subagent_model_overrides`; keep it keyed by the Task call id so the child
//! run can apply it before the saved model defaults are configured.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::Mutex;

use crate::model::ModelSpec;

const ENTRY_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_ENTRIES: usize = 256;

#[derive(Clone, Debug, Default)]
pub struct SubagentModelRegistry {
    entries: Arc<Mutex<HashMap<String, Entry>>>,
}

#[derive(Debug)]
struct Entry {
    recorded_at: Instant,
    model: ModelSpec,
}

impl SubagentModelRegistry {
    pub fn remember(&self, tool_call_id: &str, model: ModelSpec) {
        if tool_call_id.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut entries = self.entries.lock();
        entries.retain(|_, entry| now.duration_since(entry.recorded_at) <= ENTRY_TTL);
        entries.insert(
            tool_call_id.to_string(),
            Entry {
                recorded_at: now,
                model,
            },
        );
        while entries.len() > MAX_ENTRIES {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.recorded_at)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            entries.remove(&oldest);
        }
    }

    /// Returns the variant for every run on the child transport, including
    /// follow-up messages that reuse the parent Task call id. A successful
    /// lookup refreshes the TTL so a long-running subagent keeps its variant.
    pub fn get(&self, tool_call_id: &str) -> Option<ModelSpec> {
        let now = Instant::now();
        let mut entries = self.entries.lock();
        entries.retain(|_, entry| now.duration_since(entry.recorded_at) <= ENTRY_TTL);
        let entry = entries.get_mut(tool_call_id)?;
        entry.recorded_at = now;
        Some(entry.model.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(context_window_tokens: u64) -> ModelSpec {
        let mut model = ModelSpec::new("model-hash");
        model.context_window_tokens = Some(context_window_tokens);
        model
    }

    #[test]
    fn returns_a_remembered_variant_for_follow_up_runs() {
        let registry = SubagentModelRegistry::default();
        registry.remember("call-1", model(1_000_000));

        let first = registry.get("call-1").expect("variant is registered");
        let second = registry.get("call-1").expect("variant stays registered");
        assert_eq!(first.context_window_tokens, Some(1_000_000));
        assert_eq!(second.context_window_tokens, Some(1_000_000));
    }

    #[test]
    fn ignores_empty_call_ids() {
        let registry = SubagentModelRegistry::default();
        registry.remember("", model(1_000_000));
        assert!(registry.get("").is_none());
    }
}
