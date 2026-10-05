use std::process::Command;

fn run(yaml: &str) -> std::process::ExitStatus {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("w.yaml"), yaml).unwrap();
    Command::new(env!("CARGO_BIN_EXE_stroem"))
        .args(["--path", dir.path().to_str().unwrap(), "run", "t"])
        .status()
        .unwrap()
}

#[test]
fn downstream_continue_on_failure_no_longer_catches_an_upstream_failure() {
    // Under 0.17.0's structural catch, b's own continue_on_failure used to
    // excuse a's failure for job-status purposes even though a itself had
    // no flag. Spec 2026-10-01 §6 retires this: only a failing step's OWN
    // flag excuses it. a has none, so the run must now exit non-zero.
    let s = run(r#"
actions:
  fail: { type: script, script: exit 1 }
  ok: { type: script, script: echo ok }
tasks:
  t:
    flow:
      a: { action: fail }
      b: { action: ok, depends_on: [{ step: a, accept: [failed] }], continue_on_failure: true }
      c: { action: ok, depends_on: [b] }
"#);
    assert_eq!(s.code(), Some(1));
}

#[test]
fn own_continue_on_failure_exits_zero() {
    // The self-scoped replacement: a's OWN flag (not a downstream step's)
    // excuses a's failure, so the run exits 0.
    assert!(run(r#"
actions:
  fail: { type: script, script: exit 1 }
  ok: { type: script, script: echo ok }
tasks:
  t:
    flow:
      a: { action: fail, continue_on_failure: true }
      b: { action: ok, depends_on: [{ step: a, accept: [failed] }] }
"#)
    .success());
}

#[test]
fn uncaught_failure_exits_one() {
    let s = run(r#"
actions:
  fail: { type: script, script: exit 1 }
tasks:
  t:
    flow:
      a: { action: fail }
"#);
    assert_eq!(s.code(), Some(1));
}

#[cfg(unix)]
#[test]
fn ctrl_c_exits_non_zero() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("w.yaml"),
        r#"
actions:
  slow: { type: script, script: sleep 30 }
tasks:
  t:
    flow:
      a: { action: slow }
"#,
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_stroem"))
        .args(["--path", dir.path().to_str().unwrap(), "run", "t"])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(!child.wait().unwrap().success());
}
