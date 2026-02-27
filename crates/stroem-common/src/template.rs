use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use tera::Tera;

use crate::models::workflow::{InputFieldDef, WorkspaceConfig};

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
///
/// For simple variable references like `{{ input.db }}` where the context value is
/// a non-string (object, array, number, boolean), the raw value is extracted directly
/// from the context. This preserves structured data (e.g. connection objects) flowing
/// through step input templates, so downstream action spec templates can access nested
/// fields like `{{ input.db.host }}`.
pub fn render_input_map(
    input_map: &HashMap<String, serde_json::Value>,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    let mut result = serde_json::Map::new();

    for (key, value) in input_map {
        let rendered_value = match value {
            serde_json::Value::String(s) => {
                // For simple variable references ({{ path.to.value }}), extract the
                // raw value from context to preserve structured data (objects, arrays).
                // This is essential for connection objects flowing through step templates.
                // Only objects and arrays are extracted directly — primitives (strings,
                // numbers, booleans) continue through Tera to produce string output,
                // preserving backward compatibility.
                if let Some(path) = extract_simple_variable_path(s) {
                    if let Some(raw_value) = resolve_context_path(context, path) {
                        if raw_value.is_object() || raw_value.is_array() {
                            raw_value
                        } else {
                            // Primitive value — render through Tera for string output
                            let rendered = render_template(s, context).with_context(|| {
                                format!("Failed to render template for key '{}'", key)
                            })?;
                            serde_json::Value::String(rendered)
                        }
                    } else {
                        // Path not found — fall through to Tera for proper error message
                        let rendered = render_template(s, context).with_context(|| {
                            format!("Failed to render template for key '{}'", key)
                        })?;
                        serde_json::Value::String(rendered)
                    }
                } else {
                    let rendered = render_template(s, context)
                        .with_context(|| format!("Failed to render template for key '{}'", key))?;
                    serde_json::Value::String(rendered)
                }
            }
            // Pass through non-string values unchanged
            other => other.clone(),
        };
        result.insert(key.clone(), rendered_value);
    }

    Ok(serde_json::Value::Object(result))
}

/// Check if a template string is a simple variable reference like `{{ input.db }}`.
/// Returns the variable path (e.g. "input.db") if it matches, None otherwise.
/// Does NOT match templates with filters, surrounding text, or multiple expressions.
fn extract_simple_variable_path(template: &str) -> Option<&str> {
    let trimmed = template.trim();
    let inner = trimmed.strip_prefix("{{")?.strip_suffix("}}")?.trim();
    // Must be a simple dot-path: alphanumeric, dots, underscores only
    if !inner.is_empty()
        && inner
            .chars()
            .all(|c| c.is_alphanumeric() || c == '.' || c == '_')
    {
        Some(inner)
    } else {
        None
    }
}

/// Resolve a dot-separated path against a JSON context value.
/// E.g. "input.db.host" resolves through context["input"]["db"]["host"].
fn resolve_context_path(context: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    let mut current = context;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current.clone())
}

/// Merge task input defaults into user-provided input.
///
/// For each field in the input schema that is missing from `user_input`:
/// - If the field has a `default`, insert it (rendering string defaults through Tera with `context`)
/// - If the field is `required` and has no default, return an error
///
/// User-provided fields always take precedence. Extra user fields not in the schema pass through.
pub fn merge_defaults(
    user_input: &serde_json::Value,
    input_schema: &HashMap<String, InputFieldDef>,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    let empty = serde_json::Map::new();
    let user_map = user_input.as_object().unwrap_or(&empty);

    let mut result = user_map.clone();

    // Fill in defaults for schema fields not provided by the user
    for (field_name, field_def) in input_schema {
        if result.contains_key(field_name) {
            continue;
        }

        if let Some(ref default_value) = field_def.default {
            let resolved = match default_value {
                serde_json::Value::String(s) => {
                    if s.contains("{{") {
                        let rendered = render_template(s, context).with_context(|| {
                            format!(
                                "Failed to render default template for input field '{}'",
                                field_name
                            )
                        })?;
                        serde_json::Value::String(rendered)
                    } else {
                        default_value.clone()
                    }
                }
                _ => default_value.clone(),
            };
            result.insert(field_name.clone(), resolved);
        }
        // Note: required-field validation is intentionally not done here.
        // Webhooks and triggers supply different input shapes (body, headers, etc.)
        // that don't match the task's input schema.
    }

    Ok(serde_json::Value::Object(result))
}

