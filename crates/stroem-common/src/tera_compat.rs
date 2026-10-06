//! Tera 1 behaviour kept under Tera 2 (spec 2026-10-06 § 3.6 C1, C3;
//! `indent` and `unique` per ruling R20).
//! The ported filters reproduce tera 1.20.1 (MIT, Keats) output exactly;
//! they convert through serde_json so the Tera 1 logic applies unchanged.

use chrono::{DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use std::fmt::Write as _;
use tera::{Kwargs, State, TeraResult, Value};

fn to_json(v: &Value) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

fn from_json(v: &serde_json::Value) -> Value {
    Value::from_serializable(v)
}

fn err(msg: &str) -> tera::Error {
    tera::Error::message(msg.to_string())
}

/// C1: `null` and undefined both take `value` (Tera 1 replaced null too).
fn default(val: Value, kwargs: Kwargs, _: &State) -> TeraResult<Value> {
    let default_val = kwargs.must_get::<Value>("value")?;
    if kwargs.get::<bool>("boolean")?.unwrap_or(false) {
        return Ok(if val.is_truthy() { val } else { default_val });
    }
    Ok(if val.is_undefined() || val.is_none() {
        default_val
    } else {
        val
    })
}

fn tera1_render(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::String(s) => out.push_str(s),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                let _ = write!(out, "{i}");
            } else if let Some(u) = n.as_u64() {
                let _ = write!(out, "{u}");
            } else if let Some(f) = n.as_f64() {
                let _ = write!(out, "{f}");
            }
        }
        serde_json::Value::Bool(b) => {
            let _ = write!(out, "{b}");
        }
        serde_json::Value::Null => {}
        serde_json::Value::Array(a) => {
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                tera1_render(item, out);
            }
            out.push(']');
        }
        serde_json::Value::Object(_) => out.push_str("[object]"),
    }
}

fn as_str(val: &Value, _: Kwargs, _: &State) -> Value {
    let mut s = String::new();
    tera1_render(&to_json(val), &mut s);
    Value::from(s)
}

fn unescape_pat(kwargs: &Kwargs, filter: &str) -> TeraResult<String> {
    let pat = kwargs
        .get::<&str>("pat")?
        .ok_or_else(|| err(&format!("Filter `{filter}` expected an arg called `pat`")))?;
    Ok(pat.replace("\\n", "\n").replace("\\t", "\t"))
}

fn trim_start_matches(val: &str, kwargs: Kwargs, _: &State) -> TeraResult<String> {
    let pat = unescape_pat(&kwargs, "trim_start_matches")?;
    Ok(val.trim_start_matches(pat.as_str()).to_string())
}

fn trim_end_matches(val: &str, kwargs: Kwargs, _: &State) -> TeraResult<String> {
    let pat = unescape_pat(&kwargs, "trim_end_matches")?;
    Ok(val.trim_end_matches(pat.as_str()).to_string())
}

fn linebreaksbr(val: &str, _: Kwargs, _: &State) -> String {
    val.replace("\r\n", "<br>").replace('\n', "<br>")
}

/// tera 1.20.1 `PointerMachina` (src/context.rs): splits a dotted path into
/// segments, honouring `"…"` / `'…'` quoted segments, `[…]` and `\`
/// escapes. Ported verbatim with one deliberate difference: Tera 1 used
/// `chars().enumerate()` positions as BYTE offsets, which mis-sliced or
/// panicked on a multi-byte character; this walks `char_indices` and slices
/// with `get`, so an ASCII path splits exactly as in Tera 1 and a non-ASCII
/// one splits correctly (`broken` marks a slice off a char boundary — the
/// lookup then fails instead of panicking).
struct PointerMachina<'a> {
    pointer: &'a str,
    single_quoted: bool,
    dual_quoted: bool,
    escaped: bool,
    last_position: usize,
    broken: bool,
}

impl<'a> PointerMachina<'a> {
    fn new(pointer: &'a str) -> Self {
        PointerMachina {
            pointer,
            single_quoted: false,
            dual_quoted: false,
            escaped: false,
            last_position: 0,
            broken: false,
        }
    }

