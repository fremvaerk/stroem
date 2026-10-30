use std::collections::HashMap;
use std::error::Error;

use serde_json::json;

use super::*;
use crate::template::{resolve_connection_inputs, SingleWorkspace};

#[test]
fn safe_name_table() {
    let long = format!("{}.csv", "a".repeat(200));
    let cases: Vec<(String, Option<String>)> = vec![
        ("report.csv".into(), Some("report.csv".into())),
        ("x;rm -rf ~.csv".into(), Some("x_rm_-rf__.csv".into())),
        ("../../etc/passwd".into(), Some("passwd".into())),
        ("C:\\Users\\me\\x.txt".into(), Some("x.txt".into())),
        ("-rf".into(), Some("_rf".into())),
        (".bashrc".into(), Some("_bashrc".into())),
        ("a b.csv".into(), Some("a_b.csv".into())),
        ("rapport-é.csv".into(), Some("rapport-_.csv".into())),
        ("$(id).sh".into(), Some("__id_.sh".into())),
        ("line\nbreak".into(), Some("line_break".into())),
        ("".into(), None),
        (".".into(), None),
        ("..".into(), None),
        ("dir/".into(), None),
        (long, Some(format!("{}.csv", "a".repeat(124)))),
        ("b".repeat(300), Some("b".repeat(128))),
    ];
    for (input, expected) in cases {
        assert_eq!(safe_name(&input), expected, "{input:?}");
    }
    assert_eq!(
        safe_name(&format!("{}.csv", "a".repeat(200)))
            .unwrap()
            .len(),
        128
    );
}

#[test]
fn file_id_shape() {
    assert!(is_file_id(&new_file_id()));
    let hex = "0123456789abcdef0123456789abcdef";
    assert!(is_file_id(&format!("file_{hex}")));
    assert!(!is_file_id(&format!("file_{}", hex.to_uppercase())));
    assert!(!is_file_id(&format!("file_{}", &hex[..31])));
    assert!(!is_file_id(&format!("file_{hex}0")));
    assert!(!is_file_id(hex));
    assert!(!is_file_id(&format!("file_{}", "g".repeat(32))));
}

fn attach(field: &str, id: &str, name: &str) -> (String, FileRef) {
    (
        field.to_string(),
        FileRef {
            file_id: id.to_string(),
            safe_name: name.to_string(),
        },
    )
}

#[test]
fn template_input_maps_only_attached_fields() {
    let files = JobFiles::new(
        "/r",
        [
            attach("a", "file_X", "r.csv"),
            attach("b", "file_X", "r.csv"),
        ],
    );
    let out = files.template_input(&json!({"a": "file_X", "b": "file_X", "c": "file_X"}));
    assert_eq!(out["a"], "/r/file_X/r.csv");
    assert_eq!(out["b"], "/r/file_X/r.csv");
    assert_eq!(out["c"], "file_X");
    assert_eq!(files.template_input(&json!("x")), json!("x"));
}

fn def(t: &str) -> InputFieldDef {
    InputFieldDef {
        field_type: t.to_string(),
        ..Default::default()
    }
}

#[test]
fn template_prepared_maps_file_fields_only() {
    let files = JobFiles::new("/r", [attach("a", "file_X", "r.csv")]);
    let schema = HashMap::from([
        ("d".to_string(), def("file")),
        ("s".to_string(), def("string")),
    ]);
    let out = files.template_prepared(&json!({"d": "file_X", "s": "file_X"}), &schema);
    assert_eq!(out["d"], "/r/file_X/r.csv");
    assert_eq!(out["s"], "file_X");
    let out = files.template_prepared(&json!({"d": "file_other"}), &schema);
    assert_eq!(out["d"], "file_other");
}

#[test]
fn resolve_target_cases() {
    let files = JobFiles::new("/r", [attach("a", "file_X", "r.csv")]);
    assert!(files.resolve_target("file_X").is_some());
    assert!(files.resolve_target("/r/file_X/r.csv").is_some());
    assert!(files
        .with_root("/o")
        .resolve_target("/r/file_X/r.csv")
        .is_none());
    assert!(files.resolve_target("file_Y").is_none());
    assert!(files.resolve_target("/r/file_Y/r.csv").is_none());
    assert!(files.resolve_target("/r/file_X/other.csv").is_none());
    assert!(files.resolve_target("/r/file_X/r.csv/x").is_none());
    assert!(files.resolve_target("").is_none());
}

#[test]
fn collision_case_rev4() {
    let files = JobFiles::new(
        "/r",
        [
            attach("a", "file_X", "report.csv"),
            attach("b", "file_Y", "report.csv"),
        ],
    );
    assert_ne!(files.path_of("file_X"), files.path_of("file_Y"));
    assert_eq!(files.files().count(), 2);
    assert_eq!(files.field_file("a").unwrap().file_id, "file_X");
    assert!(!files.is_empty());
    assert!(JobFiles::default().is_empty());
    assert_eq!(JobFiles::default().root(), CANONICAL_INPUTS_ROOT);
}

#[test]
fn file_input_error_texts() {
    let e = |kind| FileInputError {
        field: "f".into(),
        kind,
    };
    assert_eq!(
        e(FileInputErrorKind::CannotUse).to_string(),
        "Input field 'f': the file cannot be used"
    );
    assert_eq!(
        e(FileInputErrorKind::Required).to_string(),
        "Input field 'f': a file is required"
    );
    let big = e(FileInputErrorKind::InlineTooLarge { cap: 1048576 });
    assert_eq!(
        big.to_string(),
        "Input field 'f': inline content is larger than 1048576 bytes"
    );
    assert!(big.source().is_none());
}

#[test]
fn connection_resolution_leaves_file_fields_alone() {
    let config = crate::models::workflow::WorkspaceConfig::default();
    let lookup = SingleWorkspace {
        name: "default",
        config: &config,
    };
    let schema = HashMap::from([("d".to_string(), def("file"))]);
    let input = json!({"d": "file_X"});
    let out = resolve_connection_inputs(&input, &schema, &lookup).unwrap();
    assert_eq!(out, input);
}
