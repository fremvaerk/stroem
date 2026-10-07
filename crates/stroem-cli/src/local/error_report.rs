//! The operator's view of an error: our value-free chain plus Tera's full
//! report (the operator holds every secret, spec § 3.2.3).

use stroem_common::template_error::TemplateError;

pub fn full_report(err: &anyhow::Error) -> String {
    let mut out = format!("{err:#}");
    for cause in err.chain() {
        if let Some(te) = cause.downcast_ref::<TemplateError>() {
            out.push_str("\n  Tera detail:\n");
            if te.raw_detail().contains("__stroem_v") {
                out.push_str(
                    "    (this is the json field's evaluated form of the expression: \
                     {% set __stroem_v = <expr> %}...)\n",
                );
            }
            for line in te.raw_detail().lines() {
                out.push_str("    ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn full_report_appends_tera_detail() {
        let err =
            stroem_common::template::render_template("{{ nosuchvar_cli }}", &serde_json::json!({}))
                .unwrap_err();
        let r = super::full_report(&err);
        assert!(r.contains("undefined variable or field"), "{r}");
        assert!(r.contains("nosuchvar_cli"), "{r}");
    }

    #[test]
    fn full_report_labels_the_json_wrapper_detail() {
        let err = stroem_common::json_field::render_json_value(
            &serde_json::json!("{{ x | int }}"),
            "f",
            &serde_json::json!({"x": "abc"}),
        )
        .unwrap_err();
        let te = err
            .chain()
            .find_map(|c| c.downcast_ref::<stroem_common::template_error::TemplateError>())
            .expect("a TemplateError");
        let raw = te.raw_detail();
        assert!(raw.contains("__stroem_v"), "{raw}");
        let r = super::full_report(&err);
        assert!(r.contains("evaluated form of the expression"), "{r}");
    }
}
