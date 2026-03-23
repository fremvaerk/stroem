//! Agent conversation state for multi-turn dispatch.
//!
//! Persisted as JSONB in the `agent_state` column of `job_step`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Conversation state for a multi-turn agent step.
///
/// Serialized to JSON and stored in `job_step.agent_state`. Restored when
/// resuming after async tool calls (task tools, ask_user).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConversationState {
    /// Chat history in rig `Message` format (serialized as JSON values).
    pub messages: Vec<serde_json::Value>,
    /// Current conversation turn (starts at 1).
    pub turn: u32,
    /// Cumulative input tokens across all turns.
    pub total_input_tokens: u64,
    /// Cumulative output tokens across all turns.
    pub total_output_tokens: u64,
    /// Task-tool child jobs still pending completion.
    #[serde(default)]
    pub pending_tool_calls: Vec<PendingToolCall>,
    /// True when the step is suspended waiting for ask_user response.
    #[serde(default)]
    pub suspended_for_ask_user: bool,
    /// Details of the ask_user call (present when `suspended_for_ask_user` is true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_user_call: Option<AskUserCall>,
    /// Tool results collected from completed child jobs, populated by the server on
    /// `agent_tool` child job completion. The worker drains these when resuming.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolved_tool_results: Vec<ResolvedToolResult>,
}

/// A task-tool call that created a child job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingToolCall {
    /// The tool_call_id from the LLM response.
    pub tool_call_id: String,
    /// The tool name (e.g. `strom_task_deploy`).
    pub tool_name: String,
    /// The child job ID created for this tool call.
    pub child_job_id: Uuid,
}

/// Details of an ask_user suspension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskUserCall {
    /// The tool_call_id from the LLM response.
    pub tool_call_id: String,
    /// The message to show the user.
    pub message: String,
}

/// A resolved tool result from a completed child job.
///
/// Populated by the server in `propagate_to_parent` when an `agent_tool` child job
/// completes. The worker reads these out of `resolved_tool_results` when resuming
/// the dispatch loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedToolResult {
    /// The tool_call_id from the original LLM tool call.
    pub tool_call_id: String,
    /// The result text to feed back to the LLM.
    pub result_text: String,
}