    fn slice(&mut self, from: usize, to: Option<usize>) -> Option<&'a str> {
        let s = match to {
            Some(to) => self.pointer.get(from..to),
            None => self.pointer.get(from..),
        };
        if s.is_none() {
            self.broken = true;
        }
        s
    }
}

impl<'a> Iterator for PointerMachina<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.broken {
            return None;
        }
        let forwarded = self.slice(self.last_position, None)?;
        let mut offset: usize = 0;
        for (i, character) in forwarded.char_indices() {
            // Tera 1's `if !single_quoted && !dual_quoted && !escaped` inside
            // the `[`, `]` and `.` arms, as match guards (a false guard falls
            // through to `_ => ()`, exactly as the inner `if` did nothing).
            let unquoted = !self.single_quoted && !self.dual_quoted && !self.escaped;
            match character {
                '"' => {
                    if !self.escaped {
                        self.dual_quoted = !self.dual_quoted;
                        if i == offset {
                            offset += 1;
                        } else {
                            let result = self
                                .slice(self.last_position + offset, Some(self.last_position + i))?;
                            self.last_position += i + 1;
                            if !result.is_empty() {
                                return Some(result);
                            }
                        }
                    }
                }
                '\'' => {
                    if !self.escaped {
                        self.single_quoted = !self.single_quoted;
                        if i == offset {
                            offset += 1;
                        } else {
                            let result = self
                                .slice(self.last_position + offset, Some(self.last_position + i))?;
                            self.last_position += i + 1;
                            if !result.is_empty() {
                                return Some(result);
                            }
                        }
                    }
                }
                '\\' => {
                    self.escaped = true;
                    continue;
                }
                '[' if unquoted => {
                    let result =
                        self.slice(self.last_position + offset, Some(self.last_position + i))?;
                    self.last_position += i + 1;
                    if !result.is_empty() {
                        return Some(result);
                    }
                }
                ']' if unquoted => {
                    offset += 1;
                }
                '.' if unquoted => {
                    if i == offset {
                        offset += 1;
                    } else {
                        let result =
                            self.slice(self.last_position + offset, Some(self.last_position + i))?;
                        self.last_position += i + 1;
                        if !result.is_empty() {
                            return Some(result);
                        }
                    }
                }
                _ => (),
            }
            self.escaped = false;
        }
        if self.last_position + offset < self.pointer.len() {
            let result = self.slice(self.last_position + offset, None)?;
            self.last_position = self.pointer.len();
            return Some(result);
        }
        None
    }
}

/// serde_json's `parse_index`, as tera 1.20.1 copied it: no `+`, no leading
/// zero.
fn parse_index(s: &str) -> Option<usize> {
    if s.starts_with('+') || (s.starts_with('0') && s.len() != 1) {
        return None;
    }
    s.parse().ok()
}

/// tera 1.20.1 `dotted_pointer` (src/context.rs): an empty path is the value
/// itself; segments are unescaped `~1` → `/`, `~0` → `~`.
fn dotted<'a>(value: &'a serde_json::Value, pointer: &str) -> Option<&'a serde_json::Value> {
    if pointer.is_empty() {
        return Some(value);
    }
    let mut machina = PointerMachina::new(pointer);
    let tokens: Vec<String> = machina
        .by_ref()
        .map(|mat| mat.replace("~1", "/").replace("~0", "~"))
        .collect();
    if machina.broken {
        return None;
    }
    tokens.iter().try_fold(value, |target, token| match target {
        serde_json::Value::Object(map) => map.get(token),
        serde_json::Value::Array(list) => parse_index(token).and_then(|x| list.get(x)),
        _ => None,
    })
}

fn as_array(val: &Value, filter: &str) -> TeraResult<Vec<serde_json::Value>> {
    match to_json(val) {
        serde_json::Value::Array(a) => Ok(a),
        _ => Err(err(&format!("The `{filter}` filter expects an array"))),
    }
}

fn map(val: &Value, kwargs: Kwargs, _: &State) -> TeraResult<Value> {
    let arr = as_array(val, "map")?;
    // Tera 1 returns an empty array before looking at its arguments.
    if arr.is_empty() {
        return Ok(from_json(&serde_json::Value::Array(arr)));
    }
    let attribute = kwargs
        .get::<&str>("attribute")?
        .ok_or_else(|| err("The `map` filter has to have an `attribute` argument"))?;
    let out: Vec<serde_json::Value> = arr
        .iter()
        .filter_map(|v| dotted(v, attribute).filter(|x| !x.is_null()).cloned())
        .collect();
    Ok(from_json(&serde_json::Value::Array(out)))
}

