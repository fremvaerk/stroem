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
        let (category, span, enriched) = match err.kind() {
            tera::ErrorKind::SyntaxError(r) => (
                "template syntax error",
                Some(r.span().clone()),
                enrich(r.message()),
            ),
            tera::ErrorKind::RenderingError(r) => {
                let mut enriched = enrich(r.message());
                if enriched.is_none() {
                    if let Some(f) = failing_filter(src, r.span()) {
                        enriched = Some(format!("filter `{f}` failed"));
                    }
                }
                (
                    "template rendering failed",
                    Some(r.span().clone()),
                    enriched,
                )
            }
            tera::ErrorKind::Msg(text) => (msg_category(text), None, removed_builtin_hint(text)),
            tera::ErrorKind::InvalidArgument { .. } => {
                ("a filter received a value of the wrong type", None, None)
            }
            tera::ErrorKind::MissingArgument { .. } => {
                ("a filter call is missing a required argument", None, None)
            }
            tera::ErrorKind::OutOfRangeArgument { .. } => ("a number is out of range", None, None),
            _ => ("template rendering failed", None, None),
        };
        let message = match &vals {
            Some(v) => vals_category(v.kind),
            None => enriched.unwrap_or_else(|| category.to_string()),
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

/// Every filter, test and function name Tera 1.20 shipped (tera-1.20.1
/// src/tera.rs `register_builtin_*`). A name in this list is public
/// vocabulary, never a value.
pub const TERA1_BUILTIN_NAMES: &[&str] = &[
    "upper",
    "lower",
    "trim",
    "trim_start",
    "trim_end",
    "trim_start_matches",
    "trim_end_matches",
    "truncate",
    "wordcount",
    "replace",
    "capitalize",
    "title",
    "linebreaksbr",
    "indent",
    "striptags",
    "spaceless",
    "urlencode",
    "urlencode_strict",
    "escape",
    "escape_xml",
    "slugify",
    "addslashes",
    "split",
    "int",
    "float",
    "first",
    "last",
    "nth",
    "join",
    "sort",
    "unique",
    "slice",
    "group_by",
    "filter",
    "map",
    "concat",
    "abs",
    "pluralize",
    "round",
    "filesizeformat",
    "length",
    "reverse",
    "date",
    "json_encode",
    "as_str",
    "get",
    "default",
    "safe",
    "defined",
    "undefined",
    "odd",
    "even",
    "string",
    "number",
    "divisibleby",
    "iterable",
    "object",
    "starting_with",
    "ending_with",
    "containing",
    "matching",
    "range",
    "now",
    "throw",
    "get_random",
    "get_env",
];

/// Tera 2's `Value::name()` strings (tera-2.4.0 src/value/mod.rs).
const TERA_TYPE_NAMES: &[&str] = &[
    "undefined",
    "none",
    "bool",
    "u64",
    "i64",
    "f64",
    "u128",
    "i128",
    "array",
    "bytes",
    "string",
    "map/struct",
];

/// Tera 2 messages with no placeholder: safe to show verbatim.
const TERA_CONSTANT_MESSAGES: &[&str] = &[
    "Cannot divide by 0",
    "Slicing step cannot be 0",
    "Slice step is undefined",
    "Slice start is undefined",
    "Slice end is undefined",
    "Not a valid key type",
    "Tried to escape an undefined value",
    "Function `range` was called with arguments that overflow i128",
];

/// Names our engine registers as filters: Tera 2 builtins
/// (tera-2.4.0 src/tera.rs `register_builtin_filters`) + ours.
pub(crate) const REGISTERED_FILTERS: &[&str] = &[
    "safe",
    "default",
    "upper",
    "lower",
    "wordcount",
    "escape",
    "escape_html",
    "escape_xml",
    "newlines_to_br",
    "pluralize",
    "trim",
    "trim_start",
    "trim_end",
    "replace",
    "capitalize",
    "title",
    "truncate",
    "indent",
    "str",
    "int",
    "float",
    "length",
    "reverse",
    "split",
    "abs",
    "round",
    "first",
    "last",
    "nth",
    "join",
    "sort",
    "unique",
    "get",
    "values",
    "keys",
    "pairs",
    "group_by",
    "json_encode",
    "vals",
];

/// Value-free detail for a Tera message: fixed text or closed-set members
/// only. A filter error can forge any message shape (`throw`), so nothing
/// free-form is ever captured.
fn enrich(tera_message: &str) -> Option<String> {
    let m = tera_message.trim();
    if TERA_CONSTANT_MESSAGES.contains(&m) {
        return Some(m.to_string());
    }
    if (m.starts_with("Variable `")
        && (m.contains("` is not defined") || m.contains("` exists but its value is undefined")))
        || (m.starts_with("Field `") && m.contains("` is not defined"))
    {
        return Some("undefined variable or field".to_string());
    }
    if let Some(rest) = m.strip_prefix("Invalid type for the value, expected `") {
        let mut parts = rest.split('`');
        let expected = parts.next()?;
        let actual = parts.nth(1)?;
        if TERA_TYPE_NAMES.contains(&expected) && TERA_TYPE_NAMES.contains(&actual) {
            return Some(format!(
                "a filter received a value of the wrong type (expected {expected}, got {actual})"
            ));
        }
        return Some("a filter received a value of the wrong type".to_string());
    }
    None
}

/// `filter `name` is not available in Tera 2…` when an unknown-reference
/// report names a Tera 1 builtin; `None` otherwise. The emitted name is the
/// closed-list member, never the captured text.
fn removed_builtin_hint(msg_text: &str) -> Option<String> {
    let line = msg_text.lines().next()?;
    let first = line.strip_prefix("error: ").unwrap_or(line);
    for (prefix, kind) in [
        ("Unknown filter `", "filter"),
        ("Unknown test `", "test"),
        ("Unknown function `", "function"),
    ] {
        if let Some(rest) = first.strip_prefix(prefix) {
            let name = rest.split('`').next()?;
            let known = TERA1_BUILTIN_NAMES.iter().copied().find(|n| *n == name)?;
            return Some(format!(
                "{kind} `{known}` is not available in Tera 2; see the upgrade guide"
            ));
        }
    }
    None
}

/// Best effort: a registered filter name that appears as `| name` inside the
/// error's span text. Can only ever return a member of REGISTERED_FILTERS.
fn failing_filter(src: Option<&str>, span: &tera::Span) -> Option<&'static str> {
    let text = src?.get(span.range.clone())?;
    // A filter's span starts at its NAME and ends after its arguments; it
    // holds neither the left-hand value nor its own `|`, so a `|` inside is
    // part of an argument and must not be looked at.
    let text = text.trim_start();
    let ident: String = text
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if !text[ident.len()..].is_empty() && !text[ident.len()..].starts_with('(') {
        return None;
    }
    REGISTERED_FILTERS.iter().copied().find(|f| *f == ident)
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
        assert!(
            te.message() == "template rendering failed"
                || te.message().starts_with("filter `int` failed"),
            "{}",
            te.message()
        );
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

    fn msg(tpl: &str, ctx: serde_json::Value) -> String {
        let err = render_template(tpl, &ctx).unwrap_err();
        template_error(&err).to_string()
    }

    #[test]
    fn undefined_variable_is_categorised_without_the_name() {
        let m = msg("{{ nosuchvar_9c1 }}", json!({}));
        assert!(m.starts_with("undefined variable or field"), "{m}");
        assert!(!m.contains("nosuchvar_9c1"), "{m}");
        let m = msg("{{ input.nosuchfield_9c1 }}", json!({"input": {"a": 1}}));
        assert!(m.starts_with("undefined variable or field"), "{m}");
        assert!(!m.contains("nosuchfield_9c1"), "{m}");
    }

    #[test]
    fn type_mismatch_keeps_closed_set_type_names() {
        let m = msg("{{ 'abc' | round }}", json!({}));
        assert!(
            m.starts_with("a filter received a value of the wrong type (expected"),
            "{m}"
        );
    }

    #[test]
    fn constant_tera_message_is_kept_verbatim() {
        let m = msg("{{ 1 / 0 }}", json!({}));
        assert!(m.starts_with("Cannot divide by 0"), "{m}");
    }

    #[test]
    fn removed_tera1_filter_is_named_with_a_hint() {
        let err = render_template("{{ x | urlencode }}", &json!({"x": "a"})).unwrap_err();
        assert_eq!(
            template_error(&err).message(),
            "filter `urlencode` is not available in Tera 2; see the upgrade guide"
        );
    }

    #[test]
    fn forged_throw_message_selects_fixed_text_only() {
        let ctx = json!({"secret": {"X": format!("Variable `{CONTEXT_CANARY}` is not defined")}});
        let err = render_template("{{ throw(message=secret.X) }}", &ctx).unwrap_err();
        let text = format!("{err:#} {err:?}");
        assert!(!text.contains(CONTEXT_CANARY), "{text}");
    }

    /// Tera's `ReportError::message()` of a rendering failure (not the full
    /// report, which also holds the source line).
    fn tera_message(tpl: &str, ctx: &serde_json::Value) -> String {
        let tera = crate::tera_engine::render_engine(
            crate::budget::LoadBudget::unbounded(),
            Default::default(),
        );
        let ctx = tera::Context::from_serialize(ctx).unwrap();
        let err = tera.render_str(tpl, &ctx, false).unwrap_err();
        match err.kind() {
            tera::ErrorKind::RenderingError(r) => r.message().to_string(),
            other => panic!("not a RenderingError for {tpl}: {other:?}"),
        }
    }

    #[test]
    fn failing_filter_is_named_from_its_own_span() {
        let m = msg("{{ 'abc' | int }}", json!({}));
        assert!(m.starts_with("filter `int` failed"), "{m}");
        let m = msg(
            "{{ m | get(key=k | lower) }}",
            json!({"m": {}, "k": "Missing"}),
        );
        assert!(m.starts_with("filter `get` failed"), "{m}");
        let m = msg("{{ 1 | round(method=x | upper) }}", json!({"x": "bad"}));
        assert!(m.starts_with("filter `round` failed"), "{m}");
        let m = msg("{{ throw(message=x | upper) }}", json!({"x": "boom"}));
        assert!(!m.contains("upper"), "{m}");
    }

    #[test]
    fn corpus_numeric_operation_is_value_free() {
        const BIG: &str = "170141183460469231731687303715884105727";
        let ctx = json!({"secret": {"BIG": BIG}});
        let tpl = format!("{{{{ (secret.BIG | int) * 2 }}}} {SOURCE_CANARY}");
        let raw = tera_message(&tpl, &ctx);
        assert!(raw.contains(BIG), "fixture not real: {raw}");
        let err = render_template(&tpl, &ctx).unwrap_err();
        let te = template_error(&err);
        for text in [format!("{err:#}"), format!("{err:?}"), te.to_string()] {
            assert!(
                !text.contains(BIG) && !text.contains(SOURCE_CANARY),
                "{text}"
            );
        }
    }

    /// Value-bearing rendering cases: Tera's `ReportError::message()` (not
    /// the report, which contains the source line) carries the context
    /// canary — the fixture is real — and our text carries nothing.
    #[test]
    fn corpus_value_bearing_cases_are_value_free() {
        let ctx = json!({"secret": {"X": CONTEXT_CANARY, "N": "0x1f-not-a-number"}});
        let cases = [
            "{{ 1 | round(method=secret.X) }}",
            "{{ {} | get(key=secret.X) }}",
            "{{ secret.X | upper | int }}",
            "{{ secret.X | float }}",
            "{{ secret.N | int(base=16) }}",
            "{{ throw(message=secret.X) }}",
        ];
        for tpl in cases {
            let tpl = format!("{tpl} {SOURCE_CANARY}");
            let err = render_template(&tpl, &ctx).unwrap_err();
            let te = template_error(&err);
            let raw = tera_message(&tpl, &ctx);
            assert!(
                raw.contains(CONTEXT_CANARY)
                    || raw.contains(&CONTEXT_CANARY.to_uppercase())
                    || raw.contains("1f-not-a-number"),
                "fixture not real for {tpl}: {raw}"
            );
            for text in [format!("{err:#}"), format!("{err:?}"), te.to_string()] {
                for needle in [
                    CONTEXT_CANARY.to_string(),
                    CONTEXT_CANARY.to_uppercase(),
                    SOURCE_CANARY.to_string(),
                    "1f-not-a-number".to_string(),
                ] {
                    assert!(!text.contains(&needle), "{tpl}: {text}");
                }
            }
        }
    }

    /// Cases with no ReportError or no context evaluation: exact fallback.
    #[test]
    fn corpus_compile_and_conversion_cases_use_fixed_text() {
        let cases: [(&str, &str); 4] = [
            ("{{ 'a' ~ }}", "template syntax error"),
            ("{{ x | srccanary91be }}", "template uses an unknown filter"),
            (
                "{% if x is srccanary91be %}{% endif %}",
                "template uses an unknown test",
            ),
            ("{{ srccanary91be() }}", "template uses an unknown function"),
        ];
        for (tpl, expected) in cases {
            let err =
                render_template(&format!("{tpl} {SOURCE_CANARY}"), &json!({"x": 1})).unwrap_err();
            assert_eq!(template_error(&err).message(), expected, "{tpl}");
            let text = format!("{err:#} {err:?}");
            assert!(
                !text.contains("srccanary91be") && !text.contains(SOURCE_CANARY),
                "{tpl}: {text}"
            );
        }
        // A non-map context is a Context conversion failure (Msg).
        let err = render_template("{{ x }}", &json!([1, 2])).unwrap_err();
        assert_eq!(
            template_error(&err).message(),
            "template could not be compiled"
        );
    }
}
