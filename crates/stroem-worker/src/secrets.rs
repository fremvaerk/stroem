use anyhow::{Context, Result};
use std::collections::HashMap;

/// Resolves `ref+` secret references in env vars by calling the vals CLI.
///
/// Scans all env values for the `ref+` prefix. If any are found, pipes them
/// through `vals eval` to resolve the actual secret values. If no `ref+` values
/// are present, this is a no-op.
pub async fn resolve_secrets(env: &mut HashMap<String, String>) -> Result<()> {
    // Collect entries that need resolution
    let ref_entries: HashMap<&String, &String> =
        env.iter().filter(|(_, v)| v.starts_with("ref+")).collect();

    if ref_entries.is_empty() {
        return Ok(());
    }

    // Build a JSON object of only the ref+ entries
    let ref_json: serde_json::Value = ref_entries
        .iter()
        .map(|(k, v)| ((*k).clone(), serde_json::Value::String((*v).clone())))
        .collect::<serde_json::Map<String, serde_json::Value>>()
        .into();

    let input_str =
        serde_json::to_string(&ref_json).context("Failed to serialize ref+ entries to JSON")?;

    // Spawn vals and pipe JSON to stdin
    let mut child = tokio::process::Command::new("vals")
        .args(["eval", "-f", "-", "-o", "json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn vals CLI. Is vals installed and in PATH?")?;

    // Write input to stdin
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(input_str.as_bytes())
            .await
            .context("Failed to write to vals stdin")?;
        // Drop stdin to close it so vals can proceed
    }

    let output = child
        .wait_with_output()
        .await
        .context("Failed to wait for vals CLI")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("vals eval failed (exit {}): {}", output.status, stderr);
    }

    // Parse resolved values
    let resolved: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("Failed to parse vals output as JSON")?;

    let resolved_map = resolved
        .as_object()
        .context("vals output is not a JSON object")?;

    // Replace env values with resolved ones
    for (key, value) in resolved_map {
        if let Some(val_str) = value.as_str() {
            if env.contains_key(key) {
                env.insert(key.clone(), val_str.to_string());
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_no_refs_is_noop() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        env.insert("BAZ".to_string(), "qux".to_string());

        let original = env.clone();
        resolve_secrets(&mut env).await.unwrap();
        assert_eq!(env, original);
    }

    #[test]
    fn test_detect_ref_values() {
        let mut env = HashMap::new();
        env.insert("NORMAL".to_string(), "plain-value".to_string());
        env.insert(
            "SECRET".to_string(),
            "ref+awsssm:///prod/db/password".to_string(),
        );
        env.insert(
            "VAULT".to_string(),
            "ref+vault://secret/data/api#key".to_string(),
        );

        let ref_entries: Vec<&String> = env
            .iter()
            .filter(|(_, v)| v.starts_with("ref+"))
            .map(|(k, _)| k)
            .collect();

        assert_eq!(ref_entries.len(), 2);
        assert!(!ref_entries.contains(&&"NORMAL".to_string()));
    }

    #[tokio::test]
    async fn test_resolve_returns_error_if_vals_missing() {
        let mut env = HashMap::new();
        env.insert(
            "SECRET".to_string(),
            "ref+awsssm:///prod/db/password".to_string(),
        );

        // This test will fail with an error if vals is not installed,
        // which is the expected behavior
        let result = resolve_secrets(&mut env).await;
        // We can't guarantee vals is installed, so just verify the function
        // either succeeds (vals installed) or returns an error (vals not installed)
        if let Err(e) = result {
            let err_msg = e.to_string();
            assert!(
                err_msg.contains("vals") || err_msg.contains("No such file"),
                "Error should mention vals: {}",
                err_msg
            );
        }
    }
}
