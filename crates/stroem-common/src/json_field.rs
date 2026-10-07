//! The `json` input field rule (spec 2026-10-06-json-input-type § 4).
//!
//! Every string inside a `json` field's value is literal text (no Tera
//! delimiters), a single `{{ expression }}` — evaluated to its native JSON
//! value — or anything else, which is an error. Object keys are never
//! rendered, and a rendered value is never rendered again (R26).

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use crate::template_error::TemplateError;

/// The wrapper around a single expression (spec § 4.2 step 1). `set` parses
/// exactly the expressions `{{ }}` parses (tera parser.rs:1529 / :1816); the
/// `is undefined` branch writes an undefined value out so that `{{ typo }}`
/// raises "Tried to render a variable that is undefined", as in any field.
const PREFIX: &str = "{% set __stroem_v = ";
const SUFFIX: &str = " %}{% if __stroem_v is undefined %}{{ __stroem_v }}{% endif %}{{ __stroem_v | json_encode() }}";

const DELIMITERS: [&str; 6] = ["{{", "}}", "{%", "%}", "{#", "#}"];

const NOT_JSON: &str = "the expression's value could not be converted to JSON";

/// What one string inside a `json` value is (spec § 4.1).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Classified<'a> {
    Literal,
    Single(SingleExpr<'a>),
    Mixed,
}

/// A string that is exactly one `{{ expression }}`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SingleExpr<'a> {
    /// The text between the delimiters (and whitespace-control `-`), verbatim.
    pub inner: &'a str,
    /// 1-based `(line, column)` of `inner`'s first character in the string.
    pub start: (usize, usize),
}

pub(crate) fn classify(s: &str) -> Classified<'_> {
    if !crate::template::looks_templated(s) {
        return Classified::Literal;
    }
    let is_ws = |c: char| c.is_ascii_whitespace();
    let lead = s.len() - s.trim_start_matches(is_ws).len();
    let trimmed = s.trim_matches(is_ws);
    let Some(body) = trimmed
        .strip_prefix("{{")
        .and_then(|b| b.strip_suffix("}}"))
    else {
        return Classified::Mixed;
    };
    let (body, open_len) = match body.strip_prefix('-') {
        Some(b) => (b, 3),
        None => (body, 2),
    };
    let inner = body.strip_suffix('-').unwrap_or(body);
    if DELIMITERS.iter().any(|d| inner.contains(d)) {
        return Classified::Mixed;
    }
    Classified::Single(SingleExpr {
        inner,
        start: position_of(s, lead + open_len),
    })
}

/// 1-based `(line, column)` of byte offset `byte`; columns count characters,
/// as Tera's lexer does (lexer.rs:270-277).
fn position_of(s: &str, byte: usize) -> (usize, usize) {
    let before = &s[..byte];
    let line = 1 + before.matches('\n').count();
    let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, column)
}

/// Map a 1-based wrapper position to the author's string (spec § 4.2 step 3).
/// A position inside `PREFIX` or `SUFFIX` has no counterpart: `None`.
pub(crate) fn map_position(expr: &SingleExpr<'_>, pos: (usize, usize)) -> Option<(usize, usize)> {
    let p = PREFIX.chars().count();
    let (l, c) = pos;
    let newlines = expr.inner.matches('\n').count();
    let last_line = 1 + newlines;
    let last_col = if newlines == 0 {
        p + expr.inner.chars().count()
    } else {
        expr.inner.rsplit('\n').next().unwrap_or("").chars().count()
    };
    let before_start = l == 0 || (l == 1 && c < p + 1);
    let after_end = l > last_line || (l == last_line && c > last_col);
    if before_start || after_end {
        return None;
    }
    let (l0, c0) = expr.start;
    if l == 1 {
        Some((l0, c0 + c - (p + 1)))
    } else {
        Some((l0 + l - 1, c))
    }
}

/// Evaluate a single expression to its native value. `render` is called
/// exactly once; a failure keeps its value-free category and gets its
/// position mapped back — the expression is never evaluated a second time.
pub(crate) fn eval_single(
    expr: &SingleExpr<'_>,
    render: &dyn Fn(&str) -> Result<String>,
) -> Result<Value> {
    let wrapper = format!("{PREFIX}{}{SUFFIX}", expr.inner);
    let text = render(&wrapper).map_err(|e| remap(e, expr))?;
    serde_json::from_str(&text).map_err(|_| anyhow!(NOT_JSON))
}

fn remap(err: anyhow::Error, expr: &SingleExpr<'_>) -> anyhow::Error {
    // `render_template` wraps the TemplateError in one context of our own
    // ("Failed to render template" / "Failed to parse template").
    let outer = (err.chain().count() > 1).then(|| err.to_string());
    match err.downcast::<TemplateError>() {
        Ok(te) => {
            let pos = te.position().and_then(|p| map_position(expr, p));
            let e = anyhow::Error::new(te.with_position(pos));
            match outer {
                Some(o) => e.context(o),
                None => e,
            }
        }
        Err(err) => err,
    }
}

