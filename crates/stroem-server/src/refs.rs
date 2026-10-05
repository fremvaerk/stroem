//! § 4.3 of the git-refs spec (`docs/superpowers/specs/2026-10-02-git-refs-design.md`):
//! which workspace owns a `ref:`'d name, and whether an un-ref'd name inherits
//! the pin of the config it is written in. Pure — no I/O, no manager.

use std::collections::HashSet;

use crate::workspace::pins::PinRef;

/// Whether a `ref:`'d name stays in the workspace it is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerKind {
    /// The owner is the base workspace itself (an own-workspace ref).
    Local,
    /// The owner is another configured workspace.
    CrossWorkspace,
}

/// The owner of a `ref:`'d name, decided syntactically (spec § 4.3 rules 1–2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefTarget {
    pub owner: String,
    /// The name inside the owner's config (the qualifier stripped).
    pub local_name: String,
    pub kind: OwnerKind,
}

/// An author mistake in a `ref:`'d reference — always a 400 (spec § 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefResolveError {
    /// `lib.item` + `ref`: libraries are server-level and have no ref.
    LibraryItem(String),
    /// `ws.item` + `ref` where `ws` is not configured.
    UnknownWorkspace(String),
    /// The owner is configured but not a git workspace.
    NotGit(String),
    /// A flow step's `ref:`'d action is `type: agent` (spec § 7.1).
    AgentAction(String),
}

impl std::fmt::Display for RefResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LibraryItem(name) => {
                write!(f, "'{name}' is a library item; library items have no ref")
            }
            Self::UnknownWorkspace(ws) => write!(f, "ref target: unknown workspace '{ws}'"),
            Self::NotGit(ws) => write!(
                f,
                "workspace '{ws}' is not a git workspace; `ref` needs a git owner"
            ),
            Self::AgentAction(name) => write!(
                f,
                "action '{name}': agent actions cannot be referenced with `ref` yet"
            ),
        }
    }
}

impl std::error::Error for RefResolveError {}

/// § 4.3 rules 1–2: the owner of a name written with `ref:` in `base_ws`.
/// Undotted (or degenerate, see `parse_qualified_ref`) → `base_ws`. Dotted →
/// a library prefix is refused, a configured workspace prefix owns it, anything
/// else is an unknown workspace. Deliberately does NOT require the name to exist
/// in the base config: a step may call an action that exists only on a release branch.
pub fn ref_owner(
    base_ws: &str,
    name: &str,
    library_names: &HashSet<String>,
    configured_ws: &HashSet<String>,
) -> Result<RefTarget, RefResolveError> {
    match stroem_common::template::parse_qualified_ref(name) {
        (None, local) => Ok(RefTarget {
            owner: base_ws.to_string(),
            local_name: local.to_string(),
            kind: OwnerKind::Local,
        }),
        (Some(prefix), rest) => {
            if library_names.contains(prefix) {
                return Err(RefResolveError::LibraryItem(name.to_string()));
            }
            if !configured_ws.contains(prefix) {
                return Err(RefResolveError::UnknownWorkspace(prefix.to_string()));
            }
            let kind = if prefix == base_ws {
                OwnerKind::Local
            } else {
                OwnerKind::CrossWorkspace
            };
            Ok(RefTarget {
                owner: prefix.to_string(),
                local_name: rest.to_string(),
                kind,
            })
        }
    }
}

/// What a reference resolves in: library prefixes, configured workspaces, and
/// which of them are git workspaces (only those can be pinned).
pub struct RefWorld<'a> {
    pub library_names: &'a HashSet<String>,
    pub configured: &'a HashSet<String>,
    pub git: &'a HashSet<String>,
}

/// How one reference resolves (spec § 4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefPlan {
    /// `ref:` present: look `target.local_name` up in `target.owner`@`git_ref`.
    AtRef { target: RefTarget, git_ref: String },
    /// No `ref:`: today's resolution. The result inherits `base_pin` iff it
    /// lands in the base workspace — decide with [`inherited_pin`].
    Today { base_pin: Option<PinRef> },
}

/// Plan the resolution of `name` written in a config of `base_ws`, which is
/// pinned at `base_pin` (or live when `None`), carrying `git_ref` if written.
pub fn plan_reference(
    base_ws: &str,
    base_pin: Option<&PinRef>,
    name: &str,
    git_ref: Option<&str>,
    world: &RefWorld<'_>,
) -> Result<RefPlan, RefResolveError> {
    match git_ref {
        Some(r) => {
            let target = ref_owner(base_ws, name, world.library_names, world.configured)?;
            if !world.git.contains(&target.owner) {
                return Err(RefResolveError::NotGit(target.owner));
            }
            Ok(RefPlan::AtRef {
                target,
                git_ref: r.to_string(),
            })
        }
        None => Ok(RefPlan::Today {
            base_pin: base_pin.cloned(),
        }),
    }
}

