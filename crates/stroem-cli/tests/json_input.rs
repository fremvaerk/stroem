use std::process::{Command, Output};

fn stroem(yaml: &str, args: &[&str]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("w.yaml"), yaml).unwrap();
    let mut full = vec!["--path", dir.path().to_str().unwrap()];
    full.extend_from_slice(args);
    Command::new(env!("CARGO_BIN_EXE_stroem"))
        .args(&full)
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

const OK: &str = r#"
secrets:
  PORT: 5432
actions:
  emit:
    type: script
    script: |
      echo 'OUTPUT: {"cfg": {"region": "eu"}, "items": [1, 2, 3]}'
  check:
    type: script
    script: |
      [ "{{ input.cfg.region }}" = "eu" ] || exit 11
      [ "{{ input.count + 1 }}" = "4" ] || exit 12
      [ "{{ input.db.port + 1 }}" = "5433" ] || exit 13
    input:
      cfg: { type: json }
      count: { type: json }
      db: { type: json, default: { port: "{{ secret.PORT }}" } }
tasks:
  t:
    flow:
      a: { action: emit }
      b:
        action: check
        depends_on: [a]
        input:
          cfg: "{{ a.output.cfg }}"
          count: "{{ a.output.items | length }}"
"#;

#[test]
fn run_passes_native_values_through_json_fields_and_defaults() {
    let o = stroem(OK, &["run", "t"]);
    assert!(o.status.success(), "{}", text(&o));
}

#[test]
fn run_fails_a_mixed_template_in_a_json_field() {
    let yaml = OK.replace(
        "count: \"{{ a.output.items | length }}\"",
        "count: \"n={{ a.output.items | length }}\"",
    );
    let o = stroem(&yaml, &["run", "t"]);
    assert!(!o.status.success());
    assert!(
        text(&o).contains("a json field takes a literal value"),
        "{}",
        text(&o)
    );
}

#[test]
fn validate_reports_json_errors_and_warnings() {
    let mixed = OK.replace(
        "cfg: \"{{ a.output.cfg }}\"",
        "cfg: \"x {{ a.output.cfg }}\"",
    );
    let o = stroem(&mixed, &["validate"]);
    assert!(!o.status.success());
    assert!(
        text(&o).contains("Task 't' step 'b' input 'cfg'"),
        "{}",
        text(&o)
    );

    let o = stroem(OK, &["validate"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(
        text(&o).contains("secret 'PORT' is a number"),
        "{}",
        text(&o)
    );
}
