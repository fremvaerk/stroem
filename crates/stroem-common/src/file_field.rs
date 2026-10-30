//! The `file` input field rules (spec 2026-10-29-file-inputs § 4.2, § 5.1, § 8).
//!
//! Pure and shared by the server, the worker and the CLI: safe names, file
//! ids, paths, and the per-job map of attached files. Errors are value-free:
//! they name the input field, never an id, a name, a path or a rendered value.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::workflow::InputFieldDef;

/// The type name of a file input field.
pub const FILE_TYPE: &str = "file";
/// Where input files appear inside a container and in non-worker contexts.
pub const CANONICAL_INPUTS_ROOT: &str = "/inputs";
/// Longest safe name, in bytes.
pub const MAX_SAFE_NAME_BYTES: usize = 128;
/// Longest extension kept when a safe name is truncated, in bytes.
const MAX_KEPT_EXTENSION_BYTES: usize = 32;

const ID_PREFIX: &str = "file_";

/// Reduce an uploaded file name to a safe single path component (spec § 4.2).
/// `None` when nothing usable is left (empty, `.`, `..`, a trailing slash).
pub fn safe_name(original: &str) -> Option<String> {
    let base = original.rsplit(['/', '\\']).next().unwrap_or("");
    if base.is_empty() || base == "." || base == ".." {
        return None;
    }
    let mut name: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.starts_with(['.', '-']) {
        name.replace_range(0..1, "_");
    }
    if name.len() > MAX_SAFE_NAME_BYTES {
        // Every char is ASCII here, so bytes and chars agree.
        match name.rfind('.') {
            Some(pos) if pos > 0 && name.len() - pos <= MAX_KEPT_EXTENSION_BYTES => {
                let ext = name[pos..].to_string();
                name.truncate(MAX_SAFE_NAME_BYTES - ext.len());
                name.push_str(&ext);
            }
            _ => name.truncate(MAX_SAFE_NAME_BYTES),
        }
    }
    Some(name)
}

/// A new file id: `file_` + 32 lowercase hex characters.
pub fn new_file_id() -> String {
    format!("{ID_PREFIX}{}", uuid::Uuid::new_v4().simple())
}

/// `true` for exactly `file_` followed by 32 lowercase hex characters.
pub fn is_file_id(s: &str) -> bool {
    s.strip_prefix(ID_PREFIX).is_some_and(|hex| {
        hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// `{root}/{file_id}/{safe_name}`.
pub fn file_path(root: &str, file_id: &str, safe_name: &str) -> String {
    format!("{root}/{file_id}/{safe_name}")
}

/// One file attached to a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    pub file_id: String,
    pub safe_name: String,
}

/// The files attached to one job and the task-level fields that hold them.
#[derive(Debug, Clone)]
pub struct JobFiles {
    root: String,
    fields: BTreeMap<String, String>,
    files: BTreeMap<String, FileRef>,
}

impl Default for JobFiles {
    fn default() -> Self {
        Self::empty()
    }
}

impl JobFiles {
    /// `attachments` are `(task input field, file)` pairs.
    pub fn new(
        root: impl Into<String>,
        attachments: impl IntoIterator<Item = (String, FileRef)>,
    ) -> Self {
        let mut fields = BTreeMap::new();
        let mut files = BTreeMap::new();
        for (field, file) in attachments {
            fields.insert(field, file.file_id.clone());
            files.insert(file.file_id.clone(), file);
        }
        Self {
            root: root.into(),
            fields,
            files,
        }
    }

    pub fn empty() -> Self {
        Self::new(CANONICAL_INPUTS_ROOT, [])
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    /// The same attachments under another root.
    pub fn with_root(&self, root: &str) -> Self {
        Self {
            root: root.to_string(),
            ..self.clone()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Every distinct file, in id order.
    pub fn files(&self) -> impl Iterator<Item = &FileRef> {
        self.files.values()
    }

    /// The file attached to a task-level input field.
    pub fn field_file(&self, field: &str) -> Option<&FileRef> {
        self.fields.get(field).and_then(|id| self.files.get(id))
    }

    pub fn path_of(&self, file_id: &str) -> Option<String> {
        self.files
            .get(file_id)
            .map(|f| file_path(&self.root, &f.file_id, &f.safe_name))
    }

    /// Job input with every attached field that still holds its own file id
    /// replaced by the file path. Non-object input is returned unchanged.
    pub fn template_input(&self, input: &Value) -> Value {
        let Some(map) = input.as_object() else {
            return input.clone();
        };
        let mut out = map.clone();
        for (field, id) in &self.fields {
            if out.get(field).and_then(Value::as_str) == Some(id.as_str()) {
                if let Some(path) = self.path_of(id) {
                    out.insert(field.clone(), Value::String(path));
                }
            }
        }
        Value::Object(out)
    }

    /// Prepared step input with every `file` field that holds an attached id
    /// replaced by the file path.
    pub fn template_prepared(
        &self,
        prepared: &Value,
        schema: &HashMap<String, InputFieldDef>,
    ) -> Value {
        let Some(map) = prepared.as_object() else {
            return prepared.clone();
        };
        let mut out = map.clone();
        for (name, def) in schema {
            if def.field_type != FILE_TYPE {
                continue;
            }
            let path = out
                .get(name)
                .and_then(Value::as_str)
                .and_then(|id| self.path_of(id));
            if let Some(path) = path {
                out.insert(name.clone(), Value::String(path));
            }
        }
        Value::Object(out)
    }

    /// The attached file a `file` target value names: its id, or exactly
    /// `{root}/{id}/{its safe name}`.
    pub fn resolve_target(&self, value: &str) -> Option<&FileRef> {
        if let Some(f) = self.files.get(value) {
            return Some(f);
        }
        let rest = value.strip_prefix(self.root.as_str())?.strip_prefix('/')?;
        let (id, _) = rest.split_once('/')?;
        let f = self.files.get(id)?;
        (file_path(&self.root, &f.file_id, &f.safe_name) == value).then_some(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileInputErrorKind {
    CannotUse,
    Required,
    InlineTooLarge { cap: u64 },
}

/// A typed, value-free file input error (spec § 8). Callers `downcast_ref` it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInputError {
    pub field: String,
    pub kind: FileInputErrorKind,
}

impl fmt::Display for FileInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let field = &self.field;
        match &self.kind {
            FileInputErrorKind::CannotUse => {
                write!(f, "Input field '{field}': the file cannot be used")
            }
            FileInputErrorKind::Required => {
                write!(f, "Input field '{field}': a file is required")
            }
            FileInputErrorKind::InlineTooLarge { cap } => write!(
                f,
                "Input field '{field}': inline content is larger than {cap} bytes"
            ),
        }
    }
}

impl std::error::Error for FileInputError {}

/// A file as the claim response lists it for the worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimedInputFile {
    pub file_id: String,
    pub safe_name: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[cfg(test)]
mod tests;
