//! Templates whose RAW Tera error demonstrably carries the secret
//! (spec 2026-10-06 § 3.5): every security test that uses one first asserts
//! [`raw_detail_contains`] on it, and `fixtures_are_real` below pins both,
//! so a test asserting the secret is absent from our output is never vacuous.
//!
//! Test code only (`crate::common::tera_fixtures`) — with
//! `src/test_support.rs` it is one of the two server files the `raw_detail`
//! guard (`stroem-common/tests/raw_detail_guard.rs`) allows.

#![allow(dead_code)]

use stroem_common::template::render_template;
use stroem_common::template_error::TemplateError;

/// Raw: `round(method=X)` quotes X verbatim in Tera's error.
pub fn quoting_round_method(secret_path: &str) -> String {
    format!("{{{{ 1 | round(method={secret_path}) }}}}")
}

/// Transformed: `upper` then `int` quotes the UPPER-CASED value — a form no
/// exact-value scrub matches.
pub fn quoting_upper_int(secret_path: &str) -> String {
    format!("{{{{ {secret_path} | upper | int }}}}")
}

/// True when rendering `tpl` against `ctx` fails and Tera's raw detail
/// contains `needle`. The raw detail quotes the template's source line, so a
/// needle occurring in `tpl` itself would prove nothing: refused.
pub fn raw_detail_contains(tpl: &str, ctx: &serde_json::Value, needle: &str) -> bool {
    assert!(
        !tpl.contains(needle),
        "proof needle occurs in the template source, so the source line alone would satisfy it"
    );
    let err = render_template(tpl, ctx).expect_err("the fixture must fail to render");
    err.chain()
        .find_map(|c| c.downcast_ref::<TemplateError>())
        .is_some_and(|te| te.raw_detail().contains(needle))
}

#[test]
fn fixtures_are_real() {
    const CANARY: &str = "fixture-canary-5d2e";
    let ctx = serde_json::json!({ "secret": { "X": CANARY } });
    for (tpl, leaked) in [
        (quoting_round_method("secret.X"), CANARY.to_string()),
        (quoting_upper_int("secret.X"), CANARY.to_uppercase()),
    ] {
        assert!(
            raw_detail_contains(&tpl, &ctx, &leaked),
            "{tpl}: Tera's raw error no longer quotes the value"
        );
        let err = render_template(&tpl, &ctx).unwrap_err();
        let ours = format!("{err:#} {err:?}");
        assert!(!ours.contains(CANARY), "{tpl}: {ours}");
        assert!(!ours.contains(&CANARY.to_uppercase()), "{tpl}: {ours}");
    }
}
