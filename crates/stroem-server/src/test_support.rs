//! Test-only helpers shared by the server's unit tests. The whole module is
//! `#[cfg(test)]` (see `lib.rs`), so production code cannot reach it — which
//! is why the `raw_detail` guard (`stroem-common/tests/raw_detail_guard.rs`)
//! allows this file and no production one.

/// Proof that a security fixture is real (spec 2026-10-06 § 3.5): true when
/// rendering `tpl` against `ctx` fails and Tera's RAW detail — the text our
/// value-free `TemplateError` never shows — contains `needle`. A test
/// asserting a secret is absent from our output first asserts this, so it
/// cannot pass vacuously. The raw detail quotes the template's source line,
/// so a needle occurring in `tpl` itself would prove nothing: refused.
pub(crate) fn tera_raw_detail_contains(tpl: &str, ctx: &serde_json::Value, needle: &str) -> bool {
    assert!(
        !tpl.contains(needle),
        "proof needle occurs in the template source, so the source line alone would satisfy it"
    );
    let err = stroem_common::template::render_template(tpl, ctx)
        .expect_err("the fixture must fail to render");
    err.chain()
        .find_map(|c| c.downcast_ref::<stroem_common::template_error::TemplateError>())
        .is_some_and(|te| te.raw_detail().contains(needle))
}
