//! LLM provider dispatch via rig-core.
//!
//! Contains `call_completion`, which builds the provider's completion model
//! and sends one request through it. Used by both single-turn and multi-turn
//! dispatch paths.

use anyhow::{bail, Context, Result};
use rig_core::completion::{AssistantContent, CompletionRequest, Usage};
use rig_core::driver::DynModel;
use rig_core::operation::Completion;
use rig_core::providers::anthropic::AnthropicConfig;
use rig_core::providers::cohere::CohereConfig;
use rig_core::providers::gemini::GeminiConfig;
use rig_core::providers::ollama::OllamaConfig;
use rig_core::providers::openai::wire::{self as openai_wire, Dialect};
use rig_core::providers::openai::OpenAIConfig;
use rig_core::ProviderError;

use crate::config::AgentProviderConfig;

/// Simplified completion response (provider-independent).
#[derive(Debug)]
pub struct CompletionResponse {
    pub choice: Vec<AssistantContent>,
    pub usage: Usage,
    pub message_id: Option<String>,
}

/// Galadriel's OpenAI-compatible chat-completions API.
///
/// rig-core dropped its Galadriel provider (0xPlaygrounds/rig#2041), which
/// was a plain chat-completions client on this base URL with Bearer auth, so
/// `type: galadriel` keeps working as a generic OpenAI-compatible gateway.
const GALADRIEL: Dialect = Dialect::gateway(
    "galadriel",
    "https://api.galadriel.com/v1/verified",
    "GALADRIEL_API_KEY",
);

/// A provider's completion model, type-erased, with the label used in errors.
struct ProviderModel {
    model: DynModel<Completion>,
    label: &'static str,
}

