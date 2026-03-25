use anyhow::Result;
use stroem_common::models::workflow::WorkspaceConfig;

fn truncate_desc(s: &str, max_len: usize) -> String {
    let clean = s.replace(['\n', '\r'], " ");
    if clean.len() <= max_len {
        return clean;
    }
    let suffix_len = 3; // "..."
    let target = max_len - suffix_len;
    let end = clean
        .char_indices()
        .take_while(|(i, _)| *i <= target)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(target);
    format!("{}...", &clean[..end])
}

pub fn cmd_actions(config: &WorkspaceConfig) -> Result<()> {
    if config.actions.is_empty() {
        println!("No actions found.");
        return Ok(());
    }

    let mut rows: Vec<(&str, &str, &str, &str, &str)> = config
        .actions
        .iter()
        .map(|(name, action)| {
            (
                name.as_str(),
                action.action_type.as_str(),
                action.runner.as_deref().unwrap_or("-"),
                action.language.as_deref().unwrap_or("-"),
                action.description.as_deref().unwrap_or(""),
            )
        })
        .collect();
    rows.sort_by_key(|r| r.0);

    println!(
        "{:<30} {:<10} {:<10} {:<12} DESCRIPTION",
        "NAME", "TYPE", "RUNNER", "LANGUAGE"
    );
    println!("{}", "-".repeat(85));
    for (name, action_type, runner, language, desc) in &rows {
        let desc_truncated = truncate_desc(desc, 30);
        println!(
            "{:<30} {:<10} {:<10} {:<12} {}",
            name, action_type, runner, language, desc_truncated
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use stroem_common::models::workflow::ActionDef;

    fn make_action(action_type: &str, runner: Option<&str>, language: Option<&str>) -> ActionDef {
        ActionDef {
            action_type: action_type.to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("echo hello".to_string()),
            source: None,
            runner: runner.map(|s| s.to_string()),
            language: language.map(|s| s.to_string()),
            dependencies: vec![],
            interpreter: None,
            tags: vec![],
            image: None,
            command: None,
            entrypoint: None,
            env: None,
            workdir: None,
            resources: None,
            input: HashMap::new(),
            output: None,
            manifest: None,
            provider: None,
            model: None,
            system_prompt: None,
            prompt: None,
            temperature: None,
            max_tokens: None,
            tools: vec![],
            max_turns: None,
            interactive: false,
            message: None,
        }
    }

    #[test]
    fn actions_empty_workspace() {
        let config = WorkspaceConfig::new();
        assert!(cmd_actions(&config).is_ok());
    }

    #[test]
    fn actions_lists_all() {
        let mut config = WorkspaceConfig::new();
        config.actions.insert(
            "greet".to_string(),
            make_action("script", None, Some("shell")),
        );
        config
            .actions
            .insert("build".to_string(), make_action("docker", None, None));
        assert!(cmd_actions(&config).is_ok());
    }

    #[test]
    fn truncate_desc_at_boundary_30() {
        let s = "a".repeat(30);
        assert_eq!(truncate_desc(&s, 30), s);
    }

    #[test]
    fn truncate_desc_over_boundary_30() {
        // target = 30 - 3 = 27; take_while i <= 27 keeps the char at index 27
        // (28th char), so 28 chars are kept before "...".
        let s = "a".repeat(31);
        let result = truncate_desc(&s, 30);
        assert_eq!(result, format!("{}...", "a".repeat(28)));
    }

    #[test]
    fn truncate_desc_multibyte_safe() {
        // Place multi-byte chars right at the truncation boundary so the
        // implementation must snap back to a valid char boundary.
        let s = "aaaaaaaaaaaaaaaaaaaaaaaaaaa日本語xyz"; // 27 ASCII + multi-byte tail
        let result = truncate_desc(s, 30);
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
        assert!(result.len() <= 33); // at most 30 chars + "..." (3 bytes)
    }

    #[test]
    fn actions_no_runner_no_language() {
        let mut config = WorkspaceConfig::new();
        config
            .actions
            .insert("bare".to_string(), make_action("script", None, None));
        assert!(cmd_actions(&config).is_ok());
    }

    #[test]
    fn actions_agent_type() {
        let mut config = WorkspaceConfig::new();
        config
            .actions
            .insert("ask-llm".to_string(), make_action("agent", None, None));
        assert!(cmd_actions(&config).is_ok());
    }
}
