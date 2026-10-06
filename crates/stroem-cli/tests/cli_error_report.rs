//! The operator's view (spec 2026-10-06 § 3.2.3): every top-level error of
//! `stroem validate` / `stroem run` carries Tera's full detail, while what a
//! downstream template sees as `<step>.error` stays value-free.

use std::process::{Command, Output};

fn stroem(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stroem"))
        .arg("--path")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
}

fn workspace(yaml: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("w.yaml"), yaml).unwrap();
    dir
}

/// A secret whose template fails at load: the load error reaches `main`
/// through `?` (validate's `check_workspace`, run's `load_workspace`).
const LOAD_FAILS: &str = r#"
secrets:
  X: "{{ 'not-a-number-cli' | int }}"
actions:
  ok: { type: script, script: echo ok }
tasks:
  t:
    flow:
      a: { action: ok }
"#;

#[test]
fn validate_load_error_prints_tera_detail() {
    let dir = workspace(LOAD_FAILS);
    let out = stroem(dir.path(), &["validate"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("Failed to render workspace secrets"),
        "{stderr}"
    );
    assert!(stderr.contains("Tera detail"), "{stderr}");
    assert!(stderr.contains("not-a-number-cli"), "{stderr}");
}

#[test]
fn run_load_error_prints_tera_detail() {
    let dir = workspace(LOAD_FAILS);
    let out = stroem(dir.path(), &["run", "t"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("Tera detail"), "{stderr}");
    assert!(stderr.contains("not-a-number-cli"), "{stderr}");
}

#[test]
fn run_setup_error_prints_tera_detail() {
    // A task input default that fails to render: merge_defaults in cmd_run.
    let dir = workspace(
        r#"
actions:
  ok: { type: script, script: echo ok }
tasks:
  t:
    input:
      n: { type: string, default: "{{ 'default-not-a-number' | int }}" }
    flow:
      a: { action: ok }
"#,
    );
    let out = stroem(dir.path(), &["run", "t"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("Failed to merge input defaults"),
        "{stderr}"
    );
    assert!(stderr.contains("Tera detail"), "{stderr}");
}

#[test]
fn step_error_seen_downstream_is_value_free_while_terminal_gets_detail() {
    let dir = workspace(
        r#"
actions:
  show:
    type: script
    script: 'printf "ERR=[%s]\n" "$ERR"'
    env:
      ERR: "{{ a.error | default(value='') }}"
tasks:
  t:
    flow:
      a: { action: show, when: "{{ 'when-not-a-number' | int }}", continue_on_failure: true }
      b: { action: show, depends_on: [{ step: a, accept: [failed] }] }
"#,
    );
    let out = stroem(dir.path(), &["run", "t"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    let line = stdout
        .lines()
        .find(|l| l.starts_with("ERR=["))
        .unwrap_or_else(|| panic!("b printed nothing: {stdout}\n{stderr}"));
    // One line, value-free: the old full report spanned several lines.
    assert!(line.starts_with("ERR=[when condition error: "), "{line}");
    assert!(line.ends_with(']'), "{stdout}");
    // Script output is the only stdout: nothing of Tera's report reached `b`.
    assert!(!stdout.contains("Tera detail"), "{stdout}");
    assert!(!stdout.contains("when-not-a-number"), "{stdout}");
    assert!(stderr.contains("Tera detail"), "{stderr}");
    assert!(stderr.contains("when-not-a-number"), "{stderr}");
}

#[test]
fn run_renders_first_run_state_idioms() {
    let dir = workspace(
        r#"
actions:
  show:
    type: script
    script: 'echo "cursor={{ state.cursor | default(value=0) }} global={{ global_state.x | default(value=1) }} rev=[{{ job.revision }}]"'
tasks:
  t:
    flow:
      a: { action: show }
"#,
    );
    let out = stroem(dir.path(), &["run", "t"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("cursor=0 global=1 rev=[]"),
        "{stdout}\n{stderr}"
    );
}