fn filter(val: &Value, kwargs: Kwargs, _: &State) -> TeraResult<Value> {
    let arr = as_array(val, "filter")?;
    if arr.is_empty() {
        return Ok(from_json(&serde_json::Value::Array(arr)));
    }
    let key = kwargs
        .get::<&str>("attribute")?
        .ok_or_else(|| err("The `filter` filter has to have an `attribute` argument"))?;
    let wanted = kwargs
        .get::<Value>("value")?
        .map(|v| to_json(&v))
        .unwrap_or(serde_json::Value::Null);
    let out: Vec<serde_json::Value> = arr
        .into_iter()
        .filter(|v| {
            let got = dotted(v, key).unwrap_or(&serde_json::Value::Null);
            if wanted.is_null() {
                !got.is_null()
            } else {
                *got == wanted
            }
        })
        .collect();
    Ok(from_json(&serde_json::Value::Array(out)))
}

/// tera 1.20.1 `unique` (src/builtins/filters/array.rs + filter_utils.rs):
/// strings compare case-INsensitively unless `case_sensitive=true`;
/// `attribute=` picks the key; the first element's type decides the
/// comparison and a mixed-type array is an error; floats, arrays, objects
/// and null cannot be made unique.
fn unique(val: &Value, kwargs: Kwargs, _: &State) -> TeraResult<Value> {
    use std::collections::HashSet;
    enum Seen {
        Numbers(HashSet<i64>),
        Bools(HashSet<bool>),
        Strings(HashSet<String>, bool),
    }
    impl Seen {
        fn insert(&mut self, key: &serde_json::Value) -> TeraResult<bool> {
            match self {
                Seen::Numbers(set) => {
                    let n = key
                        .as_i64()
                        .ok_or_else(|| err(&format!("expected number got {key}")))?;
                    Ok(set.insert(n))
                }
                Seen::Bools(set) => {
                    let b = key
                        .as_bool()
                        .ok_or_else(|| err(&format!("expected bool got {key}")))?;
                    Ok(set.insert(b))
                }
                Seen::Strings(set, case_sensitive) => {
                    let s = key
                        .as_str()
                        .ok_or_else(|| err(&format!("expected string got {key}")))?;
                    Ok(set.insert(if *case_sensitive {
                        s.to_owned()
                    } else {
                        s.to_lowercase()
                    }))
                }
            }
        }
    }

    let arr = as_array(val, "unique")?;
    if arr.is_empty() {
        return Ok(from_json(&serde_json::Value::Array(arr)));
    }
    let case_sensitive = kwargs.get::<bool>("case_sensitive")?.unwrap_or(false);
    let attribute = kwargs.get::<&str>("attribute")?.unwrap_or("").to_string();

    let first = dotted(&arr[0], &attribute).ok_or_else(|| {
        err(&format!(
            "attribute '{attribute}' does not reference a field"
        ))
    })?;
    let disc = std::mem::discriminant(first);
    let mut seen = match first {
        serde_json::Value::Null => return Err(err("Null is not a unique value")),
        serde_json::Value::Bool(_) => Seen::Bools(HashSet::new()),
        serde_json::Value::Number(n) if n.is_f64() => {
            return Err(err("Unique floats are not implemented"))
        }
        serde_json::Value::Number(_) => Seen::Numbers(HashSet::new()),
        serde_json::Value::String(_) => Seen::Strings(HashSet::new(), case_sensitive),
        serde_json::Value::Array(_) => return Err(err("Unique arrays are not implemented")),
        serde_json::Value::Object(_) => return Err(err("Unique objects are not implemented")),
    };

    let mut out = Vec::new();
    for v in &arr {
        if let Some(key) = dotted(v, &attribute) {
            if disc != std::mem::discriminant(key) {
                return Err(err("unique filter can't compare multiple types"));
            }
            if seen.insert(key)? {
                out.push(v.clone());
            }
        }
    }
    Ok(from_json(&serde_json::Value::Array(out)))
}

