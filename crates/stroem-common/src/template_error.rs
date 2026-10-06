//! Value-free template errors (spec 2026-10-06 § 3.2).
//!
//! Every Tera error is converted here. The message is assembled by us from a
//! category, a position and (Task 2) names drawn from closed sets — Tera's own
//! text, which can quote context values and template source, is kept only for
//! [`TemplateError::raw_detail`], which only `stroem-cli` may call.

use std::fmt;

/// How a `vals` call failed (recorded by the filter itself, spec § 3.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValsFailureKind {
    /// `vals eval` exited unsuccessfully; `None` when killed by a signal.
    Exited(Option<i32>),
    /// The load budget's deadline passed while `vals` ran.
    TimedOut,
    /// The `vals` binary could not be started.
    SpawnFailed,
    /// `vals` succeeded but its output was not the expected JSON.
    BadOutput,
}

/// A `vals` failure. `stderr` is arbitrary subprocess output: it is shown
/// only by [`TemplateError::raw_detail`].
#[derive(Clone)]
pub struct ValsFailure {
    pub kind: ValsFailureKind,
    pub stderr: String,
}

pub struct TemplateError {
    message: String,
    line: Option<usize>,
    column: Option<usize>,
    vals: Option<ValsFailure>,
    raw: String,
}

impl TemplateError {
    pub(crate) fn from_tera(
        err: &tera::Error,
        src: Option<&str>,
        vals: Option<ValsFailure>,
    ) -> Self {
        let _ = src; // used by Task 2's filter-name enrichment
        let (category, span) = match err.kind() {
            tera::ErrorKind::SyntaxError(r) => ("template syntax error", Some(r.span().clone())),
            tera::ErrorKind::RenderingError(r) => {
                ("template rendering failed", Some(r.span().clone()))
            }
            tera::ErrorKind::Msg(text) => (msg_category(text), None),
            tera::ErrorKind::InvalidArgument { .. } => {
                ("a filter received a value of the wrong type", None)
            }
            tera::ErrorKind::MissingArgument { .. } => {
                ("a filter call is missing a required argument", None)
            }
            tera::ErrorKind::OutOfRangeArgument { .. } => ("a number is out of range", None),
            _ => ("template rendering failed", None),
        };
        let message = match &vals {
            Some(v) => vals_category(v.kind),
            None => category.to_string(),
        };
        TemplateError {
            message,
            line: span.as_ref().map(|s| s.start_line),
            column: span.as_ref().map(|s| s.start_col + 1),
            vals,
            raw: err.to_string(),
        }
    }

    /// The value-free message, without position.
    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn is_vals_failure(&self) -> bool {
        self.vals.is_some()
    }

    pub fn vals_failure_kind(&self) -> Option<ValsFailureKind> {
        self.vals.as_ref().map(|v| v.kind)
    }

    /// Tera's original report plus any `vals` stderr. It contains template
    /// source and may contain context values. ONLY `stroem-cli` may call
    /// this (enforced by a test in Task 8).
    pub fn raw_detail(&self) -> String {
        match &self.vals {
            Some(v) if !v.stderr.trim().is_empty() => {
                format!("{}\nvals stderr:\n{}", self.raw, v.stderr.trim())
            }
            _ => self.raw.clone(),
        }
    }
}

fn msg_category(text: &str) -> &'static str {
    let first = text.lines().next().unwrap_or("");
    let first = first.strip_prefix("error: ").unwrap_or(first);
    if first.starts_with("Unknown filter `") {
        "template uses an unknown filter"
    } else if first.starts_with("Unknown test `") {
        "template uses an unknown test"
    } else if first.starts_with("Unknown function `") {
        "template uses an unknown function"
    } else if first.starts_with("Template inheritance ({% extends %}) is not supported") {
        "{% extends %} is not supported"
    } else if first.starts_with("Blocks not supported") {
        "{% block %} is not supported"
    } else {
        "template could not be compiled"
    }
}

fn vals_category(kind: ValsFailureKind) -> String {
    match kind {
        ValsFailureKind::Exited(Some(code)) => format!("vals failed (exit status {code})"),
        ValsFailureKind::Exited(None) => "vals failed (killed by a signal)".to_string(),
        ValsFailureKind::TimedOut => "vals timed out".to_string(),
        ValsFailureKind::SpawnFailed => "vals could not be started".to_string(),
        ValsFailureKind::BadOutput => "vals returned invalid output".to_string(),
    }
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.line, self.column) {
            (Some(l), Some(c)) => write!(f, "{} (line {l}, column {c})", self.message),
            _ => write!(f, "{}", self.message),
        }
    }
}

impl fmt::Debug for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TemplateError")
            .field("message", &self.message)
            .field("line", &self.line)
            .field("column", &self.column)
            .field("vals", &self.vals.as_ref().map(|v| v.kind))
            .finish()
    }
}

impl std::error::Error for TemplateError {}

#[cfg(test)]
mod tests {
    use crate::template::render_template;
    use serde_json::json;

    const CONTEXT_CANARY: &str = "ctx-canary-7f3a";
    const SOURCE_CANARY: &str = "src-canary-91be";

    fn template_error(err: &anyhow::Error) -> &super::TemplateError {
        err.chain()
            .find_map(|c| c.downcast_ref::<super::TemplateError>())
            .expect("chain holds a TemplateError")
    }

    #[test]
    fn render_error_carries_category_and_position_only() {
        let ctx = json!({ "secret": { "X": CONTEXT_CANARY } });
        let tpl = format!("{{{{ secret.X | upper | int }}}} {SOURCE_CANARY}");
        let err = render_template(&tpl, &ctx).unwrap_err();
        let te = template_error(&err);
        assert!(te.raw_detail().contains(&CONTEXT_CANARY.to_uppercase()));
        for text in [format!("{err:#}"), format!("{err:?}"), te.to_string()] {
            assert!(!text.contains(CONTEXT_CANARY), "{text}");
            assert!(!text.contains(&CONTEXT_CANARY.to_uppercase()), "{text}");
            assert!(!text.contains(SOURCE_CANARY), "{text}");
        }
        assert_eq!(te.message(), "template rendering failed");
        assert!(te.to_string().contains("(line 1, column"), "{te}");
    }

    #[test]
    fn syntax_error_carries_no_source() {
        let tpl = format!("{{{{ 'x' + }}}} {SOURCE_CANARY}");
        let err = render_template(&tpl, &json!({})).unwrap_err();
        let text = format!("{err:#} {err:?}");
        assert!(!text.contains(SOURCE_CANARY), "{text}");
        assert_eq!(template_error(&err).message(), "template syntax error");
    }

    #[test]
    fn unknown_filter_msg_never_echoes_the_name() {
        let tpl = format!(
            "{{{{ x | {SOURCE_CANARY_IDENT} }}}}",
            SOURCE_CANARY_IDENT = "srccanary91be"
        );
        let err = render_template(&tpl, &json!({"x": 1})).unwrap_err();
        let text = format!("{err:#} {err:?}");
        assert!(!text.contains("srccanary91be"), "{text}");
        assert_eq!(
            template_error(&err).message(),
            "template uses an unknown filter"
        );
    }

    #[test]
    fn blocks_are_rejected_with_fixed_text() {
        let err = render_template("{% block b %}x{% endblock %}", &json!({})).unwrap_err();
        assert_eq!(
            template_error(&err).message(),
            "{% block %} is not supported"
        );
    }
}
