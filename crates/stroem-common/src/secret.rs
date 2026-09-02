//! Log-safe wrappers for sensitive values.
//!
//! Re-exports [`redact::Secret`], whose `Debug` prints `[REDACTED T]` instead
//! of the value. Wrap any field that can carry a rendered secret (action specs
//! with `env`, resolved connection inputs, agent prompts/state, MCP server
//! definitions, step output, log lines) so a `#[tracing::instrument]` span or
//! a stray `{:?}` can never leak it. Reads go through `expose_secret()`, which
//! doubles as a grep-able audit trail of where secrets are unwrapped.
//!
//! `Deserialize` is transparent. `Serialize` must be opted into explicitly so
//! the wire format stays unchanged — use [`serialize_opt_secret`] for
//! `Option<Secret<T>>` fields and [`redact::expose_secret`] for bare `Secret<T>`.

pub use redact::{expose_secret, Secret};
use serde::{Serialize, Serializer};

/// Serialize an `Option<Secret<T>>` by exposing the inner value.
///
/// Use as `#[serde(serialize_with = "stroem_common::secret::serialize_opt_secret")]`.
pub fn serialize_opt_secret<S, T>(
    value: &Option<Secret<T>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: Serialize,
{
    match value {
        Some(secret) => secret.expose_secret().serialize(serializer),
        None => serializer.serialize_none(),
    }
}

/// Take the inner value out of a `Secret` for an owned consumer (DB write,
/// job input). `redact` deliberately offers no owned accessor, so this clones.
pub fn into_exposed<T: Clone>(secret: Secret<T>) -> T {
    secret.expose_secret().clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    const SENTINEL: &str = "SUPER-SECRET-VALUE-7f3a";

    #[derive(Debug, Serialize, Deserialize)]
    struct Carrier {
        name: String,
        #[serde(default, serialize_with = "serialize_opt_secret")]
        spec: Option<Secret<serde_json::Value>>,
        #[serde(default, serialize_with = "serialize_opt_secret")]
        prompt: Option<Secret<String>>,
    }

    fn carrier() -> Carrier {
        Carrier {
            name: "publish".to_string(),
            spec: Some(Secret::new(serde_json::json!({"env": {"PW": SENTINEL}}))),
            prompt: Some(Secret::new(format!("use {SENTINEL}"))),
        }
    }

    #[test]
    fn debug_redacts_wrapped_fields_but_keeps_identity() {
        let dbg = format!("{:?}", carrier());
        assert!(!dbg.contains(SENTINEL), "leaked: {dbg}");
        assert!(dbg.contains("publish"));
        assert!(dbg.contains("REDACTED"));
    }

    #[test]
    fn serialize_opt_secret_exposes_value_on_the_wire() {
        let json = serde_json::to_value(carrier()).unwrap();
        assert_eq!(json["spec"]["env"]["PW"], SENTINEL);
        assert_eq!(json["prompt"], format!("use {SENTINEL}"));
    }

    #[test]
    fn serialize_opt_secret_none_is_null() {
        let c = Carrier {
            name: "x".into(),
            spec: None,
            prompt: None,
        };
        let json = serde_json::to_value(c).unwrap();
        assert!(json["spec"].is_null());
        assert!(json["prompt"].is_null());
    }

    #[test]
    fn deserialize_is_transparent_and_missing_field_is_none() {
        let c: Carrier = serde_json::from_value(serde_json::json!({
            "name": "x", "spec": {"k": SENTINEL}
        }))
        .unwrap();
        assert_eq!(c.spec.as_ref().unwrap().expose_secret()["k"], SENTINEL);
        assert!(c.prompt.is_none());
    }

    #[test]
    fn into_exposed_returns_inner_value() {
        let s = Secret::new(serde_json::json!({"k": SENTINEL}));
        assert_eq!(into_exposed(s)["k"], SENTINEL);
    }
}