/// Build the completion model for `provider_config`.
///
/// Only constructs the client; nothing is sent. Every client shares rig's
/// bundled reqwest transport.
fn build_model(provider_config: &AgentProviderConfig, model_name: &str) -> Result<ProviderModel> {
    let endpoint = provider_config.api_endpoint.as_deref();
    let require_api_key = || -> Result<&str> {
        provider_config
            .api_key
            .as_deref()
            .context("Agent provider requires api_key")
    };
    // OpenAI-compatible vendors: the dialect carries the vendor's base URL,
    // paths and request quirks; `completion` takes the dialect's default
    // route (Chat Completions, or Responses for xAI).
    let compatible = |dialect: &Dialect, label: &'static str| -> Result<ProviderModel> {
        let mut config = OpenAIConfig::with_key(dialect, require_api_key()?);
        if let Some(endpoint) = endpoint {
            config = config.with_base_url(endpoint);
        }
        Ok(ProviderModel {
            model: config.client().completion(model_name).erase(),
            label,
        })
    };

    let model = match provider_config.provider_type.as_str() {
        "anthropic" => {
            let mut config = AnthropicConfig::new(require_api_key()?);
            if let Some(endpoint) = endpoint {
                config = config.with_base_url(endpoint);
            }
            ProviderModel {
                model: config.client().completion(model_name).erase(),
                label: "Anthropic",
            }
        }
        "openai" => {
            // Chat Completions, not the Responses API rig now defaults OpenAI
            // to: this is the endpoint `type: openai` has always called, and
            // the one OpenAI-compatible servers (vLLM, ...) behind a custom
            // `api_endpoint` implement.
            let mut config = OpenAIConfig::new(require_api_key()?);
            if let Some(endpoint) = endpoint {
                config = config.with_base_url(endpoint);
            }
            ProviderModel {
                model: config.client().chat(model_name).erase(),
                label: "OpenAI",
            }
        }
        "azure" => {
            let api_key = require_api_key()?;
            let endpoint = endpoint.context("Azure provider requires api_endpoint")?;
            // `api-key` header auth, deployment in the URL, API version
            // 2024-10-21 — the dialect's defaults.
            let config =
                OpenAIConfig::with_key(&openai_wire::AZURE, api_key).with_base_url(endpoint);
            ProviderModel {
                model: config.client().completion(model_name).erase(),
                label: "Azure",
            }
        }
        "cohere" => {
            let mut config = CohereConfig::new(require_api_key()?);
            if let Some(endpoint) = endpoint {
                config = config.with_base_url(endpoint);
            }
            ProviderModel {
                model: config.client().completion(model_name).erase(),
                label: "Cohere",
            }
        }
        "gemini" => {
            let mut config = GeminiConfig::new(require_api_key()?);
            if let Some(endpoint) = endpoint {
                config = config.with_base_url(endpoint);
            }
            ProviderModel {
                model: config.client().completion(model_name).erase(),
                label: "Gemini",
            }
        }
        "ollama" => {
            // No credential is sent, as before: a configured `api_key` is
            // ignored.
            let mut config = OllamaConfig::new();
            if let Some(endpoint) = endpoint {
                config = config.with_base_url(endpoint);
            }
            ProviderModel {
                model: config.client().completion(model_name).erase(),
                label: "Ollama",
            }
        }
        "llamafile" => {
            // rig-core replaced its llamafile provider with llama.cpp's
            // `llama-server`, which serves the same OpenAI-compatible API.
            // An empty key sends no `Authorization` header, as before. The
            // dialect's base URL includes `/v1` (`http://localhost:8080/v1`),
            // while `api_endpoint` has always named the server root, so the
            // `/v1` is appended to keep requests on `{root}/v1/chat/completions`.
            let mut config = OpenAIConfig::with_key(&openai_wire::LLAMACPP, "");
            if let Some(endpoint) = endpoint {
                config = config.with_base_url(format!("{}/v1", endpoint.trim_end_matches('/')));
            }
            ProviderModel {
                model: config.client().completion(model_name).erase(),
                label: "Llamafile",
            }
        }
        "deepseek" => compatible(&openai_wire::DEEPSEEK, "DeepSeek")?,
        "galadriel" => compatible(&GALADRIEL, "Galadriel")?,
        "groq" => compatible(&openai_wire::GROQ, "Groq")?,
        "huggingface" => compatible(&openai_wire::HUGGINGFACE, "HuggingFace")?,
        "hyperbolic" => compatible(&openai_wire::HYPERBOLIC, "Hyperbolic")?,
        "mira" => compatible(&openai_wire::MIRA, "Mira")?,
        "mistral" => compatible(&openai_wire::MISTRAL, "Mistral")?,
        "moonshot" => compatible(&openai_wire::MOONSHOT, "Moonshot")?,
        "openrouter" => compatible(&openai_wire::OPENROUTER, "OpenRouter")?,
        "perplexity" => compatible(&openai_wire::PERPLEXITY, "Perplexity")?,
        "together" => compatible(&openai_wire::TOGETHER, "Together")?,
        "xai" => compatible(&rig_core::providers::xai::DIALECT, "xAI")?,
        other => bail!("Unknown agent provider type: {}", other),
    };
    Ok(model)
}

/// Call the LLM completion endpoint using rig-core.
///
/// Sends one unary request through the provider's completion model and
/// returns its normalized response, including token usage. Used by both the
/// multi-turn dispatch loop and the single-turn path.
pub async fn call_completion(
    provider_config: &AgentProviderConfig,
    model_name: &str,
    mut request: CompletionRequest,
) -> Result<CompletionResponse> {
    let ProviderModel { model, label } = build_model(provider_config, model_name)?;
    if provider_config.provider_type == "galadriel" {
        // The removed rig Galadriel client never put `max_tokens` on the wire.
        request.max_tokens = None;
    }
    let resp = model
        .call(request)
        .await
        .map_err(CompletionCallError)
        .with_context(|| format!("{label} completion call failed"))?;
    Ok(CompletionResponse {
        choice: resp.choice,
        usage: resp.usage,
        message_id: resp.message_id,
    })
}

/// The issuer rig seals this provider's reasoning blocks to: the provider
/// descriptor's name (`anthropic`, `openai`, `gcp.gemini`, ...).
///
/// Used to lift reasoning persisted before rig sealed reasoning to its issuer
/// (see [`crate::legacy_history`]).
pub fn reasoning_issuer(provider_config: &AgentProviderConfig, model_name: &str) -> Option<String> {
    build_model(provider_config, model_name)
        .ok()
        .map(|provider| provider.model.name().to_string())
}

