//! Only stroem-cli may call TemplateError::raw_detail (spec § 3.2.3).

use std::path::{Path, PathBuf};

/// Splits `text` into tokens, whitespace dropped: an identifier
/// (`[A-Za-z0-9_]+`) is one token, every other character its own.
fn tokens(text: &str) -> Vec<&str> {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        let mut end = start + c.len_utf8();
        if is_ident(c) {
            while let Some(&(i, n)) = chars.peek() {
                if !is_ident(n) {
                    break;
                }
                end = i + n.len_utf8();
                chars.next();
            }
        }
        out.push(&text[start..end]);
    }
    out
}

/// Does `text` define, call or name `raw_detail`? Matched on tokens, so
/// whitespace and line breaks anywhere (`te.raw_detail ()`, a split
/// `TemplateError::\n raw_detail`) do not matter and `raw_detail_contains`
/// is a different identifier. A hit is the `raw_detail` token followed by
/// `(`, reached through a path (`TemplateError::raw_detail`,
/// `<T>::raw_detail`), or declared (`fn raw_detail`).
fn mentions_raw_detail(text: &str) -> bool {
    let toks = tokens(text);
    toks.iter().enumerate().any(|(i, &t)| {
        t == "raw_detail"
            && (toks.get(i + 1) == Some(&"(")
                || (i >= 2 && toks[i - 1] == ":" && toks[i - 2] == ":")
                || (i >= 1 && toks[i - 1] == "fn"))
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

    // Vacuity check: the walk reached the defining file, the matcher
    // recognises the definition line itself (not only some other mention in
    // that file), and the file is reported, so an empty offender list means
    // something.
    assert!(
        seen.iter()
            .any(|p| p.ends_with("stroem-common/src/template_error.rs")),
        "the walk never visited template_error.rs"
    );
    let def_src = std::fs::read_to_string(crates.join("stroem-common/src/template_error.rs"))
        .expect("read template_error.rs");
    let def_line = def_src
        .lines()
        .find(|l| l.trim_start().starts_with("pub fn raw_detail("))
        .expect("raw_detail's definition in template_error.rs");
    assert!(
        mentions_raw_detail(def_line),
        "the matcher does not recognise raw_detail's own definition: {def_line}"
    );
    assert!(
        hits.iter()
            .any(|p| p.ends_with("stroem-common/src/template_error.rs")),
        "template_error.rs is not reported although it defines raw_detail: {hits:?}"
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
    // The definition, however it is spaced, and a call after a keyword
    // (whitespace squashing used to glue `fn`/`return` onto the name).
    assert!(mentions_raw_detail("pub fn raw_detail(&self) -> String {"));
    assert!(mentions_raw_detail("pub fn\n    raw_detail\n    (&self)"));
    assert!(mentions_raw_detail("return raw_detail(&te);"));
    // Not hits: a bare mention, a field, a longer identifier, another
    // function whose name ends in `raw_detail`.
    assert!(!mentions_raw_detail("the raw_detail is CLI-only"));
    assert!(!mentions_raw_detail("let d = te.raw_detail;"));
    assert!(!mentions_raw_detail("fn raw_detail_contains(tpl: &str)"));
    assert!(!mentions_raw_detail("fn my_raw_detail()"));
    assert!(!mentions_raw_detail("x.not_raw_detail()"));
    assert!(!mentions_raw_detail("é raw_details()"));
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