/// The json rule for one field value (spec § 4.1–4.3), recursive over
/// objects and arrays.
pub fn render_json_value(value: &Value, field: &str, context: &Value) -> Result<Value> {
    render_json_value_with(value, field, &|t| {
        crate::template::render_template(t, context)
    })
}

pub(crate) fn render_json_value_with(
    value: &Value,
    field: &str,
    render: &dyn Fn(&str) -> Result<String>,
) -> Result<Value> {
    walk(value, field, render, &mut Vec::new())
}

fn walk(
    value: &Value,
    field: &str,
    render: &dyn Fn(&str) -> Result<String>,
    path: &mut Vec<Option<usize>>,
) -> Result<Value> {
    match value {
        Value::String(s) => match classify(s) {
            Classified::Literal => Ok(value.clone()),
            Classified::Single(expr) => {
                eval_single(&expr, render).with_context(|| location(field, path))
            }
            Classified::Mixed => Err(anyhow!(
                "{}: a json field takes a literal value or a single {{{{ expression }}}}",
                location(field, path)
            )),
        },
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                path.push(None);
                let rendered = walk(v, field, render, path);
                path.pop();
                out.insert(k.clone(), rendered?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, v) in items.iter().enumerate() {
                path.push(Some(i));
                let rendered = walk(v, field, render, path);
                path.pop();
                out.push(rendered?);
            }
            Ok(Value::Array(out))
        }
        other => Ok(other.clone()),
    }
}

