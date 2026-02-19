use anyhow::{Context, Result};
use std::collections::HashMap;
use tera::Tera;

/// Tera filter that resolves `ref+` secret references via the vals CLI.
///
/// Usage in templates: `{{ secret.KEY | vals }}`
/// - Non-string values pass through unchanged
/// - Strings not starting with `ref+` pass through unchanged
/// - Strings starting with `ref+` are resolved via `vals eval`
fn vals_filter(
    value: &tera::Value,
    _args: &HashMap<String, tera::Value>,
) -> tera::Result<tera::Value> {
    let s = match value.as_str() {
        Some(s) => s,
        None => return Ok(value.clone()),
    };

    if !s.starts_with("ref+") {
        return Ok(value.clone());
    }

    let input = serde_json::json!({"_v": s});
    let input_str = serde_json::to_string(&input)
        .map_err(|e| tera::Error::msg(format!("vals: serialize failed: {e}")))?;

    let mut child = std::process::Command::new("vals")
        .args(["eval", "-f", "-", "-o", "json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            tera::Error::msg(format!(
                "vals CLI not found. Install vals to use ref+ secrets: {e}"
            ))
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(input_str.as_bytes())
            .map_err(|e| tera::Error::msg(format!("vals: stdin write failed: {e}")))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| tera::Error::msg(format!("vals: process failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(tera::Error::msg(format!(
            "vals eval failed (exit {}): {}",
            output.status,
            stderr.trim()
        )));
    }

    let resolved: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| tera::Error::msg(format!("vals: invalid output JSON: {e}")))?;

    match resolved.get("_v").and_then(|v| v.as_str()) {
        Some(resolved_str) => Ok(tera::Value::String(resolved_str.to_string())),
        None => Err(tera::Error::msg("vals: resolved output missing '_v' key")),
    }
}

/// Renders a single Tera template string against a JSON context
pub fn render_template(template: &str, context: &serde_json::Value) -> Result<String> {
    let mut tera = Tera::default();
    let template_name = "__template__";

    tera.add_raw_template(template_name, template)
        .context("Failed to parse template")?;

    tera.register_filter("vals", vals_filter);

    let tera_context =
        tera::Context::from_serialize(context).context("Failed to convert JSON to Tera context")?;

    tera.render(template_name, &tera_context)
        .context("Failed to render template")
}

/// Renders all values in a String→String env map, returns a new HashMap.
/// Each value is treated as a Tera template.
pub fn render_env_map(
    env_map: &HashMap<String, String>,
    context: &serde_json::Value,
) -> Result<HashMap<String, String>> {
    let mut result = HashMap::new();

    for (key, value) in env_map {
        let rendered = render_template(value, context)
            .with_context(|| format!("Failed to render env template for key '{}'", key))?;
        result.insert(key.clone(), rendered);
    }

    Ok(result)
}

/// Renders an optional string template. Returns None if input is None.
pub fn render_string_opt(
    template: &Option<String>,
    context: &serde_json::Value,
) -> Result<Option<String>> {
    match template {
        Some(s) => {
            let rendered =
                render_template(s, context).context("Failed to render string template")?;
            Ok(Some(rendered))
        }
        None => Ok(None),
    }
}

