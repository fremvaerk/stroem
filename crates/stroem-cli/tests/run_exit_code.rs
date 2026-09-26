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
fn caught_failure_exits_zero() {
    assert!(run(r#"
actions:
  fail: { type: script, script: exit 1 }
  ok: { type: script, script: echo ok }
tasks:
  t:
    flow:
      a: { action: fail }
      b: { action: ok, depends_on: [a], continue_on_failure: true }
      c: { action: ok, depends_on: [b] }
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
