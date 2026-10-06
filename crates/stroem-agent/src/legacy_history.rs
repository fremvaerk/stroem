//! Lifts agent conversation history persisted by rig-core 0.36.
//!
//! `AgentConversationState::messages` stores rig `Message`s as JSON in
//! `job_step.agent_state`. rig-core 0.43 changed that schema: assistant
//! content is tagged with `"type"`, a tool call's `id`/`call_id` strings
//! became one typed `CallId`, a tool result carries the call's `CallId` and
//! the tool's name instead of `id`/`call_id`, and reasoning is sealed to the
//! provider that issued it. A step suspended (task-tool children or
//! `ask_user`) by a 0.36-era worker would otherwise resume with its
//! assistant turns and tool results unreadable — the dispatch loop skips a
//! message that does not parse, and providers reject tool results whose call
//! is missing.
//!
//! [`upgrade`] rewrites such messages in place. Messages already in the
//! current schema are left untouched, so it is safe to run on every resume.

use rig_core::message::{CallId, LocalCallId};
use serde_json::{Map, Value};
use std::collections::HashMap;

/// Name given to a legacy tool result whose tool call is not in the history.
/// rig requires a non-empty name; the old schema never recorded one.
const UNKNOWN_TOOL_NAME: &str = "unknown_tool";

/// Rewrite every message of a rig-core 0.36 conversation into the current
/// schema. `reasoning_issuer` is the provider the history was produced with
/// (see [`crate::provider::reasoning_issuer`]); legacy reasoning is sealed
/// to it so the same provider keeps receiving it on replay.
pub fn upgrade(messages: &mut [Value], reasoning_issuer: &str) {
    // Legacy tool-call id → (current CallId JSON, tool name), for the results.
    let mut calls: HashMap<String, (Value, Value)> = HashMap::new();

    for message in messages.iter_mut() {
        if role(message) != Some("assistant") {
            continue;
        }
        for item in content_items(message) {
            if item.contains_key("type") {
                continue;
            }
            upgrade_assistant_item(item, reasoning_issuer, &mut calls);
        }
    }

    for message in messages.iter_mut() {
        if role(message) != Some("user") {
            continue;
        }
        for item in content_items(message) {
            let legacy_result = item.get("type").and_then(Value::as_str) == Some("toolresult")
                && !item.contains_key("call");
            if legacy_result {
                upgrade_tool_result(item, &calls);
            }
        }
    }
}

fn role(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

fn content_items(message: &mut Value) -> impl Iterator<Item = &mut Map<String, Value>> {
    message
        .get_mut("content")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object_mut)
}

