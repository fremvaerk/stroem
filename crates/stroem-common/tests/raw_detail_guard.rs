//! Only stroem-cli may call TemplateError::raw_detail (spec § 3.2.3).

use std::path::Path;

fn visit(dir: &Path, hits: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.is_dir() {
            visit(&p, hits);
        } else if p.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&p).unwrap();
            if text.contains("raw_detail(") {
                hits.push(p.display().to_string());
            }
        }
    }
}

#[test]
fn only_the_cli_and_template_error_mention_raw_detail() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut hits = Vec::new();
    visit(crates, &mut hits);
    let allowed = |p: &String| {
        p.contains("/stroem-cli/")
            || p.ends_with("stroem-common/src/template_error.rs")
            // test module (fixture proof, R8)
            || p.ends_with("stroem-common/src/tera_compat.rs")
            || p.ends_with("stroem-common/tests/raw_detail_guard.rs")
    };
    let offenders: Vec<_> = hits.iter().filter(|p| !allowed(p)).collect();
    assert!(
        offenders.is_empty(),
        "raw_detail used outside stroem-cli: {offenders:?}"
    );
}

#[test]
fn template_rs_contexts_never_interpolate_template_text() {
    let src =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/template.rs"))
            .unwrap();
    for (i, line) in src.lines().enumerate() {
        let l = line.trim();
        if (l.contains("context(") || l.contains("with_context(")) && l.contains("format!") {
            for var in ["{}\", s", "{s}", "template)", "{template}", "src)", "{src}"] {
                assert!(
                    !l.contains(var),
                    "template.rs:{}: context interpolates template text: {l}",
                    i + 1
                );
            }
        }
    }
}