/// § 4.3 inheritance: a reference resolved WITHOUT `ref:` keeps the pin of the
/// config it is written in iff today's resolution put it in that config's own
/// workspace. A qualified `ws.name` that leaves the base stays live.
pub fn inherited_pin(
    base_ws: &str,
    base_pin: Option<&PinRef>,
    resolved_owner: &str,
) -> Option<PinRef> {
    if resolved_owner == base_ws {
        base_pin.cloned()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use crate::workspace::pins::PinRef;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    struct W {
        libs: HashSet<String>,
        configured: HashSet<String>,
        git: HashSet<String>,
    }

    impl W {
        fn new() -> Self {
            W {
                libs: set(&["common"]),
                configured: set(&["etl", "billing", "docs"]),
                // `docs` is a folder workspace: configured, not git.
                git: set(&["etl", "billing"]),
            }
        }
        fn world(&self) -> RefWorld<'_> {
            RefWorld {
                library_names: &self.libs,
                configured: &self.configured,
                git: &self.git,
            }
        }
    }

    fn pin() -> PinRef {
        PinRef {
            git_ref: "release/2.3".to_string(),
            commit: "a".repeat(40),
        }
    }

    #[test]
    fn unqualified_with_ref_targets_own_workspace() {
        let w = W::new();
        let plan = plan_reference("etl", None, "import", Some("release/2.3"), &w.world()).unwrap();
        assert_eq!(
            plan,
            RefPlan::AtRef {
                target: RefTarget {
                    owner: "etl".into(),
                    local_name: "import".into(),
                    kind: OwnerKind::Local,
                },
                git_ref: "release/2.3".into(),
            }
        );
    }

    #[test]
    fn qualified_with_ref_targets_named_workspace() {
        let w = W::new();
        let plan =
            plan_reference("etl", None, "billing.export", Some("v4.1.0"), &w.world()).unwrap();
        assert_eq!(
            plan,
            RefPlan::AtRef {
                target: RefTarget {
                    owner: "billing".into(),
                    local_name: "export".into(),
                    kind: OwnerKind::CrossWorkspace,
                },
                git_ref: "v4.1.0".into(),
            }
        );
    }

    #[test]
    fn self_qualified_with_ref_is_local_kind() {
        let w = W::new();
        let target = ref_owner("etl", "etl.import", &w.libs, &w.configured).unwrap();
        assert_eq!(target.owner, "etl");
        assert_eq!(target.local_name, "import");
        assert_eq!(target.kind, OwnerKind::Local);
    }

    #[test]
    fn library_item_with_ref_is_rejected() {
        let w = W::new();
        let err =
            plan_reference("etl", None, "common.slack", Some("main"), &w.world()).unwrap_err();
        assert_eq!(err, RefResolveError::LibraryItem("common.slack".into()));
        assert!(
            err.to_string().contains("library items have no ref"),
            "{err}"
        );
    }

    #[test]
    fn unknown_workspace_with_ref_is_rejected() {
        let w = W::new();
        let err = plan_reference("etl", None, "nope.x", Some("main"), &w.world()).unwrap_err();
        assert_eq!(err, RefResolveError::UnknownWorkspace("nope".into()));
        assert!(
            err.to_string().contains("unknown workspace 'nope'"),
            "{err}"
        );
    }

    #[test]
    fn folder_owner_with_ref_is_not_git() {
        let w = W::new();
        assert_eq!(
            plan_reference("etl", None, "docs.render", Some("main"), &w.world()).unwrap_err(),
            RefResolveError::NotGit("docs".into())
        );
        // An unqualified ref written IN a folder workspace targets that folder workspace.
        assert_eq!(
            plan_reference("docs", None, "render", Some("main"), &w.world()).unwrap_err(),
            RefResolveError::NotGit("docs".into())
        );
    }

    #[test]
    fn unqualified_without_ref_inside_pinned_config_inherits() {
        let w = W::new();
        let p = pin();
        let plan = plan_reference("etl", Some(&p), "nightly", None, &w.world()).unwrap();
        assert_eq!(
            plan,
            RefPlan::Today {
                base_pin: Some(p.clone())
            }
        );
        // Today's resolution lands in the base workspace → inherits.
        assert_eq!(inherited_pin("etl", Some(&p), "etl"), Some(p));
    }

    #[test]
    fn qualified_without_ref_inside_pinned_config_stays_live() {
        let p = pin();
        // `billing.nightly` written in etl@release/2.3 resolves to billing → live.
        assert_eq!(inherited_pin("etl", Some(&p), "billing"), None);
    }

    #[test]
    fn live_foreign_action_with_unqualified_task_stays_unpinned() {
        // A live foreign `billing.run` (type: task, task: nightly) inside a pinned
        // etl job: the task's base is billing's LIVE config, which has no pin.
        let w = W::new();
        let plan = plan_reference("billing", None, "nightly", None, &w.world()).unwrap();
        assert_eq!(plan, RefPlan::Today { base_pin: None });
        assert_eq!(inherited_pin("billing", None, "billing"), None);
    }

    #[test]
    fn degenerate_dotted_names_are_local() {
        let w = W::new();
        let target = ref_owner("etl", ".hidden", &w.libs, &w.configured).unwrap();
        assert_eq!(target.owner, "etl");
        assert_eq!(target.local_name, ".hidden");
    }
}