impl Default for AgentConversationState {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentConversationState {
    /// Create a new conversation state for the first turn.
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            turn: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            pending_tool_calls: Vec::new(),
            suspended_for_ask_user: false,
            ask_user_call: None,
            resolved_tool_results: Vec::new(),
        }
    }

    /// Check if all pending task-tool calls have been resolved.
    pub fn all_tool_calls_resolved(&self) -> bool {
        self.pending_tool_calls.is_empty()
    }

    /// Remove a pending tool call by child_job_id, returning the removed entry.
    pub fn resolve_tool_call(&mut self, child_job_id: Uuid) -> Option<PendingToolCall> {
        if let Some(pos) = self
            .pending_tool_calls
            .iter()
            .position(|tc| tc.child_job_id == child_job_id)
        {
            Some(self.pending_tool_calls.remove(pos))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_defaults() {
        let state = AgentConversationState::new();
        assert_eq!(state.turn, 0);
        assert!(state.messages.is_empty());
        assert_eq!(state.total_input_tokens, 0);
        assert_eq!(state.total_output_tokens, 0);
        assert!(state.pending_tool_calls.is_empty());
        assert!(!state.suspended_for_ask_user);
        assert!(state.ask_user_call.is_none());
    }

    #[test]
    fn test_default_delegates_to_new() {
        let state = AgentConversationState::default();
        assert_eq!(state.turn, 0);
        assert!(state.pending_tool_calls.is_empty());
    }

    #[test]
    fn test_all_tool_calls_resolved_empty() {
        let state = AgentConversationState::new();
        assert!(state.all_tool_calls_resolved());
    }

    #[test]
    fn test_all_tool_calls_resolved_with_pending() {
        let mut state = AgentConversationState::new();
        state.pending_tool_calls.push(PendingToolCall {
            tool_call_id: "tc1".to_string(),
            tool_name: "strom_task_deploy".to_string(),
            child_job_id: Uuid::new_v4(),
        });
        assert!(!state.all_tool_calls_resolved());
    }

    #[test]
    fn test_resolve_tool_call_found() {
        let mut state = AgentConversationState::new();
        let job_id = Uuid::new_v4();
        state.pending_tool_calls.push(PendingToolCall {
            tool_call_id: "tc1".to_string(),
            tool_name: "strom_task_deploy".to_string(),
            child_job_id: job_id,
        });
        let resolved = state.resolve_tool_call(job_id);
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().tool_call_id, "tc1");
        assert!(state.pending_tool_calls.is_empty());
    }

    #[test]
    fn test_resolve_tool_call_not_found() {
        let mut state = AgentConversationState::new();
        let job_id = Uuid::new_v4();
        state.pending_tool_calls.push(PendingToolCall {
            tool_call_id: "tc1".to_string(),
            tool_name: "strom_task_deploy".to_string(),
            child_job_id: job_id,
        });
        let other_id = Uuid::new_v4();
        let resolved = state.resolve_tool_call(other_id);
        assert!(resolved.is_none());
        assert_eq!(state.pending_tool_calls.len(), 1);
    }

    #[test]
    fn test_resolve_tool_call_removes_only_target() {
        let mut state = AgentConversationState::new();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        state.pending_tool_calls.push(PendingToolCall {
            tool_call_id: "tc1".to_string(),
            tool_name: "strom_task_a".to_string(),
            child_job_id: id1,
        });
        state.pending_tool_calls.push(PendingToolCall {
            tool_call_id: "tc2".to_string(),
            tool_name: "strom_task_b".to_string(),
            child_job_id: id2,
        });

        let resolved = state.resolve_tool_call(id1);
        assert!(resolved.is_some());
        assert_eq!(state.pending_tool_calls.len(), 1);
        assert_eq!(state.pending_tool_calls[0].child_job_id, id2);
    }

    #[test]
    fn test_resolved_tool_results_round_trip() {
        let mut state = AgentConversationState::new();
        state.resolved_tool_results.push(ResolvedToolResult {
            tool_call_id: "tc1".to_string(),
            result_text: "success".to_string(),
        });

        let json = serde_json::to_value(&state).unwrap();
        assert!(json.get("resolved_tool_results").is_some());
        let results = json["resolved_tool_results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["tool_call_id"], "tc1");
        assert_eq!(results[0]["result_text"], "success");

        let restored: AgentConversationState = serde_json::from_value(json).unwrap();
        assert_eq!(restored.resolved_tool_results.len(), 1);
    }

    #[test]
    fn test_resolved_tool_results_empty_omitted() {
        let state = AgentConversationState::new();
        let json = serde_json::to_value(&state).unwrap();
        assert!(
            json.get("resolved_tool_results").is_none(),
            "empty resolved_tool_results should be omitted from JSON"
        );
    }

    #[test]
    fn test_resolved_tool_results_backward_compat() {
        // Old JSON without resolved_tool_results should deserialize fine
        let json = serde_json::json!({
            "messages": [],
            "turn": 2,
            "total_input_tokens": 100,
            "total_output_tokens": 50,
            "pending_tool_calls": [],
            "suspended_for_ask_user": false
        });
        let state: AgentConversationState = serde_json::from_value(json).unwrap();
        assert!(state.resolved_tool_results.is_empty());
    }

    #[test]
    fn test_json_round_trip() {
        let mut state = AgentConversationState::new();
        state.turn = 3;
        state.total_input_tokens = 1000;
        state.total_output_tokens = 500;
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": "hello"}));
        state.suspended_for_ask_user = true;
        state.ask_user_call = Some(AskUserCall {
            tool_call_id: "tc_ask".to_string(),
            message: "What do you think?".to_string(),
        });
        state.pending_tool_calls.push(PendingToolCall {
            tool_call_id: "tc1".to_string(),
            tool_name: "strom_task_deploy".to_string(),
            child_job_id: Uuid::new_v4(),
        });

        let json = serde_json::to_value(&state).unwrap();
        let restored: AgentConversationState = serde_json::from_value(json).unwrap();

        assert_eq!(restored.turn, 3);
        assert_eq!(restored.total_input_tokens, 1000);
        assert_eq!(restored.total_output_tokens, 500);
        assert_eq!(restored.messages.len(), 1);
        assert!(restored.suspended_for_ask_user);
        assert_eq!(
            restored.ask_user_call.as_ref().unwrap().tool_call_id,
            "tc_ask"
        );
        assert_eq!(restored.pending_tool_calls.len(), 1);
    }
}
