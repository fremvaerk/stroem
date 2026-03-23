//! LLM provider dispatch via rig-core.
//!
//! Contains `call_completion` which builds a provider client and calls
//! `CompletionModel::completion()`. Used by both single-turn and multi-turn
//! dispatch paths.

use anyhow::{bail, Context, Result};
use rig::completion::{AssistantContent, CompletionModel as _, CompletionRequest, Usage};
use rig::OneOrMany;

use crate::config::AgentProviderConfig;

/// Simplified completion response (provider-independent).
#[derive(Debug)]
pub struct CompletionResponse {
    pub choice: OneOrMany<AssistantContent>,
    pub usage: Usage,
    pub message_id: Option<String>,
}

/// Call the LLM completion endpoint using rig-core.
///
/// Uses the lower-level `CompletionModel::completion()` interface which
/// returns real token usage counts. Used by both the multi-turn dispatch
/// loop and the single-turn path.
pub async fn call_completion(
    provider_config: &AgentProviderConfig,
    model_name: &str,
    request: CompletionRequest,
) -> Result<CompletionResponse> {
    use rig::prelude::CompletionClient as _;

    /// Build a provider client, call completion, and erase the raw response type.
    macro_rules! call_model {
        ($client_type:ty, $label:expr, $api_key:expr) => {{
            let mut builder = <$client_type>::builder().api_key($api_key);
            if let Some(ref endpoint) = provider_config.api_endpoint {
                builder = builder.base_url(endpoint);
            }
            let client = builder
                .build()
                .context(concat!("Failed to build ", $label, " client"))?;
            let model = client.completion_model(model_name);
            let resp = model
                .completion(request)
                .await
                .context(concat!($label, " completion call failed"))?;
            return Ok(CompletionResponse {
                choice: resp.choice,
                usage: resp.usage,
                message_id: resp.message_id,
            });
        }};
    }

    let require_api_key = || -> Result<&str> {
        provider_config
            .api_key
            .as_deref()
            .context("Agent provider requires api_key")
    };

    // Each macro arm returns early via `return Ok(...)` to erase the
    // provider-specific raw response type. The match itself is `!` (never).
    match provider_config.provider_type.as_str() {
        "anthropic" => call_model!(
            rig::providers::anthropic::Client,
            "Anthropic",
            require_api_key()?.to_string()
        ),
        "openai" => call_model!(
            rig::providers::openai::CompletionsClient,
            "OpenAI",
            require_api_key()?.to_string()
        ),
        "cohere" => call_model!(
            rig::providers::cohere::Client,
            "Cohere",
            require_api_key()?.to_string()
        ),
        "deepseek" => call_model!(
            rig::providers::deepseek::Client,
            "DeepSeek",
            require_api_key()?.to_string()
        ),
        "gemini" => call_model!(
            rig::providers::gemini::Client,
            "Gemini",
            require_api_key()?.to_string()
        ),
        "groq" => call_model!(
            rig::providers::groq::Client,
            "Groq",
            require_api_key()?.to_string()
        ),
        "mistral" => call_model!(
            rig::providers::mistral::Client,
            "Mistral",
            require_api_key()?.to_string()
        ),
        "openrouter" => call_model!(
            rig::providers::openrouter::Client,
            "OpenRouter",
            require_api_key()?.to_string()
        ),
        "together" => call_model!(
            rig::providers::together::Client,
            "Together",
            require_api_key()?.to_string()
        ),
        "xai" => call_model!(
            rig::providers::xai::Client,
            "xAI",
            require_api_key()?.to_string()
        ),
        "perplexity" => call_model!(
            rig::providers::perplexity::Client,
            "Perplexity",
            require_api_key()?.to_string()
        ),
        "galadriel" => call_model!(
            rig::providers::galadriel::Client,
            "Galadriel",
            require_api_key()?.to_string()
        ),
        "huggingface" => call_model!(
            rig::providers::huggingface::Client,
            "HuggingFace",
            require_api_key()?.to_string()
        ),
        "hyperbolic" => call_model!(
            rig::providers::hyperbolic::Client,
            "Hyperbolic",
            require_api_key()?.to_string()
        ),
        "mira" => call_model!(
            rig::providers::mira::Client,
            "Mira",
            require_api_key()?.to_string()
        ),
        "moonshot" => call_model!(
            rig::providers::moonshot::Client,
            "Moonshot",
            require_api_key()?.to_string()
        ),
        "ollama" => call_model!(
            rig::providers::ollama::Client,
            "Ollama",
            rig::client::Nothing
        ),
        "llamafile" => call_model!(
            rig::providers::llamafile::Client,
            "Llamafile",
            rig::client::Nothing
        ),
        "azure" => {
            let api_key = require_api_key()?;
            let endpoint = provider_config
                .api_endpoint
                .as_deref()
                .context("Azure provider requires api_endpoint")?;
            let auth = rig::providers::azure::AzureOpenAIAuth::ApiKey(api_key.to_string());
            let client = rig::providers::azure::Client::builder()
                .api_key(auth)
                .azure_endpoint(endpoint.to_string())
                .build()
                .context("Failed to build Azure client")?;
            let model = client.completion_model(model_name);
            let resp = model
                .completion(request)
                .await
                .context("Azure completion call failed")?;
            #[allow(clippy::needless_return)]
            return Ok(CompletionResponse {
                choice: resp.choice,
                usage: resp.usage,
                message_id: resp.message_id,
            });
        }
        other => bail!("Unknown agent provider type: {}", other),
    }
}

