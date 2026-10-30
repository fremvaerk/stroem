//! Accepting `file` input values at job creation (spec § 4.4 – § 4.6): the
//! shape and required check of every task-level `file` field, and the
//! attachment rows written in the creation transaction.

use serde_json::Value;
use std::collections::HashMap;
use stroem_common::file_field::{is_file_id, FileInputError, FileInputErrorKind, FILE_TYPE};
use stroem_common::models::workflow::{InputFieldDef, TaskDef};
use stroem_db::{AttachRule, JobInputFileRepo};
use uuid::Uuid;

use crate::state::AppState;

/// What a creation may attach besides its own lineage (spec § 4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileGrant {
    /// No A1. A2 only through the creation mode's lineage (Rerun/Restart →
    /// `source_job_id`, Retry → `failed_job_id`).
    Indirect,
    /// A creation the caller starts directly (execute, MCP, Re-run, Restart):
    /// A1 for this caller's uploads. `user_id` is `None` when auth is off.
    Direct {
        user_id: Option<Uuid>,
        ttl_secs: i64,
    },
    /// A `type: task` child: A2 against `parent_job_id`.
    TaskChild,
}

impl FileGrant {
    pub fn direct(state: &AppState, user_id: Option<Uuid>) -> Self {
        FileGrant::Direct {
            user_id,
            ttl_secs: i64::try_from(state.input_files_config.upload_ttl_secs).unwrap_or(i64::MAX),
        }
    }
}

/// Shape + required check of every task-level `file` field, on the input as
/// submitted (before `merge_defaults`). `Ok` = the `(field, file_id)` pairs to
/// attach, in field-name order. Absent, `null` and `""` are "no file".
pub fn task_file_values(
    task: &TaskDef,
    input: &Value,
) -> Result<Vec<(String, String)>, FileInputError> {
    let fields = sorted_file_fields(&task.input);
    let mut values = Vec::new();
    for (name, def) in fields {
        match input.get(name) {
            v if is_no_file(v) => {}
            Some(Value::String(s)) if is_file_id(s) => {
                values.push((name.clone(), s.clone()));
                continue;
            }
            _ => {
                return Err(FileInputError {
                    field: name.clone(),
                    kind: FileInputErrorKind::CannotUse,
                })
            }
        }
        if def.required {
            return Err(FileInputError {
                field: name.clone(),
                kind: FileInputErrorKind::Required,
            });
        }
    }
    Ok(values)
}

/// For a creation that can never carry a file (trigger, webhook, action
/// hook; spec § 4.5, § 4.6): fails on the first required `file` field, else
/// on the first `file` field the input gives a value. Field-name order.
pub fn preflight(
    schema: &HashMap<String, InputFieldDef>,
    input: &Value,
) -> Result<(), FileInputError> {
    let fields = sorted_file_fields(schema);
    if let Some((name, _)) = fields.iter().find(|(_, def)| def.required) {
        return Err(FileInputError {
            field: (*name).clone(),
            kind: FileInputErrorKind::Required,
        });
    }
    for (name, _) in fields {
        if !is_no_file(input.get(name)) {
            return Err(FileInputError {
                field: name.clone(),
                kind: FileInputErrorKind::CannotUse,
            });
        }
    }
    Ok(())
}

/// Absent, `null` and `""` are "no file".
fn is_no_file(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(_) => false,
    }
}

fn sorted_file_fields(schema: &HashMap<String, InputFieldDef>) -> Vec<(&String, &InputFieldDef)> {
    let mut fields: Vec<_> = schema
        .iter()
        .filter(|(_, def)| def.field_type == FILE_TYPE)
        .collect();
    fields.sort_by(|a, b| a.0.cmp(b.0));
    fields
}