fn non_empty_str(item: &Map<String, Value>, key: &str) -> Option<String> {
    item.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The current `CallId` for a legacy `(id, call_id)` pair. A distinct
/// `call_id` is the provider's correlator, with `id` as its item id (OpenAI
/// Responses' `fc_…`); otherwise the one id present is the provider's.
fn legacy_call_id(id: Option<String>, call_id: Option<String>) -> CallId {
    match (id, call_id) {
        (Some(id), Some(call_id)) if id != call_id => CallId::from_dual_wire(id, call_id),
        (_, Some(call_id)) => CallId::from_wire(call_id),
        (Some(id), None) => CallId::from_wire(id),
        (None, None) => CallId::Local(LocalCallId::new()),
    }
}

fn call_id_json(id: &CallId) -> Value {
    serde_json::to_value(id).expect("CallId serializes")
}

/// Untagged 0.36 `AssistantContent`: tell the variant by its fields.
fn upgrade_assistant_item(
    item: &mut Map<String, Value>,
    reasoning_issuer: &str,
    calls: &mut HashMap<String, (Value, Value)>,
) {
    if item.contains_key("function") {
        // ToolCall { id, call_id, function, signature, additional_params }
        let id = non_empty_str(item, "id");
        let call_id = non_empty_str(item, "call_id");
        let call = call_id_json(&legacy_call_id(id.clone(), call_id.clone()));
        let name = item
            .get("function")
            .and_then(|f| f.get("name"))
            .cloned()
            .unwrap_or(Value::Null);
        for key in [id, call_id].into_iter().flatten() {
            calls
                .entry(key)
                .or_insert_with(|| (call.clone(), name.clone()));
        }
        item.remove("call_id");
        item.insert("id".to_owned(), call);
        item.insert("type".to_owned(), Value::from("toolcall"));
    } else if item.get("content").is_some_and(Value::is_array) {
        // Reasoning { id, content: [ReasoningContent] } — the content blocks
        // kept their shape; the block is now sealed to its issuer.
        item.insert("issuer".to_owned(), Value::from(reasoning_issuer));
        item.insert("type".to_owned(), Value::from("reasoning"));
    } else if item.contains_key("data") {
        item.insert("type".to_owned(), Value::from("image"));
    } else if item.contains_key("text") {
        item.insert("type".to_owned(), Value::from("text"));
    }
}

/// 0.36 `ToolResult { id, call_id, content }` → `{ call, name, content }`.
fn upgrade_tool_result(item: &mut Map<String, Value>, calls: &HashMap<String, (Value, Value)>) {
    let id = non_empty_str(item, "id");
    let call_id = non_empty_str(item, "call_id");
    let known = [&id, &call_id]
        .into_iter()
        .flatten()
        .find_map(|key| calls.get(key))
        .cloned();
    let (call, name) = known.unwrap_or_else(|| {
        (
            call_id_json(&legacy_call_id(id, call_id)),
            Value::from(UNKNOWN_TOOL_NAME),
        )
    });
    item.remove("id");
    item.remove("call_id");
    item.insert("call".to_owned(), call);
    item.insert("name".to_owned(), name);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::{AssistantContent, Message};
    use rig_core::message::{ToolResultContent, UserContent};

    /// A conversation exactly as rig-core 0.36 serialized it (generated with
    /// rig-core 0.36.0 from the messages the dispatch loop built): prompt;
    /// assistant reasoning + text + tool call; its result; an OpenAI
    /// Responses-style dual-id `ask_user` call; its result.
    fn legacy_conversation() -> Vec<Value> {
        serde_json::from_str(
            r#"[
              {"content": [{"text": "Deploy it", "type": "text"}], "role": "user"},
              {"content": [
                 {"content": [{"content": {"signature": "sig", "text": "thinking..."}, "type": "text"}], "id": null},
                 {"text": "On it"},
                 {"additional_params": null, "call_id": null,
                  "function": {"arguments": {"env": "prod"}, "name": "strom_task_deploy"},
                  "id": "call_abc", "signature": null}
               ], "id": "msg_1", "role": "assistant"},
              {"content": [{"call_id": "call_abc", "content": [{"text": "{\"ok\":true}", "type": "text"}],
                            "id": "call_abc", "type": "toolresult"}], "role": "user"},
              {"content": [{"additional_params": null, "call_id": "call_xyz",
                            "function": {"arguments": {"message": "Sure?"}, "name": "ask_user"},
                            "id": "fc_123", "signature": null}], "id": null, "role": "assistant"},
              {"content": [{"call_id": "call_xyz", "content": [{"text": "yes", "type": "text"}],
                            "id": "fc_123", "type": "toolresult"}], "role": "user"}
            ]"#,
        )
        .unwrap()
    }

    fn parse(messages: &[Value]) -> Vec<Message> {
        messages
            .iter()
            .map(|m| serde_json::from_value(m.clone()).unwrap_or_else(|e| panic!("{e}: {m}")))
            .collect()
    }

    #[test]
    fn legacy_messages_do_not_parse_without_upgrade() {
        let legacy = legacy_conversation();
        let unparseable = legacy
            .iter()
            .filter(|m| serde_json::from_value::<Message>((*m).clone()).is_err())
            .count();
        // Both assistant turns and both tool results.
        assert_eq!(unparseable, 4);
    }

    #[test]
    fn upgrade_makes_every_legacy_message_parse_and_keeps_its_content() {
        let mut messages = legacy_conversation();
        upgrade(&mut messages, "anthropic");
        let parsed = parse(&messages);
        assert_eq!(parsed.len(), 5);
        assert_eq!(parsed[0], Message::user("Deploy it"));

        let Message::Assistant { id, content } = &parsed[1] else {
            panic!("assistant expected")
        };
        assert_eq!(id.as_deref(), Some("msg_1"));
        assert_eq!(content.len(), 3);
        let AssistantContent::Reasoning(reasoning) = &content[0] else {
            panic!("reasoning expected")
        };
        assert_eq!(reasoning.issuer().as_str(), "anthropic");
        let opened = reasoning.open(reasoning.issuer()).unwrap();
        assert_eq!(opened.first_text(), Some("thinking..."));
        assert_eq!(opened.first_signature(), Some("sig"));
        assert_eq!(content[1], AssistantContent::text("On it"));
        let AssistantContent::ToolCall(call) = &content[2] else {
            panic!("tool call expected")
        };
        assert_eq!(call.id, CallId::from_wire("call_abc"));
        assert_eq!(call.function.name, "strom_task_deploy");
        assert_eq!(call.function.arguments, serde_json::json!({"env": "prod"}));

        let Message::User { content } = &parsed[2] else {
            panic!("user expected")
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result expected")
        };
        assert_eq!(result.call, call.id);
        assert_eq!(result.name, "strom_task_deploy");
        assert_eq!(
            result.content,
            vec![ToolResultContent::text("{\"ok\":true}")]
        );
    }

    #[test]
    fn upgrade_keeps_both_ids_of_a_dual_id_call() {
        let mut messages = legacy_conversation();
        upgrade(&mut messages, "openai");
        let parsed = parse(&messages);
        let Message::Assistant { content, .. } = &parsed[3] else {
            panic!("assistant expected")
        };
        let AssistantContent::ToolCall(call) = &content[0] else {
            panic!("tool call expected")
        };
        assert_eq!(call.id, CallId::from_dual_wire("fc_123", "call_xyz"));
        let Message::User { content } = &parsed[4] else {
            panic!("user expected")
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result expected")
        };
        assert_eq!(result.call, call.id);
        assert_eq!(result.name, "ask_user");
    }

    #[test]
    fn upgrade_is_idempotent_and_leaves_current_messages_alone() {
        let mut once = legacy_conversation();
        upgrade(&mut once, "anthropic");
        let mut twice = once.clone();
        upgrade(&mut twice, "anthropic");
        assert_eq!(once, twice);

        let current = vec![
            serde_json::to_value(Message::user("hi")).unwrap(),
            serde_json::to_value(Message::assistant("hello")).unwrap(),
        ];
        let mut upgraded = current.clone();
        upgrade(&mut upgraded, "anthropic");
        assert_eq!(upgraded, current);
    }

    #[test]
    fn orphan_legacy_tool_result_still_parses() {
        let mut messages = vec![serde_json::json!({
            "role": "user",
            "content": [{"type": "toolresult", "id": "call_9", "content": [{"type": "text", "text": "x"}]}]
        })];
        upgrade(&mut messages, "openai");
        let parsed = parse(&messages);
        let Message::User { content } = &parsed[0] else {
            panic!("user expected")
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result expected")
        };
        assert_eq!(result.call, CallId::from_wire("call_9"));
        assert_eq!(result.name, UNKNOWN_TOOL_NAME);
    }
}