/// tera 1.20.1 `indent` (src/builtins/filters/string.rs): `prefix=` (default
/// four spaces), `first=` indents the first line, `blank=` indents blank
/// (whitespace-only) lines; the trailing newline is dropped, as in Tera 1.
/// Tera 2's `width=` / `indentation=` are also accepted (`indentation`, one
/// character, default a space, repeated `width` times, width ≤ 1000) so a
/// template written for Tera 2 keeps its prefix; `prefix=` wins over both.
fn indent(val: &str, kwargs: Kwargs, _: &State) -> TeraResult<String> {
    let prefix = match kwargs.get::<&str>("prefix")? {
        Some(p) => p.to_string(),
        None => {
            let width = kwargs.get::<usize>("width")?;
            let indentation = kwargs.get::<&str>("indentation")?;
            if indentation.is_some_and(|i| i.chars().count() != 1) {
                return Err(err(
                    "The `indentation` argument must contain exactly one character",
                ));
            }
            indentation
                .unwrap_or(" ")
                .repeat(width.unwrap_or(4).min(1000))
        }
    };
    let first = kwargs.get::<bool>("first")?.unwrap_or(false);
    let blank = kwargs.get::<bool>("blank")?.unwrap_or(false);

    let mut out = String::with_capacity(
        val.len() + (prefix.len() * (val.chars().filter(|&c| c == '\n').count() + 1)),
    );
    let mut first_pass = true;
    for line in val.lines() {
        if first_pass {
            if first {
                out.push_str(&prefix);
            }
            first_pass = false;
        } else {
            out.push('\n');
            if blank || !line.trim_start().is_empty() {
                out.push_str(&prefix);
            }
        }
        out.push_str(line);
    }
    Ok(out)
}

fn concat(val: &Value, kwargs: Kwargs, _: &State) -> TeraResult<Value> {
    let mut arr = as_array(val, "concat")?;
    let with = kwargs
        .get::<Value>("with")?
        .ok_or_else(|| err("The `concat` filter has to have a `with` argument"))?;
    match to_json(&with) {
        serde_json::Value::Array(more) => arr.extend(more),
        other => arr.push(other),
    }
    Ok(from_json(&serde_json::Value::Array(arr)))
}

fn slice(val: &Value, kwargs: Kwargs, _: &State) -> TeraResult<Value> {
    let arr = as_array(val, "slice")?;
    if arr.is_empty() {
        return Ok(from_json(&serde_json::Value::Array(arr)));
    }
    let index = |i: f64| {
        if i >= 0.0 {
            i as usize
        } else {
            (arr.len() as f64 + i) as usize
        }
    };
    let start = kwargs.get::<f64>("start")?.map(index).unwrap_or(0);
    let end = kwargs
        .get::<f64>("end")?
        .map(index)
        .unwrap_or(arr.len())
        .min(arr.len());
    let out = if start >= end {
        Vec::new()
    } else {
        arr[start..end].to_vec()
    };
    Ok(from_json(&serde_json::Value::Array(out)))
}

fn date(val: &Value, kwargs: Kwargs, _: &State) -> TeraResult<String> {
    use chrono::format::{Item, StrftimeItems};
    let format = kwargs
        .get::<&str>("format")?
        .unwrap_or("%Y-%m-%d")
        .to_string();
    if StrftimeItems::new(&format).any(|i| matches!(i, Item::Error)) {
        return Err(err("Invalid date format"));
    }
    let tz: Option<Tz> = match kwargs.get::<&str>("timezone")? {
        Some(t) => Some(t.parse().map_err(|_| err("Error parsing the timezone"))?),
        None => None,
    };
    let formatted = match to_json(val) {
        serde_json::Value::Number(n) => {
            let i = n
                .as_i64()
                .ok_or_else(|| err("Filter `date` was invoked on a float"))?;
            let naive = DateTime::<Utc>::from_timestamp(i, 0)
                .ok_or_else(|| err("timestamp out of range"))?
                .naive_utc();
            match tz {
                Some(tz) => tz.from_utc_datetime(&naive).format(&format).to_string(),
                None => naive.format(&format).to_string(),
            }
        }
        serde_json::Value::String(s) if s.contains('T') => match s.parse::<DateTime<FixedOffset>>()
        {
            Ok(d) => match tz {
                Some(tz) => d.with_timezone(&tz).format(&format).to_string(),
                None => d.format(&format).to_string(),
            },
            Err(_) => match s.parse::<NaiveDateTime>() {
                Ok(n) => DateTime::<Utc>::from_naive_utc_and_offset(n, Utc)
                    .format(&format)
                    .to_string(),
                Err(_) => {
                    return Err(tera::Error::message(format!(
                        "Error parsing `{s:?}` as rfc3339 date or naive datetime"
                    )))
                }
            },
        },
        serde_json::Value::String(s) => match NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
            Ok(d) => DateTime::<Utc>::from_naive_utc_and_offset(
                d.and_hms_opt(0, 0, 0).unwrap_or_default(),
                Utc,
            )
            .format(&format)
            .to_string(),
            Err(_) => {
                return Err(tera::Error::message(format!(
                    "Error parsing `{s:?}` as YYYY-MM-DD date"
                )))
            }
        },
        _ => {
            return Err(err(
                "Filter `date` received an incorrect type: expected i64|u64|String",
            ))
        }
    };
    Ok(formatted)
}

