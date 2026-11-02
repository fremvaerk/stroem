use std::process::{Command, Output};

const YAML: &str = r#"
actions:
  show:
    type: script
    input:
      data_file: { type: file, required: true }
    script: 'cat "{{ input.data_file }}"; echo "path={{ input.data_file }}"; if [ -w "{{ input.data_file }}" ]; then echo writable; else echo readonly; fi'
tasks:
  t:
    input:
      data_file: { type: file, required: true }
    flow:
      a:
        action: show
        input:
          data_file: "{{ input.data_file }}"
  opt:
    input:
      f: { type: file }
    flow:
      a:
        action: show
        when: "{{ input.f | default(value='') }}"
        input:
          data_file: "{{ input.f }}"
"#;

fn run(task: &str, input: &str) -> (Output, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("w.yaml"), YAML).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_stroem"))
        .args(["--path", dir.path().to_str().unwrap(), "run", task])
        .args(["--input", input])
        .output()
        .unwrap();
    (out, dir)
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn input_for(path: &std::path::Path) -> String {
    serde_json::json!({ "data_file": path.to_str().unwrap() }).to_string()
}

#[test]
fn run_file_field_renders_local_path_and_reads_file() {
    let data = tempfile::tempdir().unwrap();
    let f = data.path().join("report.csv");
    std::fs::write(&f, "hello-file-content\n").unwrap();
    let (out, _d) = run("t", &input_for(&f));
    assert!(out.status.success(), "{}", stderr(&out));
    let so = stdout(&out);
    assert!(so.contains("hello-file-content"), "{so}");
    // Same path shape as on the server: {root}/file_<32 hex>/<safe name>.
    let line = so.lines().find(|l| l.starts_with("path=")).unwrap();
    assert!(line.contains("stroem-run-inputs-"), "{line}");
    assert!(line.contains("/file_"), "{line}");
    assert!(line.ends_with("/report.csv"), "{line}");
    // The temp tree is gone afterwards.
    let file = std::path::Path::new(line.trim_start_matches("path="));
    let root = file.parent().unwrap().parent().unwrap();
    assert!(!root.exists(), "{root:?} was not removed");
}

#[test]
fn run_file_field_read_only() {
    let data = tempfile::tempdir().unwrap();
    let f = data.path().join("a.txt");
    std::fs::write(&f, "x").unwrap();
    let (out, _d) = run("t", &input_for(&f));
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("readonly"), "{}", stdout(&out));
    // The source file is untouched.
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "x");
}

#[test]
fn run_file_field_not_a_file_is_error() {
    let data = tempfile::tempdir().unwrap();
    let (out, _d) = run("t", &input_for(data.path()));
    assert!(!out.status.success());
    let se = stderr(&out);
    assert!(se.contains("data_file"), "{se}");
    assert!(!se.contains(data.path().to_str().unwrap()), "{se}");
}

#[test]
fn run_required_file_field_without_value_is_error() {
    let (out, _d) = run("t", "{}");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("Input field 'data_file': a file is required"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn run_optional_file_field_without_value_runs() {
    let (out, _d) = run("opt", "{}");
    assert!(out.status.success(), "{}", stderr(&out));
}
