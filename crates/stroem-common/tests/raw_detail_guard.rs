//! Only stroem-cli may call TemplateError::raw_detail (spec § 3.2.3).

use std::path::{Path, PathBuf};

/// Does `text` call or name `raw_detail`? Whitespace is removed first, so
/// `te.raw_detail ()` and a split `TemplateError::\n raw_detail` count; a
/// hit is the bare identifier (not `raw_detail_contains`) followed by `(`
/// or reached through a path (`TemplateError::raw_detail`, `<T>::raw_detail`).
fn mentions_raw_detail(text: &str) -> bool {
    const NAME: &str = "raw_detail";
    let squashed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    squashed.match_indices(NAME).any(|(at, _)| {
        let before = &squashed[..at];
        let after = &squashed[at + NAME.len()..];
        if before.chars().next_back().is_some_and(is_ident)
            || after.chars().next().is_some_and(is_ident)
        {
            return false;
        }
        after.starts_with('(') || before.ends_with("::")
    })
}

fn visit(dir: &Path, seen: &mut Vec<PathBuf>, hits: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.is_dir() {
            visit(&p, seen, hits);
        } else if p.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&p).unwrap();
            if mentions_raw_detail(&text) {
                hits.push(p.display().to_string());
            }
            seen.push(p);
        }
    }
}

#[test]
fn only_the_cli_and_template_error_mention_raw_detail() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut seen = Vec::new();
    let mut hits = Vec::new();
    visit(crates, &mut seen, &mut hits);

    // Vacuity check: the walk reached the definition and the matcher
    // recognised it, so an empty offender list means something.
    assert!(
        seen.iter()
            .any(|p| p.ends_with("stroem-common/src/template_error.rs")),
        "the walk never visited template_error.rs"
    );
    assert!(
        hits.iter()
            .any(|p| p.ends_with("stroem-common/src/template_error.rs")),
        "the matcher no longer recognises raw_detail's own definition: {hits:?}"
    );

    let allowed = |p: &String| {
        p.contains("/stroem-cli/")
            // definition + its #[cfg(test)] proof helper `raw_detail_contains`
            || p.ends_with("stroem-common/src/template_error.rs")
            // #[cfg(test)] module file: the server's proof helper
            || p.ends_with("stroem-server/src/test_support.rs")
            // integration-test fixtures (test code only)
            || p.ends_with("stroem-server/tests/common/tera_fixtures.rs")
            || p.ends_with("stroem-common/tests/raw_detail_guard.rs")
    };
    let offenders: Vec<_> = hits.iter().filter(|p| !allowed(p)).collect();
    assert!(
        offenders.is_empty(),
        "raw_detail used outside stroem-cli: {offenders:?}"
    );
}

#[test]
fn the_matcher_catches_spacing_and_path_forms() {
    assert!(mentions_raw_detail("te.raw_detail()"));
    assert!(mentions_raw_detail("te.raw_detail ()"));
    assert!(mentions_raw_detail("te\n    .raw_detail\n    ()"));
    assert!(mentions_raw_detail(".map(TemplateError::raw_detail)"));
    assert!(mentions_raw_detail(".map(TemplateError :: raw_detail)"));
    assert!(mentions_raw_detail("<TemplateError>::raw_detail(&te)"));
    assert!(!mentions_raw_detail("raw_detail_contains(tpl, ctx, n)"));
    assert!(!mentions_raw_detail(
        "tera_fixtures::raw_detail_contains(t)"
    ));
    assert!(!mentions_raw_detail("// see raw_detail_guard.rs"));
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