fn now(kwargs: Kwargs, _: &State) -> TeraResult<Value> {
    let utc = kwargs.get::<bool>("utc")?.unwrap_or(false);
    let timestamp = kwargs.get::<bool>("timestamp")?.unwrap_or(false);
    Ok(match (utc, timestamp) {
        (_, true) if utc => Value::from(Utc::now().timestamp()),
        (_, true) => Value::from(Local::now().timestamp()),
        (true, false) => Value::from(Utc::now().to_rfc3339()),
        (false, false) => Value::from(Local::now().to_rfc3339()),
    })
}

/// Register C1 and C3 on the base engine.
pub(crate) fn register(t: &mut tera::Tera) {
    t.register_filter("default", default);
    t.register_filter("as_str", as_str);
    t.register_filter("trim_start_matches", trim_start_matches);
    t.register_filter("trim_end_matches", trim_end_matches);
    t.register_filter("linebreaksbr", linebreaksbr);
    t.register_filter("map", map);
    t.register_filter("filter", filter);
    t.register_filter("concat", concat);
    t.register_filter("unique", unique);
    t.register_filter("indent", indent);
    t.register_filter("slice", slice);
    t.register_filter("date", date);
    t.register_function("now", now);
}

#[cfg(test)]
mod tests {
    use crate::template::render_template;
    use serde_json::json;

    fn r(tpl: &str, ctx: serde_json::Value) -> String {
        render_template(tpl, &ctx).unwrap()
    }

    #[test]
    fn default_replaces_null_like_tera1() {
        let ctx = json!({"step": {"output": null}, "x": null});
        assert_eq!(r("{{ x | default(value='d') }}", ctx.clone()), "d");
        assert_eq!(
            r(
                "{{ step.output.items | default(value='n/a') }}",
                ctx.clone()
            ),
            "n/a"
        );
        assert_eq!(r("{{ missing | default(value=1) }}", ctx.clone()), "1");
        assert_eq!(
            r("{{ '' | default(value='d', boolean=true) }}", ctx.clone()),
            "d"
        );
        assert_eq!(r("{{ 'v' | default(value='d') }}", ctx), "v");
    }

    #[test]
    fn as_str_renders_like_tera1() {
        let ctx = json!({"o": {"a": 1}, "a": ["x", 2, null, 2.0], "f": 2.0});
        assert_eq!(r("{{ o | as_str }}", ctx.clone()), "[object]");
        assert_eq!(r("{{ a | as_str }}", ctx.clone()), "[x, 2, , 2]");
        assert_eq!(r("{{ f | as_str }}", ctx), "2");
    }

