//! Templated refs (spec `docs/superpowers/specs/2026-10-15-templated-refs-design.md`
//! § 5.5 "Changes" 3): the ONE error a resolution of a rendered ref leaves
//! with. A rendered value is private until it passes both secret checks
//! (§ 10.3), so the error is classified by TYPE from whatever failed and
//! carries a fixed sentence per kind, the owner workspace (a configured
//! name), and — for a render error only — the value-free `TemplateError`
//! text. Never the value, never the replaced error: it is a leaf with no
//! `source()`, because `AppError::Internal` and the webhook handler print
//! `{:#}` chains and the replaced error can quote the value.

use std::fmt;

use stroem_common::git_ref::{GitRefError, RenderedRefRejection};
use stroem_common::template_error::TemplateError;

use crate::job_creator::MissingAtRef;
use crate::refs::RefResolveError;
use crate::workspace::pins::{PinError, PinLoadWithheld};

/// Why a rendered ref could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderedRefKind {
    Empty,
    Invalid,
    SecretValue,
    NotFound {
        short_sha: bool,
    },
    NotGit,
    NoAction,
    AgentAction,
    LoadWithheld,
    /// Transient (D7: it still fails the step; `classify_execute_error` → 500).
    Unavailable,
    Render,
    Unresolvable,
}

/// The value-free error of a templated-ref resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedRefError {
    pub kind: RenderedRefKind,
    /// The owner workspace the ref names (a configured name — an identifier).
    pub workspace: String,
    /// `Render` only: the `TemplateError` text (category + position).
    detail: Option<String>,
}

impl RenderedRefError {
    pub fn new(kind: RenderedRefKind, workspace: &str) -> Self {
        Self {
            kind,
            workspace: workspace.to_string(),
            detail: None,
        }
    }

    pub fn from_rejection(r: RenderedRefRejection, workspace: &str) -> Self {
        let kind = match r {
            RenderedRefRejection::Empty => RenderedRefKind::Empty,
            RenderedRefRejection::SecretValue => RenderedRefKind::SecretValue,
            RenderedRefRejection::Invalid => RenderedRefKind::Invalid,
        };
        Self::new(kind, workspace)
    }

    /// Classify `err` by type. `err` itself is dropped: its text may carry
    /// the rendered value.
    pub fn from_error(err: &anyhow::Error, workspace: &str) -> Self {
        if let Some(t) = err.downcast_ref::<TemplateError>() {
            return Self {
                kind: RenderedRefKind::Render,
                workspace: workspace.to_string(),
                detail: Some(t.to_string()),
            };
        }
        Self::new(classify(err), workspace)
    }
}

fn classify(err: &anyhow::Error) -> RenderedRefKind {
    if let Some(p) = err.downcast_ref::<PinError>() {
        return match p {
            PinError::NotGit { .. } => RenderedRefKind::NotGit,
            PinError::RefNotFound { git_ref, .. } => RenderedRefKind::NotFound {
                short_sha: stroem_common::git_ref::short_sha_hint(git_ref).is_some(),
            },
            PinError::CommitNotFound { .. } => RenderedRefKind::NotFound { short_sha: false },
            PinError::PinLoadFailed { .. } => RenderedRefKind::LoadWithheld,
            PinError::PinUnavailable { .. } => RenderedRefKind::Unavailable,
        };
    }
    if err.downcast_ref::<PinLoadWithheld>().is_some() {
        return RenderedRefKind::LoadWithheld;
    }
    if let Some(r) = err.downcast_ref::<RefResolveError>() {
        return match r {
            RefResolveError::NotGit(_) => RenderedRefKind::NotGit,
            RefResolveError::AgentAction(_) => RenderedRefKind::AgentAction,
            RefResolveError::LibraryItem(_) | RefResolveError::UnknownWorkspace(_) => {
                RenderedRefKind::Unresolvable
            }
        };
    }
    if err.downcast_ref::<GitRefError>().is_some() {
        return RenderedRefKind::Invalid;
    }
    if err.downcast_ref::<MissingAtRef>().is_some() {
        return RenderedRefKind::NoAction;
    }
    RenderedRefKind::Unresolvable
}

impl fmt::Display for RenderedRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ws = &self.workspace;
        match self.kind {
            RenderedRefKind::Empty => f.write_str("the rendered ref is empty"),
            RenderedRefKind::Invalid => {
                f.write_str("the rendered ref is not a valid branch, tag or full commit SHA")
            }
            RenderedRefKind::SecretValue => f.write_str("the rendered ref contains a secret value"),
            RenderedRefKind::NotFound { short_sha } => {
                write!(f, "the rendered ref was not found in workspace '{ws}'")?;
                if short_sha {
                    f.write_str(
                        " (short commit SHAs are not supported; use the full 40-character SHA)",
                    )?;
                }
                Ok(())
            }
            RenderedRefKind::NotGit => {
                write!(f, "workspace '{ws}' is not a git workspace; `ref` needs a git owner")
            }
            RenderedRefKind::NoAction => {
                write!(f, "workspace '{ws}' has no such action or task at the rendered ref")
            }
            RenderedRefKind::AgentAction => f.write_str(
                "the action at the rendered ref is an agent action; agent actions cannot be referenced with `ref` yet",
            ),
            RenderedRefKind::LoadWithheld => write!(
                f,
                "workspace '{ws}' at the rendered ref cannot be loaded: its configuration does not load"
            ),
            RenderedRefKind::Unavailable => {
                write!(f, "workspace '{ws}' is not available at the rendered ref yet")
            }
            RenderedRefKind::Render => match &self.detail {
                Some(d) => write!(f, "the ref template could not be rendered: {d}"),
                None => f.write_str("the ref template could not be rendered"),
            },
            RenderedRefKind::Unresolvable => {
                write!(f, "the rendered ref could not be resolved in workspace '{ws}'")
            }
        }
    }
}