/// Renders all string values in an input map, returns a new JSON object.
/// Non-string values pass through unchanged.
pub fn render_input_map(
    input_map: &HashMap<String, serde_json::Value>,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    let mut result = serde_json::Map::new();

    for (key, value) in input_map {
        let rendered_value = match value {
            serde_json::Value::String(s) => {
                let rendered = render_template(s, context)
                    .with_context(|| format!("Failed to render template for key '{}'", key))?;
                serde_json::Value::String(rendered)
            }
            // Pass through non-string values unchanged
            other => other.clone(),
        };
        result.insert(key.clone(), rendered_value);
    }

    Ok(serde_json::Value::Object(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_render_simple_variable() {
        let template = "Hello {{ name }}";
        let context = json!({"name": "World"});

        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn test_render_nested_context() {
        let template = "{{ user.name }} lives in {{ user.city }}";
        let context = json!({
            "user": {
                "name": "Alice",
                "city": "Copenhagen"
            }
        });

        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "Alice lives in Copenhagen");
    }

    #[test]
    fn test_render_step_output() {
        let template = "Previous step returned: {{ step1.output.result }}";
        let context = json!({
            "step1": {
                "output": {
                    "result": "success"
                }
            }
        });

        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "Previous step returned: success");
    }

    #[test]
    fn test_render_missing_variable() {
        let template = "Hello {{ missing }}";
        let context = json!({});

        let result = render_template(template, &context);
        assert!(result.is_err());
    }

    #[test]
    fn test_render_input_map_all_strings() {
        let mut input_map = HashMap::new();
        input_map.insert("name".to_string(), json!("{{ input.name }}"));
        input_map.insert("greeting".to_string(), json!("Hello {{ input.name }}"));

        let context = json!({
            "input": {
                "name": "Alice"
            }
        });

        let result = render_input_map(&input_map, &context).unwrap();
        assert_eq!(result["name"], "Alice");
        assert_eq!(result["greeting"], "Hello Alice");
    }

    #[test]
    fn test_render_input_map_mixed_types() {
        let mut input_map = HashMap::new();
        input_map.insert("name".to_string(), json!("{{ input.name }}"));
        input_map.insert("count".to_string(), json!(42));
        input_map.insert("enabled".to_string(), json!(true));
        input_map.insert("items".to_string(), json!(["a", "b", "c"]));
        input_map.insert("config".to_string(), json!({"key": "value"}));

        let context = json!({
            "input": {
                "name": "Bob"
            }
        });

        let result = render_input_map(&input_map, &context).unwrap();
        assert_eq!(result["name"], "Bob");
        assert_eq!(result["count"], 42);
        assert_eq!(result["enabled"], true);
        assert_eq!(result["items"], json!(["a", "b", "c"]));
        assert_eq!(result["config"], json!({"key": "value"}));
    }

    #[test]
    fn test_render_input_map_empty() {
        let input_map = HashMap::new();
        let context = json!({});

        let result = render_input_map(&input_map, &context).unwrap();
        assert_eq!(result, json!({}));
    }

    #[test]
    fn test_render_input_map_template_error() {
        let mut input_map = HashMap::new();
        input_map.insert("bad".to_string(), json!("{{ missing.value }}"));

        let context = json!({});

        let result = render_input_map(&input_map, &context);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("bad"));
    }

    #[test]
    fn test_render_with_filters() {
        let template = "{{ name | upper }}";
        let context = json!({"name": "alice"});

        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "ALICE");
    }

    #[test]
    fn test_render_with_conditionals() {
        let template = "{% if enabled %}Active{% else %}Inactive{% endif %}";
        let context = json!({"enabled": true});

        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "Active");
    }

    #[test]
    fn test_render_workflow_like_context() {
        let template = "Deploy {{ input.env }} with tag {{ build.output.tag }}";
        let context = json!({
            "input": {
                "env": "production",
                "repo": "https://github.com/org/app.git"
            },
            "build": {
                "output": {
                    "tag": "v1.2.3"
                }
            }
        });

        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "Deploy production with tag v1.2.3");
    }

    #[test]
    fn test_render_input_map_workflow_step() {
        let mut input_map = HashMap::new();
        input_map.insert("repo".to_string(), json!("{{ input.repo }}"));
        input_map.insert("tag".to_string(), json!("{{ build.output.tag }}"));
        input_map.insert("replicas".to_string(), json!(3));

        let context = json!({
            "input": {
                "repo": "company/app"
            },
            "build": {
                "output": {
                    "tag": "v2.0.0"
                }
            }
        });

        let result = render_input_map(&input_map, &context).unwrap();
        assert_eq!(result["repo"], "company/app");
        assert_eq!(result["tag"], "v2.0.0");
        assert_eq!(result["replicas"], 3);
    }

    #[test]
    fn test_render_env_map_simple() {
        let mut env = HashMap::new();
        env.insert("HOST".to_string(), "{{ input.host }}".to_string());
        env.insert("PORT".to_string(), "5432".to_string());

        let context = json!({ "input": { "host": "localhost" } });
        let result = render_env_map(&env, &context).unwrap();
        assert_eq!(result["HOST"], "localhost");
        assert_eq!(result["PORT"], "5432");
    }

    #[test]
    fn test_render_env_map_with_secrets() {
        let mut env = HashMap::new();
        env.insert("DB_PASSWORD".to_string(), "{{ secret.db_pw }}".to_string());
        env.insert("API_KEY".to_string(), "{{ secret.api_key }}".to_string());

        let context = json!({
            "secret": {
                "db_pw": "ref+awsssm:///prod/db/password",
                "api_key": "ref+vault://secret/data/api#key"
            }
        });
        let result = render_env_map(&env, &context).unwrap();
        assert_eq!(result["DB_PASSWORD"], "ref+awsssm:///prod/db/password");
        assert_eq!(result["API_KEY"], "ref+vault://secret/data/api#key");
    }

    #[test]
    fn test_render_env_map_with_nested_secrets() {
        let mut env = HashMap::new();
        env.insert(
            "DB_PASSWORD".to_string(),
            "{{ secret.db.password }}".to_string(),
        );
        env.insert("DB_HOST".to_string(), "{{ secret.db.host }}".to_string());
        env.insert("API_KEY".to_string(), "{{ secret.api_key }}".to_string());

        let context = json!({
            "secret": {
                "db": {
                    "password": "ref+sops://secrets.enc.yaml#/db/password",
                    "host": "ref+sops://secrets.enc.yaml#/db/host"
                },
                "api_key": "ref+vault://secret/data/api#key"
            }
        });
        let result = render_env_map(&env, &context).unwrap();
        assert_eq!(
            result["DB_PASSWORD"],
            "ref+sops://secrets.enc.yaml#/db/password"
        );
        assert_eq!(result["DB_HOST"], "ref+sops://secrets.enc.yaml#/db/host");
        assert_eq!(result["API_KEY"], "ref+vault://secret/data/api#key");
    }

    #[test]
    fn test_render_env_map_mixed() {
        let mut env = HashMap::new();
        env.insert("STATIC".to_string(), "plain-value".to_string());
        env.insert("DYNAMIC".to_string(), "{{ input.name }}".to_string());
        env.insert("SECRET".to_string(), "{{ secret.token }}".to_string());

        let context = json!({
            "input": { "name": "alice" },
            "secret": { "token": "ref+vault://x" }
        });
        let result = render_env_map(&env, &context).unwrap();
        assert_eq!(result["STATIC"], "plain-value");
        assert_eq!(result["DYNAMIC"], "alice");
        assert_eq!(result["SECRET"], "ref+vault://x");
    }

    #[test]
    fn test_render_env_map_error() {
        let mut env = HashMap::new();
        env.insert("BAD".to_string(), "{{ missing.var }}".to_string());

        let context = json!({});
        let result = render_env_map(&env, &context);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("BAD"));
    }

    #[test]
    fn test_render_string_opt_some() {
        let template = Some("echo {{ input.name }}".to_string());
        let context = json!({ "input": { "name": "world" } });
        let result = render_string_opt(&template, &context).unwrap();
        assert_eq!(result, Some("echo world".to_string()));
    }

    #[test]
    fn test_render_step_output_with_sanitized_hyphen() {
        // Step names like "say-hello" must be sanitized to "say_hello" in context
        // because Tera interprets hyphens as subtraction
        let template = "{{ say_hello.output.greeting }}";
        let context = json!({
            "say_hello": {
                "output": {
                    "greeting": "Hello World"
                }
            }
        });

        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn test_render_hyphenated_name_fails() {
        // Proves that hyphens in variable names DON'T work in Tera
        let template = "{{ say-hello.output.greeting }}";
        let context = json!({
            "say-hello": {
                "output": {
                    "greeting": "Hello World"
                }
            }
        });

        let result = render_template(template, &context);
        assert!(result.is_err());
    }

    #[test]
    fn test_render_string_opt_none() {
        let template: Option<String> = None;
        let context = json!({});
        let result = render_string_opt(&template, &context).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_vals_filter_passthrough_non_string() {
        let value = json!(42);
        let args = HashMap::new();
        let result = vals_filter(&value, &args).unwrap();
        assert_eq!(result, json!(42));
    }

    #[test]
    fn test_vals_filter_passthrough_no_ref() {
        let value = json!("plain-text");
        let args = HashMap::new();
        let result = vals_filter(&value, &args).unwrap();
        assert_eq!(result, json!("plain-text"));
    }

    #[test]
    fn test_vals_filter_passthrough_empty() {
        let value = json!("");
        let args = HashMap::new();
        let result = vals_filter(&value, &args).unwrap();
        assert_eq!(result, json!(""));
    }

    #[test]
    fn test_vals_filter_passthrough_boolean() {
        let args = HashMap::new();
        let result = vals_filter(&json!(true), &args).unwrap();
        assert_eq!(result, json!(true));
    }

    #[test]
    fn test_vals_filter_passthrough_null() {
        let args = HashMap::new();
        let result = vals_filter(&json!(null), &args).unwrap();
        assert_eq!(result, json!(null));
    }

    #[test]
    fn test_vals_filter_ref_value_errors() {
        // When vals is not installed: "vals CLI not found"
        // When vals is installed but backend unreachable: "vals eval failed"
        let value = json!("ref+vault://secret/key");
        let args = HashMap::new();
        let result = vals_filter(&value, &args);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("vals CLI not found") || err.contains("vals eval failed"),
            "Expected vals-related error, got: {err}"
        );
    }

    #[test]
    fn test_vals_filter_in_template_passthrough() {
        let template = "password={{ db_pw | vals }}";
        let context = json!({"db_pw": "plain-value"});
        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "password=plain-value");
    }

    #[test]
    fn test_vals_filter_in_template_ref_errors() {
        // With a ref+ value, vals is invoked — either it's missing or the backend is unreachable
        let template = "password={{ secret.KEY | vals }}";
        let context = json!({"secret": {"KEY": "ref+awsssm:///prod/db/password"}});
        let result = render_template(template, &context);
        assert!(result.is_err());
    }

    #[test]
    fn test_vals_filter_chained_with_other_filters() {
        // vals on a non-ref+ string passes through, then upper transforms it
        let template = "{{ name | vals | upper }}";
        let context = json!({"name": "hello"});
        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "HELLO");
    }

    #[test]
    fn test_vals_filter_with_literal_string() {
        // Inline literal strings that don't start with ref+ pass through vals unchanged
        let template = "{{ 'plain-value' | vals }}";
        let context = json!({});
        let result = render_template(template, &context).unwrap();
        assert_eq!(result, "plain-value");
    }
}