/// Primitive type names that are NOT connection type references.
const PRIMITIVE_TYPES: &[&str] = &["string", "text", "integer", "number", "boolean"];

/// Resolve connection inputs: replace connection name strings with the full connection object.
///
/// For each field in `input_schema` where `field_type` is not a primitive type,
/// it's treated as a connection type reference. The corresponding value in `input`
/// (a string naming a connection) is looked up in `workspace_config.connections`
/// and replaced with the connection's values object.
pub fn resolve_connection_inputs(
    input: &serde_json::Value,
    input_schema: &HashMap<String, InputFieldDef>,
    workspace_config: &WorkspaceConfig,
) -> Result<serde_json::Value> {
    let empty = serde_json::Map::new();
    let input_map = input.as_object().unwrap_or(&empty);
    let mut result = input_map.clone();

    for (field_name, field_def) in input_schema {
        if PRIMITIVE_TYPES.contains(&field_def.field_type.as_str()) {
            continue;
        }

        // This field references a connection type
        let value = match result.get(field_name) {
            Some(v) => v.clone(),
            None => continue, // Field not provided — skip (merge_defaults may have already handled it)
        };

        let conn_name = match value.as_str() {
            Some(s) => s,
            None => {
                // Already an object (e.g. passed inline) — skip resolution
                if value.is_object() {
                    continue;
                }
                bail!(
                    "Input field '{}' expects a connection name (string), got {}",
                    field_name,
                    value
                );
            }
        };

        let conn = workspace_config
            .connections
            .get(conn_name)
            .with_context(|| {
                format!(
                    "Input field '{}' references connection '{}' which does not exist",
                    field_name, conn_name
                )
            })?;

        // Validate connection type matches the declared input type
        if let Some(ref conn_type) = conn.connection_type {
            if conn_type != &field_def.field_type {
                bail!(
                    "Input field '{}' expects type '{}' but connection '{}' is type '{}'",
                    field_name,
                    field_def.field_type,
                    conn_name,
                    conn_type
                );
            }
        }

        // Replace the string with the connection's values object
        let values_json =
            serde_json::to_value(&conn.values).context("Failed to serialize connection values")?;
        result.insert(field_name.clone(), values_json);
    }

    Ok(serde_json::Value::Object(result))
}

/// Recursively walk a JSON value tree and render all string values containing `{{`
/// through Tera. Objects and arrays are traversed recursively; other types are cloned as-is.
pub fn render_value_deep(
    value: &serde_json::Value,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    match value {
        serde_json::Value::String(s) => {
            if s.contains("{{") {
                let rendered = render_template(s, context)
                    .with_context(|| format!("Failed to render template in value: {}", s))?;
                Ok(serde_json::Value::String(rendered))
            } else {
                Ok(value.clone())
            }
        }
        serde_json::Value::Object(map) => {
            let mut result = serde_json::Map::new();
            for (k, v) in map {
                result.insert(k.clone(), render_value_deep(v, context)?);
            }
            Ok(serde_json::Value::Object(result))
        }
        serde_json::Value::Array(arr) => {
            let mut result = Vec::new();
            for v in arr {
                result.push(render_value_deep(v, context)?);
            }
            Ok(serde_json::Value::Array(result))
        }
        _ => Ok(value.clone()),
    }
}

