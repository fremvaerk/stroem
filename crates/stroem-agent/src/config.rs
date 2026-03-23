//! Agent provider configuration types.
//!
//! Moved from `stroem-server/src/config.rs` — shared by both server
//! (for provider name validation) and worker (for full LLM dispatch).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Agent (LLM) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsConfig {
    #[serde(default)]
    pub providers: HashMap<String, AgentProviderConfig>,
}

/// All supported agent provider types (must match dispatch match in provider.rs).
pub const SUPPORTED_AGENT_PROVIDERS: &[&str] = &[
    "anthropic",
    "azure",
    "cohere",
    "deepseek",
    "galadriel",
    "gemini",
    "groq",
    "huggingface",
    "hyperbolic",
    "llamafile",
    "mira",
    "mistral",
    "moonshot",
    "ollama",
    "openai",
    "openrouter",
    "perplexity",
    "together",
    "xai",
];

/// Configuration for a single LLM provider
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProviderConfig {
    /// Provider type (e.g. "anthropic", "openai", "gemini", "ollama")
    #[serde(rename = "type", alias = "provider_type")]
    pub provider_type: String,
    /// API key (can use ${ENV_VAR} syntax)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Custom API endpoint (for OpenAI-compatible servers like Ollama, vLLM)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_endpoint: Option<String>,
    /// Default model name
    pub model: String,
    /// Default max tokens
    #[serde(default = "default_agent_max_tokens")]
    pub max_tokens: u32,
    /// Default temperature
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Max retries for transient errors
    #[serde(default = "default_agent_max_retries")]
    pub max_retries: u32,
}

impl std::fmt::Debug for AgentProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentProviderConfig")
            .field("type", &self.provider_type)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("api_endpoint", &self.api_endpoint)
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

fn default_agent_max_tokens() -> u32 {
    4096
}

fn default_agent_max_retries() -> u32 {
    3
}
