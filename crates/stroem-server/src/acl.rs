use crate::config::{AclAction, AclConfig};
use std::collections::HashSet;

/// Task-level permission result
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPermission {
    Run,
    View,
    Deny,
}

impl From<AclAction> for TaskPermission {
    fn from(a: AclAction) -> Self {
        match a {
            AclAction::Run => TaskPermission::Run,
            AclAction::View => TaskPermission::View,
            AclAction::Deny => TaskPermission::Deny,
        }
    }
}

/// Result of enumerating allowed tasks for a user
pub enum AllowedScope {
    /// Admin or no ACL configured — everything is allowed
    All,
    /// Filtered list of (workspace, task_name, permission) tuples
    Filtered(Vec<(String, String, TaskPermission)>),
}

pub struct AclEvaluator {
    config: Option<AclConfig>,
}

impl AclEvaluator {
    pub fn new(config: Option<AclConfig>) -> Self {
        Self { config }
    }

    /// Whether ACL is configured
    pub fn is_configured(&self) -> bool {
        self.config.is_some()
    }

    /// Evaluate permission for a single task.
    /// `task_path` is the folder/task path for glob matching — if task has a folder,
    /// it's "{folder}/{task_name}", otherwise just "{task_name}".
    pub fn evaluate(
        &self,
        workspace: &str,
        task_path: &str,
        email: &str,
        groups: &HashSet<String>,
        is_admin: bool,
    ) -> TaskPermission {
        // Admin always has full access
        if is_admin {
            return TaskPermission::Run;
        }

        let config = match &self.config {
            Some(c) => c,
            // No ACL configured = backward compat, everyone gets Run
            None => return TaskPermission::Run,
        };

        let mut highest_priority: u8 = 0;

        for rule in &config.rules {
            // Check workspace match
            if !glob_match(&rule.workspace, workspace) {
                continue;
            }

            // Check task match (any task pattern must match)
            let task_matches = rule
                .tasks
                .iter()
                .any(|pattern| glob_match(pattern, task_path));
            if !task_matches {
                continue;
            }

            // Check user/group match (OR'd)
            let user_matches = rule.users.iter().any(|u| u == email);
            let group_matches = rule.groups.iter().any(|g| groups.contains(g));
            if !user_matches && !group_matches {
                continue;
            }

            // Track highest permission
            let priority = rule.action.priority();
            if priority > highest_priority {
                highest_priority = priority;
            }
        }

        if highest_priority > 0 {
            // At least one rule matched — return the highest permission
            match highest_priority {
                3 => TaskPermission::Run,
                2 => TaskPermission::View,
                _ => TaskPermission::Deny,
            }
        } else {
            // No rule matched — use default
            config.default.into()
        }
    }

    /// Build the allowed scope for a user across all workspaces/tasks.
    /// Returns `All` for admins or when no ACL is configured.
    pub fn allowed_scope(
        &self,
        workspace_tasks: &[(String, String, Option<String>)], // (workspace, task_name, folder)
        email: &str,
        groups: &HashSet<String>,
        is_admin: bool,
    ) -> AllowedScope {
        if is_admin || self.config.is_none() {
            return AllowedScope::All;
        }

        let mut result = Vec::new();
        for (ws, task_name, folder) in workspace_tasks {
            let task_path = make_task_path(folder.as_deref(), task_name);
            let perm = self.evaluate(ws, &task_path, email, groups, false);
            if !matches!(perm, TaskPermission::Deny) {
                result.push((ws.clone(), task_name.clone(), perm));
            }
        }
        AllowedScope::Filtered(result)
    }
}

/// Build the task path used for ACL glob matching.
/// If the task has a folder, returns "{folder}/{task_name}", otherwise just "{task_name}".
pub fn make_task_path(folder: Option<&str>, task_name: &str) -> String {
    match folder {
        Some(f) if !f.is_empty() => format!("{f}/{task_name}"),
        _ => task_name.to_string(),
    }
}

/// Simple glob matching supporting `*` as wildcard (including multiple wildcards).
/// - `*` alone matches everything
/// - `prefix/*` matches strings starting with prefix/
/// - `*suffix` matches strings ending with suffix
/// - `pre*suf` matches strings starting with pre and ending with suf
/// - `a/*/b*` handles multiple wildcards via recursive matching
/// - exact match otherwise
fn glob_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    match pattern.find('*') {
        None => pattern == value,
        Some(star_pos) => {
            let prefix = &pattern[..star_pos];
            let rest_pattern = &pattern[star_pos + 1..];

            if !value.starts_with(prefix) {
                return false;
            }

            let remaining = &value[prefix.len()..];

            // If no more wildcards in rest_pattern, match suffix directly
            if !rest_pattern.contains('*') {
                return remaining.ends_with(rest_pattern) && remaining.len() >= rest_pattern.len();
            }

            // Multiple wildcards: try matching rest_pattern at every position
            for i in 0..=remaining.len() {
                if glob_match(rest_pattern, &remaining[i..]) {
                    return true;
                }
            }
            false
        }
    }
}

