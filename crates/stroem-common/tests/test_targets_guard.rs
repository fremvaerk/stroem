//! One integration-test binary per crate: `tests/main.rs`, target
//! `integration`, `autotests = false`. With `autotests` off, cargo ignores a
//! file under `tests/` that nobody declares — its tests would silently never
//! run — so this walks every crate and fails on such a file.

use std::path::Path;

/// The module names a `tests/main.rs` declares (`mod x;`, `pub mod x;`).
fn declared_modules(main_rs: &str) -> Vec<String> {
    main_rs
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let l = l.strip_prefix("pub ").unwrap_or(l);
            Some(
                l.strip_prefix("mod ")?
                    .strip_suffix(';')?
                    .trim()
                    .to_string(),
            )
        })
        .collect()
}

/// The test files allowed to be a binary of their own, as `(crate, file)`.
/// Adding one is a decision a reviewer should see, not a Cargo.toml edit.
const OWN_BINARY: &[(&str, &str)] = &[
    // `harness = false` with a counting global allocator, which would count
    // every other test's allocations in a shared binary.
    ("stroem-server", "log_peak_alloc_test.rs"),
];

/// May `file` — a top-level `tests/*.rs` of `crate_name` that `tests/main.rs`
/// does not declare — be a test binary of its own? Only if it is listed in
/// [`OWN_BINARY`] AND declared as a `[[test]]` target in `manifest` (the
/// crate's Cargo.toml), since with `autotests = false` an undeclared file is
/// never compiled.
fn allowed_outside_integration(crate_name: &str, file: &str, manifest: &str) -> bool {
    OWN_BINARY.contains(&(crate_name, file))
        && manifest.contains(&format!(r#"path = "tests/{file}""#))
}

#[test]
fn every_test_file_is_in_its_crates_integration_binary() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut walked = Vec::new();
    let mut problems = Vec::new();

    for entry in std::fs::read_dir(crates).unwrap() {
        let dir = entry.unwrap().path();
        let tests = dir.join("tests");
        if !tests.is_dir() {
            continue;
        }
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        if !manifest.lines().any(|l| l.trim() == "autotests = false") {
            problems.push(format!("{name}: Cargo.toml lacks `autotests = false`"));
        }
        if !manifest.contains(r#"path = "tests/main.rs""#) {
            problems.push(format!("{name}: no [[test]] target at tests/main.rs"));
        }
        let main_rs = std::fs::read_to_string(tests.join("main.rs")).unwrap_or_default();
        let declared = declared_modules(&main_rs);

        for f in std::fs::read_dir(&tests).unwrap() {
            let p = f.unwrap().path();
            let Some(stem) = p.extension().filter(|e| *e == "rs").and(p.file_stem()) else {
                continue;
            };
            let stem = stem.to_string_lossy();
            let file = format!("{stem}.rs");
            if stem == "main"
                || declared.iter().any(|m| *m == stem)
                || allowed_outside_integration(&name, &file, &manifest)
            {
                continue;
            }
            problems.push(format!(
                "{name}: tests/{file} is not declared in tests/main.rs, so it never runs"
            ));
        }
        walked.push(name);
    }

    // Vacuity: the walk reached the crates that have integration tests.
    for expected in ["stroem-common", "stroem-db", "stroem-server"] {
        assert!(
            walked.iter().any(|w| w == expected),
            "the walk never reached {expected}/tests (walked: {walked:?})"
        );
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn declared_modules_reads_mod_lines_only() {
    let src = "//! doc\n\nmod common;\npub mod pinned;\n#[cfg(feature = \"s3\")]\nmod minio;\nmod inline {}\nuse x;\n";
    assert_eq!(declared_modules(src), ["common", "pinned", "minio"]);
}

#[test]
fn own_binary_needs_the_list_and_a_test_target() {
    let m = "[[test]]\nname = \"log_peak_alloc_test\"\npath = \"tests/log_peak_alloc_test.rs\"\n";
    assert!(allowed_outside_integration(
        "stroem-server",
        "log_peak_alloc_test.rs",
        m
    ));
    // Listed but not a target: cargo would never compile it.
    assert!(!allowed_outside_integration(
        "stroem-server",
        "log_peak_alloc_test.rs",
        ""
    ));
    // A target but not listed, or listed for another crate.
    let other = "[[test]]\nname = \"other\"\npath = \"tests/other.rs\"\n";
    assert!(!allowed_outside_integration(
        "stroem-server",
        "other.rs",
        other
    ));
    assert!(!allowed_outside_integration(
        "stroem-db",
        "log_peak_alloc_test.rs",
        m
    ));
}