/// Merge action-level input defaults into already-rendered step input.
///
/// 1. Calls `merge_defaults()` to fill missing fields from the action's input schema
/// 2. Calls `render_value_deep()` on the result to render templates inside object/array defaults
///
/// This handles the case where an action defines a default like:
/// ```yaml
/// input:
///   clickhouse:
///     type: object
///     default:
///       host: "{{ secret.clickhouse.host }}"
///       port: 8443
/// ```
/// The object default is inserted by `merge_defaults()` as-is, then `render_value_deep()`
/// walks it to render the `{{ secret.clickhouse.host }}` template.
pub fn merge_action_defaults(
    rendered_input: &serde_json::Value,
    action_input_schema: &HashMap<String, InputFieldDef>,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    let merged = merge_defaults(rendered_input, action_input_schema, context)
        .context("Failed to merge action input defaults")?;
    render_value_deep(&merged, context).context("Failed to render templates in action defaults")
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

    // --- merge_defaults tests ---

    fn field(
        field_type: &str,
        required: bool,
        default: Option<serde_json::Value>,
    ) -> InputFieldDef {
        InputFieldDef {
            field_type: field_type.to_string(),
            required,
            default,
        }
    }

    #[test]
    fn test_merge_defaults_fills_missing_fields() {
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert(
            "env".to_string(),
            field("string", false, Some(json!("staging"))),
        );
        schema.insert(
            "retries".to_string(),
            field("number", false, Some(json!(3))),
        );

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["env"], "staging");
        assert_eq!(result["retries"], 3);
    }

    #[test]
    fn test_merge_defaults_user_values_take_precedence() {
        let user_input = json!({"env": "production"});
        let mut schema = HashMap::new();
        schema.insert(
            "env".to_string(),
            field("string", false, Some(json!("staging"))),
        );

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["env"], "production");
    }

    #[test]
    fn test_merge_defaults_template_rendered_with_secrets() {
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert(
            "api_key".to_string(),
            field("string", false, Some(json!("{{ secret.API_KEY }}"))),
        );

        let context = json!({ "secret": { "API_KEY": "sk-12345" } });
        let result = merge_defaults(&user_input, &schema, &context).unwrap();
        assert_eq!(result["api_key"], "sk-12345");
    }

    #[test]
    fn test_merge_defaults_required_field_missing_skipped() {
        // Required fields without defaults are silently skipped (not an error).
        // Webhooks and triggers pass different input shapes that don't match
        // the task schema, so strict validation would break those flows.
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert("name".to_string(), field("string", true, None));

        let result = merge_defaults(&user_input, &schema, &json!({}));
        assert!(result.is_ok());
        // The field is simply absent in the result
        let obj = result.unwrap();
        assert!(obj.get("name").is_none());
    }

    #[test]
    fn test_merge_defaults_required_field_with_default_ok() {
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert(
            "name".to_string(),
            field("string", true, Some(json!("default-name"))),
        );

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["name"], "default-name");
    }

    #[test]
    fn test_merge_defaults_extra_user_fields_preserved() {
        let user_input = json!({"env": "prod", "custom_flag": true});
        let mut schema = HashMap::new();
        schema.insert("env".to_string(), field("string", false, None));

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["env"], "prod");
        assert_eq!(result["custom_flag"], true);
    }

    #[test]
    fn test_merge_defaults_empty_schema_empty_input() {
        let result = merge_defaults(&json!({}), &HashMap::new(), &json!({})).unwrap();
        assert_eq!(result, json!({}));
    }

    #[test]
    fn test_merge_defaults_non_string_defaults_passthrough() {
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert("count".to_string(), field("number", false, Some(json!(42))));
        schema.insert(
            "enabled".to_string(),
            field("boolean", false, Some(json!(true))),
        );
        schema.insert(
            "tags".to_string(),
            field("string", false, Some(json!(["a", "b"]))),
        );

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["count"], 42);
        assert_eq!(result["enabled"], true);
        assert_eq!(result["tags"], json!(["a", "b"]));
    }

    #[test]
    fn test_merge_defaults_string_without_template_passthrough() {
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert(
            "env".to_string(),
            field("string", false, Some(json!("staging"))),
        );

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["env"], "staging");
    }

    #[test]
    fn test_merge_defaults_template_render_error() {
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert(
            "key".to_string(),
            field("string", false, Some(json!("{{ missing.var }}"))),
        );

        let result = merge_defaults(&user_input, &schema, &json!({}));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("key"),
            "Error should mention field name: {err}"
        );
    }

    #[test]
    fn test_merge_defaults_optional_field_no_default_omitted() {
        let user_input = json!({});
        let mut schema = HashMap::new();
        schema.insert("optional_field".to_string(), field("string", false, None));

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert!(result.get("optional_field").is_none());
    }

    #[test]
    fn test_merge_defaults_null_input() {
        // null input treated as empty object
        let mut schema = HashMap::new();
        schema.insert(
            "env".to_string(),
            field("string", false, Some(json!("staging"))),
        );

        let result = merge_defaults(&json!(null), &schema, &json!({})).unwrap();
        assert_eq!(result["env"], "staging");
    }

    #[test]
    fn test_merge_defaults_required_field_provided_by_user() {
        let user_input = json!({"name": "alice"});
        let mut schema = HashMap::new();
        schema.insert("name".to_string(), field("string", true, None));

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["name"], "alice");
    }

    #[test]
    fn test_merge_defaults_mix_provided_and_defaulted() {
        let user_input = json!({"env": "production"});
        let mut schema = HashMap::new();
        schema.insert("env".to_string(), field("string", true, None));
        schema.insert(
            "retries".to_string(),
            field("number", false, Some(json!(3))),
        );
        schema.insert(
            "notify".to_string(),
            field("boolean", false, Some(json!(true))),
        );

        let result = merge_defaults(&user_input, &schema, &json!({})).unwrap();
        assert_eq!(result["env"], "production");
        assert_eq!(result["retries"], 3);
        assert_eq!(result["notify"], true);
    }

    // --- resolve_connection_inputs tests ---

    use crate::models::workflow::{ConnectionDef, ConnectionPropertyDef, ConnectionTypeDef};

    fn make_ws_with_connection() -> WorkspaceConfig {
        let mut ws = WorkspaceConfig::new();
        ws.connection_types.insert(
            "postgres".to_string(),
            ConnectionTypeDef {
                properties: {
                    let mut props = HashMap::new();
                    props.insert(
                        "host".to_string(),
                        ConnectionPropertyDef {
                            property_type: "string".to_string(),
                            required: true,
                            default: None,
                            secret: false,
                        },
                    );
                    props.insert(
                        "port".to_string(),
                        ConnectionPropertyDef {
                            property_type: "integer".to_string(),
                            required: false,
                            default: Some(json!(5432)),
                            secret: false,
                        },
                    );
                    props
                },
            },
        );
        ws.connections.insert(
            "prod_db".to_string(),
            ConnectionDef {
                connection_type: Some("postgres".to_string()),
                values: {
                    let mut v = HashMap::new();
                    v.insert("host".to_string(), json!("db.example.com"));
                    v.insert("port".to_string(), json!(5432));
                    v.insert("database".to_string(), json!("myapp"));
                    v
                },
            },
        );
        ws
    }

    #[test]
    fn test_resolve_connection_inputs_valid() {
        let ws = make_ws_with_connection();
        let input = json!({"db": "prod_db", "env": "production"});
        let mut schema = HashMap::new();
        schema.insert("db".to_string(), field("postgres", false, None));
        schema.insert("env".to_string(), field("string", false, None));

        let result = resolve_connection_inputs(&input, &schema, &ws).unwrap();
        // db should be resolved to the connection's values
        assert_eq!(result["db"]["host"], "db.example.com");
        assert_eq!(result["db"]["port"], 5432);
        assert_eq!(result["db"]["database"], "myapp");
        // env should pass through unchanged
        assert_eq!(result["env"], "production");
    }

    #[test]
    fn test_resolve_connection_inputs_missing_connection() {
        let ws = make_ws_with_connection();
        let input = json!({"db": "nonexistent"});
        let mut schema = HashMap::new();
        schema.insert("db".to_string(), field("postgres", false, None));

        let result = resolve_connection_inputs(&input, &schema, &ws);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("nonexistent"));
        assert!(err.contains("does not exist"));
    }

    #[test]
    fn test_resolve_connection_inputs_type_mismatch() {
        let mut ws = make_ws_with_connection();
        ws.connection_types.insert(
            "redis".to_string(),
            ConnectionTypeDef {
                properties: HashMap::new(),
            },
        );
        // prod_db is type: postgres, but schema expects redis
        let input = json!({"cache": "prod_db"});
        let mut schema = HashMap::new();
        schema.insert("cache".to_string(), field("redis", false, None));

        let result = resolve_connection_inputs(&input, &schema, &ws);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("expects type 'redis'"));
        assert!(err.contains("type 'postgres'"));
    }

    #[test]
    fn test_resolve_connection_inputs_primitives_passthrough() {
        let ws = WorkspaceConfig::new();
        let input = json!({"name": "alice", "count": 5});
        let mut schema = HashMap::new();
        schema.insert("name".to_string(), field("string", false, None));
        schema.insert("count".to_string(), field("integer", false, None));

        let result = resolve_connection_inputs(&input, &schema, &ws).unwrap();
        assert_eq!(result["name"], "alice");
        assert_eq!(result["count"], 5);
    }

    #[test]
    fn test_resolve_connection_inputs_text_type_is_primitive() {
        let ws = WorkspaceConfig::new();
        let input = json!({"query": "SELECT *\nFROM users\nWHERE active = true"});
        let mut schema = HashMap::new();
        schema.insert("query".to_string(), field("text", false, None));

        let result = resolve_connection_inputs(&input, &schema, &ws).unwrap();
        assert_eq!(result["query"], "SELECT *\nFROM users\nWHERE active = true");
    }

    #[test]
    fn test_resolve_connection_inputs_empty_input() {
        let ws = make_ws_with_connection();
        let input = json!({});
        let mut schema = HashMap::new();
        schema.insert("db".to_string(), field("postgres", false, None));

        // Missing field should be skipped (not an error)
        let result = resolve_connection_inputs(&input, &schema, &ws).unwrap();
        assert!(result.get("db").is_none());
    }

    #[test]
    fn test_resolve_connection_inputs_object_passthrough() {
        let ws = make_ws_with_connection();
        // Already an object (inline connection data) — should pass through
        let input = json!({"db": {"host": "inline.example.com", "port": 5433}});
        let mut schema = HashMap::new();
        schema.insert("db".to_string(), field("postgres", false, None));

        let result = resolve_connection_inputs(&input, &schema, &ws).unwrap();
        assert_eq!(result["db"]["host"], "inline.example.com");
        assert_eq!(result["db"]["port"], 5433);
    }

    #[test]
    fn test_render_input_map_preserves_connection_object() {
        // When a step input template references a connection object (e.g. {{ input.db }}),
        // Tera renders it as a JSON string. render_input_map should parse it back to
        // preserve the structured value, so downstream action spec templates can access
        // nested fields like {{ input.db.host }}.
        let mut input_map = HashMap::new();
        input_map.insert("db".to_string(), json!("{{ input.db }}"));
        input_map.insert("env".to_string(), json!("{{ input.env }}"));

        let context = json!({
            "input": {
                "db": {
                    "host": "db.example.com",
                    "port": 5432,
                    "database": "myapp"
                },
                "env": "production"
            }
        });

        let result = render_input_map(&input_map, &context).unwrap();
        // Connection object should be preserved as a structured value
        assert!(
            result["db"].is_object(),
            "db should be an object, got: {}",
            result["db"]
        );
        assert_eq!(result["db"]["host"], "db.example.com");
        assert_eq!(result["db"]["port"], 5432);
        assert_eq!(result["db"]["database"], "myapp");
        // Simple string should stay as string
        assert!(result["env"].is_string());
        assert_eq!(result["env"], "production");
    }

    #[test]
    fn test_render_input_map_preserves_array() {
        let mut input_map = HashMap::new();
        input_map.insert("tags".to_string(), json!("{{ input.tags }}"));

        let context = json!({
            "input": {
                "tags": ["web", "backend"]
            }
        });

        let result = render_input_map(&input_map, &context).unwrap();
        assert!(
            result["tags"].is_array(),
            "tags should be an array, got: {}",
            result["tags"]
        );
    }

    #[test]
    fn test_render_input_map_string_starting_with_brace_stays_string() {
        // A rendered string that starts with '{' but isn't valid JSON should stay as string
        let mut input_map = HashMap::new();
        input_map.insert("msg".to_string(), json!("{prefix} {{ input.name }}"));

        let context = json!({
            "input": {
                "name": "alice"
            }
        });

        let result = render_input_map(&input_map, &context).unwrap();
        assert!(result["msg"].is_string());
        assert_eq!(result["msg"], "{prefix} alice");
    }

    #[test]
    fn test_render_input_map_connection_then_action_spec() {
        // Full chain: resolved connection in job input → step input template → action spec context
        // Step 1: Simulate job input with resolved connection
        let job_input = json!({
            "db": {
                "host": "db.example.com",
                "port": 5432,
                "database": "myapp"
            },
            "env": "production"
        });

        // Step 2: Render step input template (flow_step.input)
        let mut step_input = HashMap::new();
        step_input.insert("db".to_string(), json!("{{ input.db }}"));
        step_input.insert("env".to_string(), json!("{{ input.env }}"));

        let step_context = json!({ "input": job_input });
        let rendered_step_input = render_input_map(&step_input, &step_context).unwrap();

        // Step 3: Build action spec context from rendered step input
        let spec_context = json!({ "input": rendered_step_input });

        // Step 4: Action cmd template can access nested fields
        let cmd = render_template(
            "migrate --host {{ input.db.host }} --port {{ input.db.port }} --env {{ input.env }}",
            &spec_context,
        )
        .unwrap();
        assert_eq!(
            cmd,
            "migrate --host db.example.com --port 5432 --env production"
        );
    }

    #[test]
    fn test_resolve_connection_inputs_non_string_non_object_value() {
        // Passing a number/boolean/array as connection input should error
        let ws = make_ws_with_connection();
        let input = json!({"db": 42});
        let mut schema = HashMap::new();
        schema.insert("db".to_string(), field("postgres", false, None));

        let result = resolve_connection_inputs(&input, &schema, &ws);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("expects a connection name"));
    }

    #[test]
    fn test_resolve_connection_inputs_untyped_connection() {
        let mut ws = WorkspaceConfig::new();
        ws.connection_types.insert(
            "custom".to_string(),
            ConnectionTypeDef {
                properties: HashMap::new(),
            },
        );
        ws.connections.insert(
            "my_api".to_string(),
            ConnectionDef {
                connection_type: None, // untyped
                values: {
                    let mut v = HashMap::new();
                    v.insert("url".to_string(), json!("https://api.example.com"));
                    v
                },
            },
        );

        let input = json!({"api": "my_api"});
        let mut schema = HashMap::new();
        schema.insert("api".to_string(), field("custom", false, None));

        // Untyped connection: no type mismatch check, just resolve
        let result = resolve_connection_inputs(&input, &schema, &ws).unwrap();
        assert_eq!(result["api"]["url"], "https://api.example.com");
    }

    // --- render_value_deep tests ---

    #[test]
    fn test_render_value_deep_string_with_template() {
        let value = json!("Hello {{ name }}");
        let context = json!({"name": "World"});
        let result = render_value_deep(&value, &context).unwrap();
        assert_eq!(result, json!("Hello World"));
    }

    #[test]
    fn test_render_value_deep_string_without_template() {
        let value = json!("plain text");
        let context = json!({});
        let result = render_value_deep(&value, &context).unwrap();
        assert_eq!(result, json!("plain text"));
    }

    #[test]
    fn test_render_value_deep_object_with_nested_templates() {
        let value = json!({
            "host": "{{ secret.db.host }}",
            "port": 8443,
            "nested": {
                "url": "https://{{ secret.db.host }}:8443"
            }
        });
        let context = json!({"secret": {"db": {"host": "clickhouse.example.com"}}});
        let result = render_value_deep(&value, &context).unwrap();
        assert_eq!(result["host"], "clickhouse.example.com");
        assert_eq!(result["port"], 8443);
        assert_eq!(
            result["nested"]["url"],
            "https://clickhouse.example.com:8443"
        );
    }

    #[test]
    fn test_render_value_deep_array_with_templates() {
        let value = json!(["{{ name }}", "plain", 42]);
        let context = json!({"name": "alice"});
        let result = render_value_deep(&value, &context).unwrap();
        assert_eq!(result, json!(["alice", "plain", 42]));
    }

    #[test]
    fn test_render_value_deep_primitives_passthrough() {
        let context = json!({});
        assert_eq!(render_value_deep(&json!(42), &context).unwrap(), json!(42));
        assert_eq!(
            render_value_deep(&json!(true), &context).unwrap(),
            json!(true)
        );
        assert_eq!(
            render_value_deep(&json!(null), &context).unwrap(),
            json!(null)
        );
        assert_eq!(
            render_value_deep(&json!(3.14), &context).unwrap(),
            json!(3.14)
        );
    }

    #[test]
    fn test_render_value_deep_error_propagates() {
        let value = json!({"key": "{{ missing.var }}"});
        let context = json!({});
        let result = render_value_deep(&value, &context);
        assert!(result.is_err());
    }

    // --- merge_action_defaults tests ---

    #[test]
    fn test_merge_action_defaults_object_default_with_templates() {
        let rendered_input = json!({"sql": "SELECT 1"});
        let mut schema = HashMap::new();
        schema.insert(
            "clickhouse".to_string(),
            field(
                "object",
                false,
                Some(json!({
                    "host": "{{ secret.clickhouse.host }}",
                    "port": 8443
                })),
            ),
        );
        schema.insert("sql".to_string(), field("string", true, None));

        let context = json!({"secret": {"clickhouse": {"host": "ch.example.com"}}});
        let result = merge_action_defaults(&rendered_input, &schema, &context).unwrap();
        assert_eq!(result["sql"], "SELECT 1");
        assert_eq!(result["clickhouse"]["host"], "ch.example.com");
        assert_eq!(result["clickhouse"]["port"], 8443);
    }

    #[test]
    fn test_merge_action_defaults_user_value_takes_precedence() {
        let rendered_input = json!({"clickhouse": {"host": "custom.host", "port": 9000}});
        let mut schema = HashMap::new();
        schema.insert(
            "clickhouse".to_string(),
            field(
                "object",
                false,
                Some(json!({
                    "host": "{{ secret.clickhouse.host }}",
                    "port": 8443
                })),
            ),
        );

        let context = json!({"secret": {"clickhouse": {"host": "ch.example.com"}}});
        let result = merge_action_defaults(&rendered_input, &schema, &context).unwrap();
        // User-provided value should win
        assert_eq!(result["clickhouse"]["host"], "custom.host");
        assert_eq!(result["clickhouse"]["port"], 9000);
    }

    #[test]
    fn test_merge_action_defaults_plain_string_default() {
        let rendered_input = json!({});
        let mut schema = HashMap::new();
        schema.insert(
            "env".to_string(),
            field("string", false, Some(json!("staging"))),
        );

        let context = json!({});
        let result = merge_action_defaults(&rendered_input, &schema, &context).unwrap();
        assert_eq!(result["env"], "staging");
    }

    #[test]
    fn test_merge_action_defaults_empty_schema_noop() {
        let rendered_input = json!({"key": "value"});
        let schema = HashMap::new();
        let context = json!({});
        let result = merge_action_defaults(&rendered_input, &schema, &context).unwrap();
        assert_eq!(result, json!({"key": "value"}));
    }

    #[test]
    fn test_merge_action_defaults_deeply_nested_templates() {
        let rendered_input = json!({});
        let mut schema = HashMap::new();
        schema.insert(
            "config".to_string(),
            field(
                "object",
                false,
                Some(json!({
                    "level1": {
                        "level2": {
                            "value": "{{ secret.deep }}"
                        }
                    }
                })),
            ),
        );

        let context = json!({"secret": {"deep": "resolved"}});
        let result = merge_action_defaults(&rendered_input, &schema, &context).unwrap();
        assert_eq!(result["config"]["level1"]["level2"]["value"], "resolved");
    }
}
