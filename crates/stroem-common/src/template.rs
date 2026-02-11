use anyhow::{Context, Result};
use std::collections::HashMap;
use tera::Tera;

/// Renders a single Tera template string against a JSON context
pub fn render_template(template: &str, context: &serde_json::Value) -> Result<String> {
    let mut tera = Tera::default();
    let template_name = "__template__";

    tera.add_raw_template(template_name, template)
        .context("Failed to parse template")?;

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
    fn test_render_string_opt_none() {
        let template: Option<String> = None;
        let context = json!({});
        let result = render_string_opt(&template, &context).unwrap();
        assert_eq!(result, None);
    }
}