/// Load the ACL context (admin flag + group memberships) for a user.
///
/// Returns `(is_admin, groups)`. Admins skip the group lookup.
#[tracing::instrument(skip(pool))]
pub async fn load_user_acl_context(
    pool: &sqlx::PgPool,
    user_id: uuid::Uuid,
    is_admin: bool,
) -> anyhow::Result<(bool, HashSet<String>)> {
    if is_admin {
        return Ok((true, HashSet::new()));
    }
    let groups = stroem_db::UserGroupRepo::get_groups_for_user(pool, user_id).await?;
    Ok((false, groups))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AclAction, AclConfig, AclRule};

    #[test]
    fn test_glob_match_exact() {
        assert!(glob_match("production", "production"));
        assert!(!glob_match("production", "staging"));
    }

    #[test]
    fn test_glob_match_wildcard_all() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn test_glob_match_prefix_wildcard() {
        assert!(glob_match("deploy/*", "deploy/web"));
        assert!(glob_match("deploy/*", "deploy/api"));
        assert!(!glob_match("deploy/*", "build/web"));
    }

    #[test]
    fn test_glob_match_suffix_wildcard() {
        assert!(glob_match("*-deploy", "web-deploy"));
        assert!(glob_match("*-deploy", "api-deploy"));
        assert!(!glob_match("*-deploy", "web-build"));
    }

    #[test]
    fn test_glob_match_middle_wildcard() {
        assert!(glob_match("pre*suf", "pre-middle-suf"));
        assert!(glob_match("pre*suf", "presuf"));
        assert!(!glob_match("pre*suf", "pre-middle-other"));
    }

    #[test]
    fn test_glob_match_no_match() {
        assert!(!glob_match("abc", "xyz"));
        assert!(!glob_match("abc*", "xyz"));
    }

    #[test]
    fn test_evaluate_no_config_returns_run() {
        let acl = AclEvaluator::new(None);
        let groups = HashSet::new();
        assert_eq!(
            acl.evaluate("production", "deploy", "user@example.com", &groups, false),
            TaskPermission::Run
        );
    }

    #[test]
    fn test_evaluate_admin_bypass() {
        let config = AclConfig {
            default: AclAction::Deny,
            rules: vec![],
        };
        let acl = AclEvaluator::new(Some(config));
        let groups = HashSet::new();
        assert_eq!(
            acl.evaluate("production", "deploy", "admin@example.com", &groups, true),
            TaskPermission::Run
        );
    }

    #[test]
    fn test_evaluate_default_deny_no_matching_rules() {
        let config = AclConfig {
            default: AclAction::Deny,
            rules: vec![AclRule {
                workspace: "staging".to_string(),
                tasks: vec!["*".to_string()],
                action: AclAction::Run,
                groups: vec![],
                users: vec!["other@example.com".to_string()],
            }],
        };
        let acl = AclEvaluator::new(Some(config));
        let groups = HashSet::new();
        assert_eq!(
            acl.evaluate("production", "deploy", "user@example.com", &groups, false),
            TaskPermission::Deny
        );
    }

    #[test]
    fn test_evaluate_email_match() {
        let config = AclConfig {
            default: AclAction::Deny,
            rules: vec![AclRule {
                workspace: "*".to_string(),
                tasks: vec!["*".to_string()],
                action: AclAction::Run,
                groups: vec![],
                users: vec!["dev@example.com".to_string()],
            }],
        };
        let acl = AclEvaluator::new(Some(config));
        let groups = HashSet::new();
        assert_eq!(
            acl.evaluate("production", "deploy", "dev@example.com", &groups, false),
            TaskPermission::Run
        );
    }

    #[test]
    fn test_evaluate_group_match() {
        let config = AclConfig {
            default: AclAction::Deny,
            rules: vec![AclRule {
                workspace: "*".to_string(),
                tasks: vec!["*".to_string()],
                action: AclAction::View,
                groups: vec!["engineering".to_string()],
                users: vec![],
            }],
        };
        let acl = AclEvaluator::new(Some(config));
        let mut groups = HashSet::new();
        groups.insert("engineering".to_string());
        assert_eq!(
            acl.evaluate("production", "deploy", "user@example.com", &groups, false),
            TaskPermission::View
        );
    }

    #[test]
    fn test_evaluate_highest_wins() {
        // Two rules match: one gives View, one gives Run. Run should win.
        let config = AclConfig {
            default: AclAction::Deny,
            rules: vec![
                AclRule {
                    workspace: "*".to_string(),
                    tasks: vec!["*".to_string()],
                    action: AclAction::View,
                    groups: vec!["engineering".to_string()],
                    users: vec![],
                },
                AclRule {
                    workspace: "production".to_string(),
                    tasks: vec!["deploy/*".to_string()],
                    action: AclAction::Run,
                    groups: vec!["devops".to_string()],
                    users: vec![],
                },
            ],
        };
        let acl = AclEvaluator::new(Some(config));
        let mut groups = HashSet::new();
        groups.insert("engineering".to_string());
        groups.insert("devops".to_string());
        assert_eq!(
            acl.evaluate(
                "production",
                "deploy/web",
                "user@example.com",
                &groups,
                false
            ),
            TaskPermission::Run
        );
    }

    #[test]
    fn test_evaluate_folder_task_path() {
        let config = AclConfig {
            default: AclAction::Deny,
            rules: vec![AclRule {
                workspace: "*".to_string(),
                tasks: vec!["infra/*".to_string()],
                action: AclAction::Run,
                groups: vec![],
                users: vec!["user@example.com".to_string()],
            }],
        };
        let acl = AclEvaluator::new(Some(config));
        let groups = HashSet::new();

        let path = make_task_path(Some("infra"), "deploy");
        assert_eq!(
            acl.evaluate("prod", &path, "user@example.com", &groups, false),
            TaskPermission::Run
        );

        let path2 = make_task_path(Some("other"), "deploy");
        assert_eq!(
            acl.evaluate("prod", &path2, "user@example.com", &groups, false),
            TaskPermission::Deny
        );
    }

    #[test]
    fn test_evaluate_default_view() {
        let config = AclConfig {
            default: AclAction::View,
            rules: vec![],
        };
        let acl = AclEvaluator::new(Some(config));
        let groups = HashSet::new();
        assert_eq!(
            acl.evaluate("prod", "deploy", "user@example.com", &groups, false),
            TaskPermission::View
        );
    }

    #[test]
    fn test_is_configured() {
        assert!(!AclEvaluator::new(None).is_configured());
        assert!(AclEvaluator::new(Some(AclConfig {
            default: AclAction::Deny,
            rules: vec![]
        }))
        .is_configured());
    }

    #[test]
    fn test_make_task_path() {
        assert_eq!(make_task_path(None, "deploy"), "deploy");
        assert_eq!(make_task_path(Some(""), "deploy"), "deploy");
        assert_eq!(make_task_path(Some("infra"), "deploy"), "infra/deploy");
    }

    #[test]
    fn test_allowed_scope_admin() {
        let acl = AclEvaluator::new(Some(AclConfig {
            default: AclAction::Deny,
            rules: vec![],
        }));
        let groups = HashSet::new();
        let tasks = vec![("ws".to_string(), "task1".to_string(), None)];
        assert!(matches!(
            acl.allowed_scope(&tasks, "admin@example.com", &groups, true),
            AllowedScope::All
        ));
    }

    #[test]
    fn test_allowed_scope_no_config() {
        let acl = AclEvaluator::new(None);
        let groups = HashSet::new();
        let tasks = vec![("ws".to_string(), "task1".to_string(), None)];
        assert!(matches!(
            acl.allowed_scope(&tasks, "user@example.com", &groups, false),
            AllowedScope::All
        ));
    }

    #[test]
    fn test_allowed_scope_filtered() {
        let config = AclConfig {
            default: AclAction::Deny,
            rules: vec![AclRule {
                workspace: "*".to_string(),
                tasks: vec!["visible*".to_string()],
                action: AclAction::View,
                groups: vec![],
                users: vec!["user@example.com".to_string()],
            }],
        };
        let acl = AclEvaluator::new(Some(config));
        let groups = HashSet::new();
        let tasks = vec![
            ("ws".to_string(), "visible-task".to_string(), None),
            ("ws".to_string(), "hidden-task".to_string(), None),
        ];
        match acl.allowed_scope(&tasks, "user@example.com", &groups, false) {
            AllowedScope::Filtered(items) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].1, "visible-task");
                assert_eq!(items[0].2, TaskPermission::View);
            }
            AllowedScope::All => panic!("expected Filtered"),
        }
    }

    #[test]
    fn test_glob_match_multiple_wildcards() {
        assert!(glob_match("deploy/*/prod*", "deploy/web/production"));
        assert!(glob_match("*/api/*", "staging/api/v2"));
        assert!(!glob_match("*/api/*", "staging/web/v2"));
        assert!(glob_match("*/*", "a/b"));
        assert!(!glob_match("*/*", "abc"));
    }
}
