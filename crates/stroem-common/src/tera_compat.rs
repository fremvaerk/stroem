//! Tera 1 behaviour kept under Tera 2 (spec 2026-10-06 § 3.6 C1, C3).
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

/// tera 1 `dotted_pointer`, without its quoted-segment syntax.
fn dotted<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.').try_fold(v, |cur, seg| match cur {
        serde_json::Value::Object(m) => m.get(seg),
        serde_json::Value::Array(a) => seg.parse::<usize>().ok().and_then(|i| a.get(i)),
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