/// A rig [`ProviderError`] whose transport failure stays in the source chain.
///
/// `ProviderError::Http` reports no `source()`, so `{:#}` would end at
/// "error sending request for url (...)" and drop the cause ("connection
/// refused", "operation timed out") that [`is_transient_error`] reads. This
/// exposes the transport error as the source, restoring the chain the error
/// carried before rig-core 0.43.
#[derive(Debug)]
struct CompletionCallError(ProviderError);

impl std::fmt::Display for CompletionCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for CompletionCallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.0 {
            ProviderError::Http(error) => Some(&**error),
            other => std::error::Error::source(other),
        }
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
    use crate::test_support::{capture_one_request, Captured};

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

    /// A one-message request: the user prompt "hello", nothing else set.
    fn hello_request() -> CompletionRequest {
        CompletionRequest::new("hello")
    }

    /// Every supported provider builds its client and gets as far as the
    /// network: the call fails on the closed port, not on construction or
    /// request encoding, and the connect failure keeps its cause in the
    /// error chain, so it is classified transient and retried — as it was
    /// before rig-core 0.43, whose `ProviderError::Http` drops the cause.
    #[tokio::test]
    async fn test_connect_failure_reaches_transport_and_is_transient_for_every_provider() {
        for &provider_type in SUPPORTED_AGENT_PROVIDERS {
            let mut provider_config = make_provider(provider_type);
            provider_config.api_endpoint = Some("http://127.0.0.1:1".to_string());
            let request = hello_request().max_tokens(64);
            let err = call_completion(&provider_config, "test-model", request)
                .await
                .expect_err("closed port must fail");
            let msg = format!("{:#}", err);
            assert!(
                msg.contains("completion call failed"),
                "provider '{}' failed before sending: {}",
                provider_type,
                msg
            );
            assert!(
                is_transient_error(&err),
                "provider '{}': connect failure not classified transient: {}",
                provider_type,
                msg
            );
        }
    }

    /// The error text `is_transient_error` matches survives rig-core's
    /// error types: retryable statuses are transient, client errors are not.
    #[test]
    fn test_is_transient_error_classifies_rig_provider_responses() {
        let classify = |status: u16| {
            let status = http::StatusCode::from_u16(status).unwrap();
            let err = anyhow::Error::new(CompletionCallError(ProviderError::from_http_response(
                status,
                r#"{"error":{"message":"nope"}}"#,
            )))
            .context("OpenAI completion call failed");
            is_transient_error(&err)
        };
        for status in [429, 500, 502, 503, 529] {
            assert!(classify(status), "status {status} must be transient");
        }
        for status in [400, 401, 403, 404, 422] {
            assert!(!classify(status), "status {status} must not be transient");
        }
    }

    #[test]
    fn test_completion_call_error_exposes_transport_cause() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        let transport = rig_core::http_client::Error::Instance(Box::new(io));
        let err = anyhow::Error::new(CompletionCallError(ProviderError::from(transport)))
            .context("Ollama completion call failed");
        let msg = format!("{:#}", err);
        assert!(msg.contains("connection refused"), "cause lost: {msg}");
        assert!(is_transient_error(&err));
    }

    #[test]
    fn test_reasoning_issuer_is_provider_descriptor_name() {
        assert_eq!(
            reasoning_issuer(&make_provider("anthropic"), "m").as_deref(),
            Some("anthropic")
        );
        assert_eq!(
            reasoning_issuer(&make_provider("openai"), "m").as_deref(),
            Some("openai")
        );
        assert_eq!(reasoning_issuer(&make_provider("bedrock"), "m"), None);
    }

    /// Send `request` through `provider_config` (its `api_endpoint` pointed at
    /// a one-shot local listener, plus `endpoint_suffix`) and return what
    /// reached the wire. The listener answers 400, so the call itself fails.
    async fn capture(
        mut provider_config: AgentProviderConfig,
        endpoint_suffix: &str,
        request: CompletionRequest,
    ) -> Captured {
        let (base_url, server) = capture_one_request().await;
        provider_config.api_endpoint = Some(format!("{base_url}{endpoint_suffix}"));
        let _ = call_completion(&provider_config, "test-model", request).await;
        server.await.unwrap()
    }

    fn shaped_request() -> CompletionRequest {
        crate::dispatch::completion_request(
            Some("be brief"),
            Vec::new(),
            rig_core::completion::Message::user("hello"),
            Vec::new(),
            Some(0.5),
            128,
        )
    }

    #[tokio::test]
    async fn test_openai_uses_chat_completions_with_system_message_first() {
        let captured = capture(make_provider("openai"), "/v1", shaped_request()).await;
        assert_eq!(captured.request_line, "POST /v1/chat/completions HTTP/1.1");
        assert!(captured.headers.contains("authorization: bearer test-key"));
        let messages = captured.body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        // A one-part array, the shape rig-core 0.36 sent too.
        assert_eq!(
            messages[0]["content"],
            serde_json::json!([{"type": "text", "text": "be brief"}])
        );
        assert_eq!(messages.last().unwrap()["role"], "user");
        assert_eq!(captured.body["model"], "test-model");
        assert_eq!(captured.body["temperature"], 0.5);
    }

    #[tokio::test]
    async fn test_anthropic_sends_system_prompt_and_max_tokens() {
        let captured = capture(make_provider("anthropic"), "", shaped_request()).await;
        assert_eq!(captured.request_line, "POST /v1/messages HTTP/1.1");
        assert!(captured.headers.contains("x-api-key: test-key"));
        assert_eq!(captured.body["max_tokens"], 128);
        assert_eq!(captured.body["temperature"], 0.5);
        let system = captured.body["system"].to_string();
        assert!(
            system.contains("be brief"),
            "system prompt not sent: {system}"
        );
        let messages = captured.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[tokio::test]
    async fn test_azure_addresses_the_deployment_with_api_key_header() {
        let captured = capture(make_provider("azure"), "", shaped_request()).await;
        assert_eq!(
            captured.request_line,
            "POST /openai/deployments/test-model/chat/completions?api-version=2024-10-21 HTTP/1.1"
        );
        assert!(captured.headers.contains("api-key: test-key"));
        assert!(!captured.headers.contains("authorization:"));
    }

    #[tokio::test]
    async fn test_ollama_posts_api_chat_without_credentials() {
        let captured = capture(make_provider("ollama"), "", shaped_request()).await;
        assert_eq!(captured.request_line, "POST /api/chat HTTP/1.1");
        assert!(!captured.headers.contains("authorization:"));
    }

    #[tokio::test]
    async fn test_llamafile_endpoint_is_the_server_root() {
        // `api_endpoint` has always named the server root; requests still
        // go to `{root}/v1/chat/completions`, without credentials.
        let captured = capture(make_provider("llamafile"), "/", shaped_request()).await;
        assert_eq!(captured.request_line, "POST /v1/chat/completions HTTP/1.1");
        assert!(!captured.headers.contains("authorization:"));
    }

    #[tokio::test]
    async fn test_galadriel_omits_max_tokens() {
        let captured = capture(make_provider("galadriel"), "", shaped_request()).await;
        assert_eq!(captured.request_line, "POST /chat/completions HTTP/1.1");
        assert!(captured.headers.contains("authorization: bearer test-key"));
        assert!(captured.body.get("max_tokens").is_none());
        assert!(captured.body.get("max_completion_tokens").is_none());
        assert_eq!(captured.body["messages"][0]["role"], "system");
    }

    #[test]
    fn test_galadriel_and_llamafile_map_to_their_replacements() {
        let mut galadriel = make_provider("galadriel");
        galadriel.api_endpoint = None;
        let ProviderModel { model, label } = build_model(&galadriel, "m").unwrap();
        assert_eq!((model.name(), label), ("galadriel", "Galadriel"));

        let mut llamafile = make_provider("llamafile");
        llamafile.api_key = None; // never needed one
        let ProviderModel { model, label } = build_model(&llamafile, "m").unwrap();
        assert_eq!((model.name(), label), ("llamacpp", "Llamafile"));
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
        let request = hello_request();
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
        let request = hello_request();
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
            let request = hello_request();
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