impl std::error::Error for RenderedRefError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::RefResolveError;
    use crate::workspace::pins::{PinError, PinLoadWithheld, PinRef};

    const VALUE: &str = "feature/zz-private-4711";

    fn kind_of(err: anyhow::Error) -> RenderedRefKind {
        RenderedRefError::from_error(&err, "etl").kind
    }

    #[test]
    fn errors_are_classified_by_type() {
        let ws = "etl".to_string();
        assert_eq!(
            kind_of(
                PinError::RefNotFound {
                    workspace: ws.clone(),
                    git_ref: VALUE.into()
                }
                .into()
            ),
            RenderedRefKind::NotFound { short_sha: false }
        );
        assert_eq!(
            kind_of(
                PinError::RefNotFound {
                    workspace: ws.clone(),
                    git_ref: "3f2a9c0".into()
                }
                .into()
            ),
            RenderedRefKind::NotFound { short_sha: true }
        );
        assert_eq!(
            kind_of(
                PinError::PinUnavailable {
                    workspace: ws.clone(),
                    message: format!("ref '{VALUE}' x")
                }
                .into()
            ),
            RenderedRefKind::Unavailable
        );
        assert_eq!(
            kind_of(
                PinError::PinLoadFailed {
                    workspace: ws.clone(),
                    commit: "c".into(),
                    message: "m".into()
                }
                .into()
            ),
            RenderedRefKind::LoadWithheld
        );
        assert_eq!(
            kind_of(
                PinError::NotGit {
                    workspace: ws.clone()
                }
                .into()
            ),
            RenderedRefKind::NotGit
        );
        let pin = PinRef {
            git_ref: VALUE.into(),
            commit: "3f2a9c0e".into(),
        };
        assert_eq!(
            kind_of(PinLoadWithheld::new("etl", &pin).into()),
            RenderedRefKind::LoadWithheld
        );
        assert_eq!(
            kind_of(RefResolveError::AgentAction("ask".into()).into()),
            RenderedRefKind::AgentAction
        );
        assert_eq!(
            kind_of(RefResolveError::NotGit("docs".into()).into()),
            RenderedRefKind::NotGit
        );
        assert_eq!(
            kind_of(
                anyhow::Error::new(crate::job_creator::MissingAtRef::new_for_test())
                    .context("outer")
            ),
            RenderedRefKind::NoAction
        );
        assert_eq!(
            kind_of(anyhow::anyhow!("anything {VALUE}")),
            RenderedRefKind::Unresolvable
        );
        let te = stroem_common::template::render_ref_template("{{ nope }}", &serde_json::json!({}))
            .unwrap_err();
        let e = RenderedRefError::from_error(&te, "etl");
        assert_eq!(e.kind, RenderedRefKind::Render);
        assert!(
            e.to_string()
                .starts_with("the ref template could not be rendered: "),
            "{e}"
        );
    }

    #[test]
    fn no_message_carries_the_value_and_none_has_a_source() {
        let errors: Vec<anyhow::Error> = vec![
            PinError::RefNotFound {
                workspace: "etl".into(),
                git_ref: VALUE.into(),
            }
            .into(),
            PinError::PinUnavailable {
                workspace: "etl".into(),
                message: format!("ref '{VALUE}' is not in the last listing"),
            }
            .into(),
            anyhow::anyhow!("whatever {VALUE}"),
        ];
        for err in errors {
            let e = RenderedRefError::from_error(&err, "etl");
            let shown = format!("{:#}", anyhow::Error::new(e.clone()));
            assert!(!shown.contains("zz-private"), "{shown}");
            assert!(std::error::Error::source(&e).is_none());
        }
        assert_eq!(
            RenderedRefError::new(RenderedRefKind::NotFound { short_sha: false }, "billing")
                .to_string(),
            "the rendered ref was not found in workspace 'billing'"
        );
    }

    #[test]
    fn rejections_map_one_to_one() {
        use stroem_common::git_ref::RenderedRefRejection as R;
        assert_eq!(
            RenderedRefError::from_rejection(R::Empty, "etl").kind,
            RenderedRefKind::Empty
        );
        assert_eq!(
            RenderedRefError::from_rejection(R::SecretValue, "etl").kind,
            RenderedRefKind::SecretValue
        );
        assert_eq!(
            RenderedRefError::from_rejection(R::Invalid, "etl").kind,
            RenderedRefKind::Invalid
        );
        assert_eq!(
            RenderedRefError::from_rejection(R::Empty, "etl").to_string(),
            "the rendered ref is empty"
        );
    }
}