/// In the creation transaction, after the job row: attach each pair by the
/// first matching rule — lineage (`lineage` set), then the caller's own
/// upload (`Direct` only) — or fail with [`FileInputErrorKind::CannotUse`].
pub async fn attach_all_tx(
    tx: &mut sqlx::PgConnection,
    job_id: Uuid,
    workspace: &str,
    task_name: &str,
    values: &[(String, String)],
    grant: FileGrant,
    lineage: Option<Uuid>,
) -> anyhow::Result<()> {
    let mut rules = Vec::with_capacity(2);
    if let Some(source) = lineage {
        rules.push(AttachRule::Lineage { job_id: source });
    }
    if let FileGrant::Direct { user_id, ttl_secs } = grant {
        rules.push(AttachRule::Own {
            workspace,
            task_name,
            user_id,
            ttl_secs,
        });
    }
    for (field, file_id) in values {
        if !JobInputFileRepo::attach_tx(&mut *tx, job_id, field, file_id, &rules).await? {
            return Err(anyhow::Error::new(FileInputError {
                field: field.clone(),
                kind: FileInputErrorKind::CannotUse,
            }));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use stroem_common::models::workflow::InputFieldDef;

    fn task(yaml: &str) -> TaskDef {
        serde_yaml::from_str(yaml).unwrap()
    }

    const ID: &str = "file_0123456789abcdef0123456789abcdef";

    fn err(field: &str, kind: FileInputErrorKind) -> FileInputError {
        FileInputError {
            field: field.into(),
            kind,
        }
    }

    fn schema(yaml: &str) -> HashMap<String, InputFieldDef> {
        task(yaml).input
    }

    #[test]
    fn preflight_cases() {
        let optional = schema("input: { f: { type: file }, s: { type: string } }\nflow: {}");
        for input in [
            json!({}),
            json!(null),
            json!({"f": null}),
            json!({"f": ""}),
            json!({"s": 1}),
        ] {
            assert_eq!(preflight(&optional, &input), Ok(()), "{input}");
        }
        for value in [json!(ID), json!(42), json!(["x"])] {
            assert_eq!(
                preflight(&optional, &json!({ "f": value })),
                Err(err("f", FileInputErrorKind::CannotUse)),
                "{value}"
            );
        }
        let required = schema("input: { f: { type: file, required: true } }\nflow: {}");
        for input in [json!({}), json!({"f": null}), json!({"f": ""})] {
            assert_eq!(
                preflight(&required, &input),
                Err(err("f", FileInputErrorKind::Required)),
                "{input}"
            );
        }
        // Required wins over CannotUse, and the first field alphabetically wins.
        let both =
            schema("input: { a: { type: file }, z: { type: file, required: true } }\nflow: {}");
        assert_eq!(
            preflight(&both, &json!({"a": ID})),
            Err(err("z", FileInputErrorKind::Required))
        );
        let two = schema("input: { b: { type: file }, a: { type: file } }\nflow: {}");
        assert_eq!(
            preflight(&two, &json!({"a": ID, "b": ID})),
            Err(err("a", FileInputErrorKind::CannotUse))
        );
        assert_eq!(preflight(&HashMap::new(), &json!({"a": 1})), Ok(()));
    }

    #[test]
    fn task_without_file_fields_takes_any_input() {
        let t = task("input: { a: { type: string, required: true } }\nflow: {}");
        assert_eq!(task_file_values(&t, &json!({})), Ok(vec![]));
        assert_eq!(task_file_values(&t, &json!({"a": 42})), Ok(vec![]));
        assert_eq!(task_file_values(&t, &json!(null)), Ok(vec![]));
    }

    #[test]
    fn file_ids_are_collected_in_field_order() {
        let t = task(
            "input:\n  b: { type: file }\n  a: { type: file }\n  s: { type: string }\nflow: {}",
        );
        let other = "file_fedcba9876543210fedcba9876543210";
        assert_eq!(
            task_file_values(&t, &json!({"b": other, "a": ID, "s": "x"})),
            Ok(vec![("a".into(), ID.into()), ("b".into(), other.into())])
        );
    }

    #[test]
    fn absent_null_and_empty_are_no_file() {
        let t = task("input: { f: { type: file } }\nflow: {}");
        for input in [json!({}), json!({"f": null}), json!({"f": ""}), json!(null)] {
            assert_eq!(task_file_values(&t, &input), Ok(vec![]), "{input}");
        }
        let r = task("input: { f: { type: file, required: true } }\nflow: {}");
        for input in [json!({}), json!({"f": null}), json!({"f": ""}), json!([1])] {
            assert_eq!(
                task_file_values(&r, &input),
                Err(err("f", FileInputErrorKind::Required)),
                "{input}"
            );
        }
    }

    #[test]
    fn anything_but_a_file_id_cannot_be_used() {
        let t = task("input: { f: { type: file, required: true } }\nflow: {}");
        for value in [
            json!(42),
            json!(true),
            json!(["x"]),
            json!({"file_id": ID}),
            json!("••••••"),
            json!("/inputs/file_0123456789abcdef0123456789abcdef/a.csv"),
            json!("FILE_0123456789ABCDEF0123456789ABCDEF"),
            json!(" file_0123456789abcdef0123456789abcdef"),
        ] {
            assert_eq!(
                task_file_values(&t, &json!({ "f": value })),
                Err(err("f", FileInputErrorKind::CannotUse)),
                "{value}"
            );
        }
    }

    #[tokio::test]
    async fn direct_grant_takes_the_configured_ttl() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = crate::state::test_app_state_with_workspaces(
            crate::workspace::WorkspaceManager::from_config(
                "default",
                stroem_common::models::workflow::WorkspaceConfig::new(),
            ),
            tmp.path(),
        );
        let user = Uuid::new_v4();
        assert_eq!(
            FileGrant::direct(&state, Some(user)),
            FileGrant::Direct {
                user_id: Some(user),
                ttl_secs: 86_400
            }
        );
    }
}
