//! Single-turn agent dispatch with retry logic and shared helpers.
//!
//! Contains `execute_single_turn_with_retry` for steps without tools,
//! and utility functions shared across both single and multi-turn paths.

use anyhow::{bail, Result};
use rig::completion::{CompletionRequest, Message, Usage};
use rig::message::{Text, UserContent};
use rig::OneOrMany;
use stroem_common::models::workflow::ActionDef;

use crate::config::AgentProviderConfig;
use crate::provider;

/// Default timeout for a single LLM API call (2 minutes).
const LLM_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Response from a single-turn LLM call.
#[derive(Debug)]
pub struct SingleTurnResponse {
    pub content: serde_json::Value,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Execute a single-turn LLM call with retry logic and timeout.
///
/// Retry strategy: exponential backoff (1s, 2s, 4s, …) with ±25% jitter,
/// up to `provider_config.max_retries` attempts. Only transient errors are
/// retried (see `is_transient_error`).
pub async fn execute_single_turn_with_retry(
    provider_config: &AgentProviderConfig,
    model_name: &str,
    prompt: &str,
    system_prompt: Option<&str>,
    action: &ActionDef,
) -> Result<SingleTurnResponse> {
    // Build effective system prompt with optional schema suffix
    let effective_system = build_effective_system(system_prompt, action);

    let temperature = action.temperature.or(provider_config.temperature);
    let max_tokens = action.max_tokens.unwrap_or(provider_config.max_tokens);

    let max_retries = provider_config.max_retries;
    let mut call_result: Result<(String, Usage)> = Err(anyhow::anyhow!("LLM call not attempted"));

    for attempt in 0..=max_retries {
        if attempt > 0 {
            // Exponential backoff: 1s, 2s, 4s, 8s, … with ±25% jitter.
            let base_ms = 1000u64 * (1u64 << (attempt - 1).min(6));
            // Deterministic jitter based on attempt number to avoid pulling in the
            // `rand` crate. Acceptable for single-tenant scenarios; multiple workers
            // retrying the same provider may still thundering-herd.
            let jitter = (attempt as u64 * 7919) % (base_ms / 4 + 1);
            let delay_ms = base_ms + jitter;
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
        }

        let prompt_msg = Message::User {
            content: OneOrMany::one(UserContent::Text(Text {
                text: prompt.to_string(),
            })),
        };

        let request = CompletionRequest {
            model: None,
            preamble: effective_system.clone(),
            chat_history: OneOrMany::one(prompt_msg),
            documents: vec![],
            tools: vec![],
            temperature: temperature.map(f64::from),
            max_tokens: Some(u64::from(max_tokens)),
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        };

        call_result = match tokio::time::timeout(
            LLM_CALL_TIMEOUT,
            provider::call_completion(provider_config, model_name, request),
        )
        .await
        {
            Ok(Ok(resp)) => {
                let text = resp
                    .choice
                    .iter()
                    .filter_map(|c| match c {
                        rig::completion::AssistantContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok((text, resp.usage))
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow::anyhow!(
                "LLM API call timed out after {}s",
                LLM_CALL_TIMEOUT.as_secs()
            )),
        };

        match &call_result {
            Ok(_) => break,
            Err(e) if provider::is_transient_error(e) && attempt < max_retries => continue,
            Err(_) => break,
        }
    }

    let (response_text, usage) = call_result?;

    // Parse the response text into the output JSON value
    let content = build_final_output(action, &response_text)?;

    Ok(SingleTurnResponse {
        content,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
    })
}

/// Strip a single layer of markdown code fences (```json ... ``` or ``` ... ```).
pub fn strip_code_fences(s: &str) -> &str {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("```") {
        // Skip optional language tag on the first line
        let after_tag = if let Some(nl) = rest.find('\n') {
            &rest[nl + 1..]
        } else {
            return s; // No newline — malformed fence, return as-is
        };
        if let Some(end) = after_tag.rfind("```") {
            return after_tag[..end].trim();
        }
    }
    s
}

/// Human-readable JSON value type name for error messages.
pub fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Truncate a string to at most `max_bytes` bytes, respecting UTF-8 char boundaries.
pub fn truncate_for_error(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Build the effective system prompt with optional JSON schema suffix.
pub fn build_effective_system(system_prompt: Option<&str>, action: &ActionDef) -> Option<String> {
    let schema_suffix: Option<String> = action.output.as_ref().map(|output_def| {
        let schema = output_def.to_json_schema();
        let schema_str =
            serde_json::to_string_pretty(&schema).unwrap_or_else(|_| schema.to_string());
        format!(
            "\n\nYou must respond with a JSON object matching this schema:\n{}\nReturn ONLY the JSON object, no other text, no markdown code fences.",
            schema_str
        )
    });

    match (system_prompt, schema_suffix.as_deref()) {
        (Some(sys), Some(suffix)) => Some(format!("{}{}", sys, suffix)),
        (None, Some(suffix)) => Some(suffix.to_string()),
        (Some(sys), None) => Some(sys.to_string()),
        (None, None) => None,
    }
}

/// Build the final output JSON from the agent's response text.
pub fn build_final_output(action: &ActionDef, response_text: &str) -> Result<serde_json::Value> {
    if action.output.is_some() {
        let trimmed = response_text.trim();
        let json_str = strip_code_fences(trimmed);
        match serde_json::from_str::<serde_json::Value>(json_str) {
            Ok(json) if json.is_object() => Ok(json),
            Ok(json) => {
                bail!(
                    "output requires a JSON object but got {}",
                    json_type_name(&json)
                );
            }
            Err(e) => {
                bail!(
                    "output requires a JSON object but response could not be parsed: {}. Response was: {}",
                    e,
                    truncate_for_error(trimmed, 200)
                );
            }
        }
    } else {
        match serde_json::from_str::<serde_json::Value>(response_text) {
            Ok(json) if json.is_object() => Ok(json),
            _ => Ok(serde_json::json!({ "text": response_text })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal `ActionDef` for testing, optionally with a structured `output` schema.
    ///
    /// `output_schema` should be a JSON object whose keys are field names and values are
    /// objects with at least a `"type"` property (e.g. `{"summary": {"type": "string"}}`).
    /// When `None`, the action has no structured output (`action.output` is `None`).
    fn make_action(output_schema: Option<serde_json::Value>) -> ActionDef {
        let mut spec = serde_json::json!({ "type": "agent" });
        if let Some(schema) = output_schema {
            spec["output"] = schema;
        }
        serde_json::from_value(spec).expect("make_action: failed to deserialize ActionDef")
    }

    // --- build_effective_system tests ---

    #[test]
    fn test_build_effective_system_with_system_and_schema() {
        let action = make_action(Some(serde_json::json!({"summary": {"type": "string"}})));
        let result = build_effective_system(Some("You are helpful."), &action);
        assert!(result.is_some());
        let s = result.unwrap();
        assert!(s.starts_with("You are helpful."));
        assert!(s.contains("JSON object matching this schema"));
    }

    #[test]
    fn test_build_effective_system_schema_only() {
        let action = make_action(Some(serde_json::json!({"field": {"type": "string"}})));
        let result = build_effective_system(None, &action);
        assert!(result.is_some());
        assert!(result.unwrap().contains("JSON object"));
    }

    #[test]
    fn test_build_effective_system_system_only() {
        let action = make_action(None);
        let result = build_effective_system(Some("Be concise."), &action);
        assert_eq!(result, Some("Be concise.".to_string()));
    }

    #[test]
    fn test_build_effective_system_neither() {
        let action = make_action(None);
        let result = build_effective_system(None, &action);
        assert!(result.is_none());
    }

    // --- build_final_output tests ---

    #[test]
    fn test_build_final_output_structured_valid_json() {
        let action = make_action(Some(serde_json::json!({"category": {"type": "string"}})));
        let result = build_final_output(&action, r#"{"category": "bug"}"#).unwrap();
        assert_eq!(result["category"], "bug");
    }

    #[test]
    fn test_build_final_output_structured_code_fences() {
        let action = make_action(Some(serde_json::json!({"x": {"type": "string"}})));
        let result = build_final_output(&action, "```json\n{\"x\": \"val\"}\n```").unwrap();
        assert_eq!(result["x"], "val");
    }

    #[test]
    fn test_build_final_output_structured_non_object_error() {
        let action = make_action(Some(serde_json::json!({"x": {"type": "string"}})));
        let result = build_final_output(&action, "[1, 2, 3]");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("array"));
    }

    #[test]
    fn test_build_final_output_structured_invalid_json_error() {
        let action = make_action(Some(serde_json::json!({"x": {"type": "string"}})));
        let result = build_final_output(&action, "not json at all");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("could not be parsed"));
    }

    #[test]
    fn test_build_final_output_unstructured_json_object() {
        let action = make_action(None);
        let result = build_final_output(&action, r#"{"key": "val"}"#).unwrap();
        assert_eq!(result["key"], "val");
    }

    #[test]
    fn test_build_final_output_unstructured_plain_text() {
        let action = make_action(None);
        let result = build_final_output(&action, "Hello world").unwrap();
        assert_eq!(result["text"], "Hello world");
    }

    #[test]
    fn test_build_final_output_unstructured_json_array() {
        let action = make_action(None);
        let result = build_final_output(&action, "[1, 2, 3]").unwrap();
        assert_eq!(result["text"], "[1, 2, 3]");
    }

    #[test]
    fn test_strip_code_fences_no_fences() {
        assert_eq!(strip_code_fences(r#"{"key":"val"}"#), r#"{"key":"val"}"#);
    }

    #[test]
    fn test_strip_code_fences_json_fences() {
        let input = "```json\n{\"key\":\"val\"}\n```";
        assert_eq!(strip_code_fences(input), r#"{"key":"val"}"#);
    }

    #[test]
    fn test_strip_code_fences_plain_fences() {
        let input = "```\n{\"key\":\"val\"}\n```";
        assert_eq!(strip_code_fences(input), r#"{"key":"val"}"#);
    }

    #[test]
    fn test_truncate_for_error_short() {
        assert_eq!(truncate_for_error("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_for_error_long() {
        let s = "a".repeat(300);
        let truncated = truncate_for_error(&s, 200);
        assert_eq!(truncated.len(), 200);
    }

    #[test]
    fn test_json_type_name() {
        assert_eq!(json_type_name(&serde_json::Value::Null), "null");
        assert_eq!(json_type_name(&serde_json::Value::Bool(true)), "boolean");
        assert_eq!(json_type_name(&serde_json::Value::Array(vec![])), "array");
    }

    #[test]
    fn test_truncate_for_error_multibyte_utf8() {
        let s = "Hello 🌍 world";
        let result = truncate_for_error(s, 7);
        assert!(result.len() <= 7);
        assert!(
            s.is_char_boundary(result.len()),
            "result must end on a char boundary"
        );
        assert_eq!(result, "Hello ");
    }

    #[test]
    fn test_strip_code_fences_no_closing_fence() {
        let input = "```json\n{\"key\":\"val\"}";
        assert_eq!(strip_code_fences(input), input);
    }

    #[test]
    fn test_strip_code_fences_only_opening() {
        let input = "```";
        assert_eq!(strip_code_fences(input), input);
    }
}