    #[test]
    fn trim_matches_unescape_newline_and_tab_patterns() {
        assert_eq!(
            r(
                r#"{{ "\n\nx" | trim_start_matches(pat="\\n") }}"#,
                json!({})
            ),
            "x"
        );
        assert_eq!(
            r(r#"{{ "x--" | trim_end_matches(pat="-") }}"#, json!({})),
            "x"
        );
    }

    #[test]
    fn linebreaksbr_handles_crlf_and_lf_only() {
        let ctx = json!({"s": "a\r\nb\nc\rd"});
        assert_eq!(r("{{ s | linebreaksbr }}", ctx), "a<br>b<br>c\rd");
    }

    #[test]
    fn map_filter_concat_slice_like_tera1() {
        let ctx = json!({"xs": [{"n": "a", "k": 1}, {"n": "b", "k": 2}, {"k": 3}]});
        assert_eq!(
            r("{{ xs | map(attribute='n') | join(sep=',') }}", ctx.clone()),
            "a,b"
        );
        assert_eq!(
            r(
                "{{ xs | filter(attribute='k', value=2) | map(attribute='n') | join(sep=',') }}",
                ctx.clone()
            ),
            "b"
        );
        assert_eq!(
            r("{{ [1, 2] | concat(with=3) | join(sep=',') }}", json!({})),
            "1,2,3"
        );
        assert_eq!(
            r(
                "{{ [1, 2] | concat(with=[3, 4]) | join(sep=',') }}",
                json!({})
            ),
            "1,2,3,4"
        );
        assert_eq!(
            r(
                "{{ [1, 2, 3, 4] | slice(start=1, end=-1) | join(sep=',') }}",
                json!({})
            ),
            "2,3"
        );
    }

    #[test]
    fn map_and_filter_return_empty_before_checking_attribute_like_tera1() {
        assert_eq!(r("{{ [] | map | length }}", json!({})), "0");
        assert_eq!(r("{{ [] | filter | length }}", json!({})), "0");
        assert!(render_template("{{ [1] | map | length }}", &json!({})).is_err());
        assert!(render_template("{{ [1] | filter | length }}", &json!({})).is_err());
    }

    /// tera 1.20.1 `dotted_pointer` forms, through `map` / `filter`.
    #[test]
    fn dotted_pointer_matches_tera1() {
        let ctx = json!({"xs": [{
            "a": {"b.c": "quoted", "b": {"c": "plain"}, "x/y": "slash", "t~u": "tilde"},
            "l": ["zero", "one"],
            "é": {"x": "accent"},
        }, null]});
        let m = |attr: &str| {
            r(
                &format!("{{{{ xs | map(attribute='{attr}') | join(sep=',') }}}}"),
                ctx.clone(),
            )
        };
        assert_eq!(m("a.b.c"), "plain");
        assert_eq!(m(r#"a."b.c""#), "quoted");
        assert_eq!(m(r#"a["b.c"]"#), "quoted");
        assert_eq!(m(r#"a["b"].c"#), "plain");
        assert_eq!(m("a.x~1y"), "slash");
        assert_eq!(m("a.t~0u"), "tilde");
        assert_eq!(m("l.1"), "one");
        assert_eq!(m("l.0"), "zero");
        assert_eq!(m("l.+1"), "", "parse_index rejects a leading +");
        assert_eq!(m("l.01"), "", "parse_index rejects a leading zero");
        assert_eq!(m("é.x"), "accent", "Tera 1 panicked here; we resolve it");
        // Empty path: the element itself (null elements dropped by `map`).
        assert_eq!(
            r(
                "{{ [1, none, 2] | map(attribute='') | join(sep=',') }}",
                json!({})
            ),
            "1,2"
        );
        assert_eq!(
            r(
                r#"{{ xs | filter(attribute='a."b.c"', value='quoted') | length }}"#,
                ctx.clone()
            ),
            "1"
        );
    }

    #[test]
    fn unique_like_tera1() {
        assert_eq!(
            r(
                "{{ [3, -1, 3, 3, 5, 2, 5, 4] | unique | join(sep=',') }}",
                json!({})
            ),
            "3,-1,5,2,4"
        );
        let words = json!({"w": ["One", "Two", "Three", "one", "Two"]});
        assert_eq!(
            r("{{ w | unique | join(sep=',') }}", words.clone()),
            "One,Two,Three",
            "case-insensitive by default"
        );
        assert_eq!(
            r(
                "{{ w | unique(case_sensitive=true) | join(sep=',') }}",
                words
            ),
            "One,Two,Three,one"
        );
        let foos =
            json!({"f": [{"a": 1, "b": 2}, {"a": 3, "b": 3}, {"a": 1, "b": 3}, {"a": 0, "b": 4}]});
        assert_eq!(
            r(
                "{{ f | unique(attribute='a') | map(attribute='b') | join(sep=',') }}",
                foos.clone()
            ),
            "2,3,4"
        );
        assert_eq!(r("{{ [] | unique | length }}", json!({})), "0");
        assert_eq!(
            r(
                "{{ [true, false, true] | unique | join(sep=',') }}",
                json!({})
            ),
            "true,false"
        );
        for (tpl, needle) in [
            (
                "{{ f | unique(attribute='zz') }}",
                "does not reference a field",
            ),
            ("{{ [12, []] | unique }}", "can't compare multiple types"),
            (
                "{{ [1.5, 2.5] | unique }}",
                "Unique floats are not implemented",
            ),
            ("{{ [none] | unique }}", "Null is not a unique value"),
            ("{{ [[1]] | unique }}", "Unique arrays are not implemented"),
        ] {
            assert!(
                crate::template_error::raw_detail_contains(tpl, &foos, needle),
                "{tpl}"
            );
        }
    }

    #[test]
    fn indent_like_tera1() {
        let ctx = json!({"s": "one\n\ntwo\nthree", "ws": "a\n  \nb\n", "tab": "\t"});
        assert_eq!(
            r("{{ s | indent }}", ctx.clone()),
            "one\n\n    two\n    three"
        );
        assert_eq!(
            r(
                "{{ s | indent(first=true, prefix=' ', blank=true) }}",
                ctx.clone()
            ),
            " one\n \n two\n three"
        );
        assert_eq!(
            r("{{ s | indent(prefix='> ') }}", ctx.clone()),
            "one\n\n> two\n> three"
        );
        assert_eq!(
            r("{{ s | indent(first=true) }}", ctx.clone()),
            "    one\n\n    two\n    three"
        );
        // Tera 1 line handling: a whitespace-only line counts as blank and
        // the trailing newline is dropped.
        assert_eq!(r("{{ ws | indent }}", ctx.clone()), "a\n  \n    b");
        assert_eq!(
            r("{{ ws | indent(blank=true) }}", ctx.clone()),
            "a\n      \n    b"
        );
        // Tera 2 arguments keep working.
        assert_eq!(
            r("{{ s | indent(width=2) }}", ctx.clone()),
            "one\n\n  two\n  three"
        );
        assert_eq!(
            r("{{ s | indent(width=1, indentation=tab) }}", ctx.clone()),
            "one\n\n\ttwo\n\tthree"
        );
        assert_eq!(
            r("{{ s | indent(prefix='-', width=8) }}", ctx.clone()),
            "one\n\n-two\n-three",
            "prefix= wins over width="
        );
        assert!(render_template("{{ s | indent(indentation='ab') }}", &ctx).is_err());
    }

    #[test]
    fn date_like_tera1() {
        assert_eq!(r("{{ 0 | date }}", json!({})), "1970-01-01");
        assert_eq!(
            r(
                "{{ '2026-10-06T12:30:00+02:00' | date(format='%H:%M', timezone='UTC') }}",
                json!({})
            ),
            "10:30"
        );
        assert_eq!(
            r("{{ '2026-10-06' | date(format='%d/%m') }}", json!({})),
            "06/10"
        );
    }

    #[test]
    fn now_returns_rfc3339_or_timestamp() {
        let s = r("{{ now(utc=true) }}", json!({}));
        assert!(chrono::DateTime::parse_from_rfc3339(&s).is_ok(), "{s}");
        let ts: i64 = r("{{ now(timestamp=true) }}", json!({})).parse().unwrap();
        assert!(ts > 1_700_000_000);
    }

    #[test]
    fn compat_errors_are_value_free() {
        let err = render_template(
            "{{ secret.X | date }}",
            &json!({"secret": {"X": "not-a-date-canary"}}),
        )
        .unwrap_err();
        assert!(
            crate::template_error::raw_detail_contains(
                "{{ secret.X | date }}",
                &json!({"secret": {"X": "not-a-date-canary"}}),
                "not-a-date-canary"
            ),
            "fixture must reach the date filter's error"
        );
        assert!(!format!("{err:#}").contains("not-a-date-canary"));
        assert!(!format!("{err:?}").contains("not-a-date-canary"));
    }
}