/// Names the field (a schema key) and, for a string nested only in arrays,
/// its indices. An object key is author/value text and is never named.
fn location(field: &str, path: &[Option<usize>]) -> String {
    if !path.is_empty() && path.iter().all(Option::is_some) {
        let idx: String = path.iter().flatten().map(|i| format!("[{i}]")).collect();
        format!("Input field '{field}' at `{idx}`")
    } else {
        format!("Input field '{field}'")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::Cell;

    #[test]
    fn classify_literal_single_mixed() {
        assert_eq!(classify("plain"), Classified::Literal);
        assert_eq!(classify(""), Classified::Literal);
        assert_eq!(
            classify("{{ x }}"),
            Classified::Single(SingleExpr {
                inner: " x ",
                start: (1, 3)
            })
        );
        assert_eq!(
            classify("  {{ x }} "),
            Classified::Single(SingleExpr {
                inner: " x ",
                start: (1, 5)
            })
        );
        assert_eq!(
            classify("{{- x -}}"),
            Classified::Single(SingleExpr {
                inner: " x ",
                start: (1, 4)
            })
        );
        assert_eq!(classify("id {{ x }}"), Classified::Mixed);
        assert_eq!(classify("{{ a }}{{ b }}"), Classified::Mixed);
        assert_eq!(classify("{% if x %}1{% endif %}"), Classified::Mixed);
        assert_eq!(classify("{# note #}"), Classified::Mixed);
    }

    #[test]
    fn yaml_block_scalar_trailing_newline_is_single() {
        assert!(matches!(classify("{{ x }}\n"), Classified::Single(_)));
        assert!(matches!(classify("\n  {{ x }}\n"), Classified::Single(_)));
    }

    /// Spec § 4.1: a delimiter inside a string literal of the expression
    /// classifies as Mixed — the documented, pinned false negative.
    #[test]
    fn delimiter_inside_a_string_literal_is_the_known_false_negative() {
        assert_eq!(
            classify(r#"{{ x | default(value="}}") }}"#),
            Classified::Mixed
        );
    }

    #[test]
    fn single_expressions_keep_native_values() {
        let ctx = json!({
            "o": {"a": 1}, "l": [1, 2, 3], "n": 3, "f": 1.5,
            "b": true, "s": "txt", "z": null,
        });
        let r = |t: &str| render_json_value(&json!(t), "f", &ctx).unwrap();
        assert_eq!(r("{{ o }}"), json!({"a": 1}));
        assert_eq!(r("{{ l }}"), json!([1, 2, 3]));
        assert_eq!(r("{{ n }}"), json!(3));
        assert_eq!(r("{{ f }}"), json!(1.5));
        assert_eq!(r("{{ b }}"), json!(true));
        assert_eq!(r("{{ s }}"), json!("txt"));
        assert_eq!(r("{{ z }}"), Value::Null);
        assert_eq!(r("{{ l | length }}"), json!(3));
        let missing = render_json_value(&json!("{{ o.missing }}"), "f", &ctx).unwrap_err();
        let mm = format!("{missing:#}");
        assert!(
            mm.starts_with("Input field 'f'") && mm.contains("undefined"),
            "{mm}"
        );
        assert_eq!(r("{{ o.missing | default(value=none) }}"), Value::Null);
        assert_eq!(r("{{ o.missing | default(value=[]) }}"), json!([]));
        // json_encode yields the TEXT — a string (spec § 4.3).
        assert_eq!(r("{{ o | json_encode() }}"), json!(r#"{"a":1}"#));
        assert_eq!(r("plain text"), json!("plain text"));
    }

    #[test]
    fn undefined_top_level_variable_is_an_error_not_null() {
        let err = render_json_value(&json!("{{ typo }}"), "cfg", &json!({})).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.starts_with("Input field 'cfg'"), "{msg}");
        assert!(msg.contains("undefined"), "{msg}");
    }

    #[test]
    fn objects_and_arrays_are_walked_and_keys_are_not_rendered() {
        let ctx = json!({"h": "db.local", "p": 5432});
        let v = json!({"{{ h }}": {"host": "{{ h }}", "port": "{{ p }}"}, "l": ["{{ p }}", 1]});
        let out = render_json_value(&v, "cfg", &ctx).unwrap();
        assert_eq!(
            out,
            json!({"{{ h }}": {"host": "db.local", "port": 5432}, "l": [5432, 1]})
        );
    }

    #[test]
    fn mixed_template_is_a_value_free_error() {
        let ctx = json!({"secret": {"T": "canary-secret-value"}});
        let err = render_json_value(&json!(["ok", "id {{ secret.T }}"]), "cfg", &ctx).unwrap_err();
        let msg = format!("{err:#}");
        assert_eq!(
            msg,
            "Input field 'cfg' at `[1]`: a json field takes a literal value or a single {{ expression }}"
        );
        let nested = render_json_value(&json!({"a": {"b": "x {{ y }}"}}), "cfg", &ctx).unwrap_err();
        assert!(
            format!("{nested:#}").starts_with("Input field 'cfg': "),
            "{nested:#}"
        );
    }

    #[test]
    fn a_failing_expression_never_leaks_the_value_or_the_template() {
        let ctx = json!({"secret": {"T": "canary-secret-value"}});
        let err = render_json_value(&json!("{{ secret.T | int }}"), "cfg", &ctx).unwrap_err();
        let msg = format!("{err:#}");
        assert!(!msg.contains("canary"), "{msg}");
        assert!(!msg.contains("secret.T"), "{msg}");
        assert!(!msg.contains("__stroem_v"), "{msg}");
    }

    #[test]
    fn the_expression_is_rendered_exactly_once() {
        let calls = Cell::new(0);
        let failing = |_t: &str| -> Result<String> {
            calls.set(calls.get() + 1);
            Err(anyhow!("boom"))
        };
        assert!(render_json_value_with(&json!("{{ x | vals }}"), "f", &failing).is_err());
        assert_eq!(calls.get(), 1, "a failure must not trigger a second render");

        calls.set(0);
        let ok = |_t: &str| -> Result<String> {
            calls.set(calls.get() + 1);
            Ok("1".into())
        };
        assert_eq!(
            render_json_value_with(&json!("{{ x }}"), "f", &ok).unwrap(),
            json!(1)
        );
        assert_eq!(calls.get(), 1);
    }

    fn template_position(err: &anyhow::Error) -> Option<(usize, usize)> {
        err.chain()
            .find_map(|c| c.downcast_ref::<TemplateError>())
            .and_then(TemplateError::position)
    }

    /// The json-field error points exactly where Tera points when it renders
    /// the author's own string as a plain template.
    fn assert_maps_like_plain_render(s: &str) {
        let ctx = json!({"x": "v"});
        let plain = crate::template::render_template(s, &ctx).unwrap_err();
        let expected = template_position(&plain);
        assert!(
            expected.is_some(),
            "fixture must give a positioned error: {plain:#}"
        );
        let json_err = render_json_value(&json!(s), "f", &ctx).unwrap_err();
        assert_eq!(
            template_position(&json_err),
            expected,
            "{s:?}: {json_err:#}"
        );
    }

    #[test]
    fn error_positions_point_into_the_authors_string() {
        assert_maps_like_plain_render("{{ missing[:1] }}"); // first token
        assert_maps_like_plain_render("{{ x | upper ~ missing[:1] }}"); // last token
        assert_maps_like_plain_render("{{ x\n  ~ missing[:1] }}"); // second line of inner
        assert_maps_like_plain_render("\n  {{ missing[:1] }}"); // leading newline + spaces
        assert_maps_like_plain_render("{{- missing[:1] -}}"); // whitespace control
        assert_maps_like_plain_render("{{ 'ü✓' ~ missing[:1] }}"); // non-ASCII inside inner
    }

    #[test]
    fn a_position_in_the_wrapper_itself_is_dropped() {
        let expr = SingleExpr {
            inner: " x ",
            start: (1, 3),
        };
        let p = PREFIX.chars().count();
        assert_eq!(map_position(&expr, (1, 1)), None);
        assert_eq!(map_position(&expr, (1, p)), None);
        assert_eq!(map_position(&expr, (1, p + 1)), Some((1, 3)));
        assert_eq!(map_position(&expr, (1, p + 3)), Some((1, 5)));
        assert_eq!(map_position(&expr, (1, p + 4)), None); // first SUFFIX char
                                                           // `{{ typo }}`'s undefined error is raised in SUFFIX: no position.
        let err = render_json_value(&json!("{{ typo }}"), "f", &json!({})).unwrap_err();
        assert_eq!(template_position(&err), None, "{err:#}");
    }
}