/// Returns `true` if the error is likely transient and worth retrying.
///
/// Matches HTTP status codes precisely (e.g., "status: 429") to avoid false
/// positives from error messages that happen to contain numeric substrings.
pub fn is_transient_error(err: &anyhow::Error) -> bool {
    let msg = format!("{:#}", err).to_lowercase();
    let has_transient_status = msg.contains("status: 429")
        || msg.contains("status: 500")
        || msg.contains("status: 502")
        || msg.contains("status: 503")
        || msg.contains("status: 529")
        || msg.contains("http 429")
        || msg.contains("http 500")
        || msg.contains("http 502")
        || msg.contains("http 503")
        || msg.contains("http 529")
        || msg.contains("429 too many")
        || msg.contains("500 internal")
        || msg.contains("502 bad gateway")
        || msg.contains("503 service")
        || msg.contains("529 ");
    let has_transient_keyword = msg.contains("timed out")
        || msg.contains("timeout")
        || msg.contains("connection refused")
        || msg.contains("connection reset")
        || msg.contains("connection closed")
        || msg.contains("temporarily unavailable")
        || msg.contains("overloaded");
    has_transient_status || has_transient_keyword
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SUPPORTED_AGENT_PROVIDERS;

    fn make_provider(provider_type: &str) -> AgentProviderConfig {
        AgentProviderConfig {
            provider_type: provider_type.to_string(),
            api_key: Some("test-key".to_string()),
            api_endpoint: None,
            model: "model".to_string(),
            max_tokens: 4096,
            temperature: None,
            max_retries: 3,
        }
    }

    #[tokio::test]
    async fn test_call_llm_unknown_provider_type() {
        let provider_config = AgentProviderConfig {
            provider_type: "bedrock".to_string(),
            api_key: Some("test-key".to_string()),
            api_endpoint: None,
            model: "model".to_string(),
            max_tokens: 4096,
            temperature: None,
            max_retries: 0,
        };
        let request = CompletionRequest {
            model: None,
            preamble: None,
            chat_history: OneOrMany::one(rig::completion::Message::User {
                content: OneOrMany::one(rig::message::UserContent::Text(rig::message::Text {
                    text: "hello".to_string(),
                })),
            }),
            documents: vec![],
            tools: vec![],
            temperature: None,
            max_tokens: None,
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        };
        let result = call_completion(&provider_config, "some-model", request).await;
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("Unknown agent provider type"),
            "Expected 'Unknown agent provider type', got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_call_llm_missing_api_key() {
        let provider_config = AgentProviderConfig {
            provider_type: "anthropic".to_string(),
            api_key: None,
            api_endpoint: None,
            model: "claude-3-5-haiku-latest".to_string(),
            max_tokens: 4096,
            temperature: None,
            max_retries: 0,
        };
        let request = CompletionRequest {
            model: None,
            preamble: None,
            chat_history: OneOrMany::one(rig::completion::Message::User {
                content: OneOrMany::one(rig::message::UserContent::Text(rig::message::Text {
                    text: "hello".to_string(),
                })),
            }),
            documents: vec![],
            tools: vec![],
            temperature: None,
            max_tokens: None,
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        };
        let result = call_completion(&provider_config, "claude-3-5-haiku-latest", request).await;
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("api_key"),
            "Expected error about api_key, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_dispatch_covers_all_supported_providers() {
        for &provider_type in SUPPORTED_AGENT_PROVIDERS {
            let provider_config = AgentProviderConfig {
                provider_type: provider_type.to_string(),
                api_key: Some("test-key".to_string()),
                api_endpoint: Some("http://127.0.0.1:1".to_string()),
                model: "test-model".to_string(),
                max_tokens: 4096,
                temperature: None,
                max_retries: 0,
            };
            let request = CompletionRequest {
                model: None,
                preamble: None,
                chat_history: OneOrMany::one(rig::completion::Message::User {
                    content: OneOrMany::one(rig::message::UserContent::Text(rig::message::Text {
                        text: "hello".to_string(),
                    })),
                }),
                documents: vec![],
                tools: vec![],
                temperature: None,
                max_tokens: None,
                tool_choice: None,
                additional_params: None,
                output_schema: None,
            };
            let result = call_completion(&provider_config, "test-model", request).await;
            assert!(result.is_err());
            let msg = format!("{:#}", result.unwrap_err());
            assert!(
                !msg.contains("Unknown agent provider type"),
                "Provider '{}' is not handled in dispatch: {}",
                provider_type,
                msg
            );
        }
    }

    #[test]
    fn test_is_transient_error_429() {
        let err = anyhow::anyhow!("HTTP 429 Too Many Requests");
        assert!(is_transient_error(&err));
    }

    #[test]
    fn test_is_transient_error_503() {
        let err = anyhow::anyhow!("status: 503 Service Unavailable");
        assert!(is_transient_error(&err));
    }

    #[test]
    fn test_is_not_transient_500_in_message() {
        let err = anyhow::anyhow!("max_tokens must be <= 4500");
        assert!(!is_transient_error(&err));
    }

    #[test]
    fn test_is_not_transient_connection_in_validation() {
        let err = anyhow::anyhow!("connection_type is invalid");
        assert!(!is_transient_error(&err));
    }

    #[test]
    fn test_is_transient_error_connection() {
        let err = anyhow::anyhow!("connection refused");
        assert!(is_transient_error(&err));
    }

    #[test]
    fn test_is_transient_error_timeout() {
        let err = anyhow::anyhow!("request timed out");
        assert!(is_transient_error(&err));
    }

    #[test]
    fn test_is_not_transient_error_401() {
        let err = anyhow::anyhow!("HTTP 401 Unauthorized");
        assert!(!is_transient_error(&err));
    }

    #[test]
    fn test_is_transient_error_overloaded() {
        let err = anyhow::anyhow!("API overloaded, please try again");
        assert!(is_transient_error(&err));
    }

    #[test]
    fn test_make_provider_has_temperature() {
        let mut p = make_provider("anthropic");
        p.temperature = Some(0.7);
        assert_eq!(p.temperature, Some(0.7));
    }
}
