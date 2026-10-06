//! The one configured Tera (spec 2026-10-06 § 3.1).

use crate::budget::LoadBudget;
use crate::template_error::ValsFailure;
use std::sync::{Arc, LazyLock, Mutex};

/// Builtins plus the filters we promise users. Cloned per render.
static BASE: LazyLock<tera::Tera> = LazyLock::new(|| {
    let mut t = tera::Tera::default();
    t.register_filter("json_encode", tera_contrib::json::json_encode);
    crate::tera_compat::register(&mut t);
    t
});

#[allow(dead_code)] // no caller yet; part of the module's interface (plan Task 1)
pub(crate) fn base() -> &'static tera::Tera {
    &BASE
}

/// Engine for rendering: `vals` resolves `ref+` references under `budget`
/// and records a failure in `slot`.
pub(crate) fn render_engine(
    budget: LoadBudget,
    slot: Arc<Mutex<Option<ValsFailure>>>,
) -> tera::Tera {
    let mut t = BASE.clone();
    t.register_filter(
        crate::template::VALS_FILTER,
        move |v: &tera::Value, _kw: tera::Kwargs, _st: &tera::State| {
            crate::template::vals_filter_with(v, budget, &slot)
        },
    );
    t
}

/// Engine for compile checks: `vals` is the identity, so validation never
/// starts a subprocess.
pub(crate) fn check_engine() -> tera::Tera {
    let mut t = BASE.clone();
    t.register_filter(
        crate::template::VALS_FILTER,
        |v: &tera::Value, _kw: tera::Kwargs, _st: &tera::State| -> tera::TeraResult<tera::Value> {
            Ok(v.clone())
        },
    );
    t
}
