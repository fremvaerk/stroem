//! The files attached to a job (`job_input_file` rows), as the render context
//! and the claim response need them (spec 2026-10-29 § 5.1, § 5.4).

use std::collections::BTreeMap;

use anyhow::Context;
use sqlx::PgPool;
use stroem_common::file_field::{ClaimedInputFile, FileRef, JobFiles};
use stroem_db::{JobInputFileRepo, JobInputFileRow};
use uuid::Uuid;

/// Every attachment row of `job_id`, ordered by field.
pub async fn load_attachments(pool: &PgPool, job_id: Uuid) -> anyhow::Result<Vec<JobInputFileRow>> {
    JobInputFileRepo::list_for_job(pool, job_id)
        .await
        .context("load the job's input files")
}

/// The job's files under `root`, for [`crate::render_context::Snapshots`].
pub fn job_files(rows: &[JobInputFileRow], root: &str) -> JobFiles {
    JobFiles::new(
        root,
        rows.iter().map(|r| {
            (
                r.field.clone(),
                FileRef {
                    file_id: r.file_id.clone(),
                    safe_name: r.safe_name.clone(),
                },
            )
        }),
    )
}

/// Every distinct file of the job, once each, in file-id order: one upload
/// held by two fields is one entry and one path (spec § 5.4).
pub fn claimed_files(rows: &[JobInputFileRow]) -> Vec<ClaimedInputFile> {
    let mut by_id = BTreeMap::new();
    for r in rows {
        by_id
            .entry(r.file_id.as_str())
            .or_insert_with(|| ClaimedInputFile {
                file_id: r.file_id.clone(),
                safe_name: r.safe_name.clone(),
                // `input_file.size_bytes` has `CHECK (size_bytes >= 0)`.
                size_bytes: r.size_bytes as u64,
                sha256: r.sha256.clone(),
            });
    }
    by_id.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use stroem_common::file_field::CANONICAL_INPUTS_ROOT;

    const FILE_A: &str = "file_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const FILE_B: &str = "file_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn row(field: &str, file_id: &str, safe_name: &str, size: i64) -> JobInputFileRow {
        JobInputFileRow {
            job_id: Uuid::nil(),
            field: field.to_string(),
            file_id: file_id.to_string(),
            original_name: format!("orig {safe_name}"),
            safe_name: safe_name.to_string(),
            content_type: "text/csv".to_string(),
            size_bytes: size,
            sha256: format!("sha-{file_id}"),
            storage_key: format!("inputs/{file_id}"),
        }
    }

    #[test]
    fn job_files_maps_each_field_to_its_file_under_root() {
        let rows = vec![
            row("a", FILE_B, "report.csv", 3),
            row("b", FILE_A, "report.csv", 4),
        ];
        let files = job_files(&rows, "/w/in");
        assert_eq!(files.root(), "/w/in");
        assert_eq!(
            files.path_of(FILE_A).as_deref(),
            Some("/w/in/file_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/report.csv")
        );
        assert_eq!(
            files.field_file("a").map(|f| f.file_id.as_str()),
            Some(FILE_B)
        );
        assert_eq!(
            files.field_file("b").map(|f| f.file_id.as_str()),
            Some(FILE_A)
        );

        let canonical = job_files(&rows, CANONICAL_INPUTS_ROOT);
        assert_eq!(
            canonical.path_of(FILE_B).as_deref(),
            Some("/inputs/file_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/report.csv")
        );
    }

    #[test]
    fn job_files_of_no_rows_is_empty() {
        let files = job_files(&[], CANONICAL_INPUTS_ROOT);
        assert!(files.is_empty());
        assert_eq!(files.root(), CANONICAL_INPUTS_ROOT);
    }

    #[test]
    fn claimed_files_lists_each_file_once_in_id_order() {
        // One upload in two fields, plus a second file: two entries.
        let rows = vec![
            row("a", FILE_B, "b.csv", 7),
            row("b", FILE_A, "a.csv", 0),
            row("c", FILE_B, "b.csv", 7),
        ];
        assert_eq!(
            claimed_files(&rows),
            vec![
                ClaimedInputFile {
                    file_id: FILE_A.to_string(),
                    safe_name: "a.csv".to_string(),
                    size_bytes: 0,
                    sha256: format!("sha-{FILE_A}"),
                },
                ClaimedInputFile {
                    file_id: FILE_B.to_string(),
                    safe_name: "b.csv".to_string(),
                    size_bytes: 7,
                    sha256: format!("sha-{FILE_B}"),
                },
            ]
        );
    }

    #[test]
    fn claimed_files_of_no_rows_is_empty() {
        assert!(claimed_files(&[]).is_empty());
    }
}
