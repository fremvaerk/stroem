//! Scan the runner's `/artifacts/` output directory and sniff content types.
//!
//! After a step finishes successfully, the worker walks the directory the
//! runner exposed as `/artifacts/`, flattens nested files into slash-joined
//! relative names, and prepares per-file upload metadata for the server.
//!
//! Symlinks are skipped (with a warning) so a workflow can't tar up `/etc`
//! by pointing a symlink at it. Names that would violate the server-side
//! validator (control chars, `..`, leading `-`) are skipped here too — we
//! prefer dropping the file with a warning over failing the whole step.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct ScanResult {
    pub files: Vec<ScannedFile>,
    pub warnings: Vec<String>,
}

pub struct ScannedFile {
    /// Name as exposed to the server; relative path from `/artifacts/`.
    pub name: String,
    pub abs_path: PathBuf,
    pub size_bytes: u64,
}

pub fn scan_artifacts(root: &Path) -> Result<ScanResult> {
    if !root.exists() {
        return Ok(ScanResult {
            files: vec![],
            warnings: vec![],
        });
    }
    let mut files = Vec::new();
    let mut warnings = Vec::new();

    // Walk eagerly so we can surface per-entry errors (permission denied,
    // broken symlink mid-traversal, IO race) as scan warnings instead of
    // silently dropping them — the previous `filter_map(|e| e.ok())` made
    // unreadable directories invisible to the operator.
    for entry_result in WalkDir::new(root).follow_links(false).into_iter() {
        let entry = match entry_result {
            Ok(e) => e,
            Err(e) => {
                let path = e
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<unknown>".to_string());
                warnings.push(format!("walk error at {path}: {e}"));
                continue;
            }
        };
        if entry.file_type().is_dir() {
            continue;
        }
        if entry.file_type().is_symlink() {
            warnings.push(format!("symlink skipped: {}", entry.path().display()));
            continue;
        }
        if !entry.file_type().is_file() {
            warnings.push(format!(
                "non-regular file skipped: {}",
                entry.path().display()
            ));
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .with_context(|| format!("strip_prefix {root:?} from {:?}", entry.path()))?;
        let name = rel.to_string_lossy().replace('\\', "/");
        if !valid_artifact_name(&name) {
            warnings.push(format!("name rejected: {name}"));
            continue;
        }
        let size_bytes = entry.metadata()?.len();
        files.push(ScannedFile {
            name,
            abs_path: entry.path().to_path_buf(),
            size_bytes,
        });
    }
    Ok(ScanResult { files, warnings })
}

fn valid_artifact_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 || name.starts_with('-') || name.starts_with('/') {
        return false;
    }
    for seg in name.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.starts_with('-') {
            return false;
        }
        for ch in seg.chars() {
            if ch == '\0' || (ch.is_control() && ch != ' ') {
                return false;
            }
        }
    }
    true
}

pub fn sniff(bytes: &[u8]) -> String {
    infer::get(bytes)
        .map(|kind| kind.mime_type().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

pub fn sniff_with_name(bytes: &[u8], name: &str) -> String {
    let primary = sniff(bytes);
    if primary != "application/octet-stream" {
        return primary;
    }
    // Lightweight extension fallback for textual types `infer` doesn't strongly detect.
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "md" | "markdown" => "text/markdown".into(),
        "txt" | "log" => "text/plain".into(),
        "json" => "application/json".into(),
        "yaml" | "yml" => "application/yaml".into(),
        "csv" => "text/csv".into(),
        "html" | "htm" => "text/html".into(),
        "svg" => "image/svg+xml".into(),
        _ => "application/octet-stream".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn scan_recurses_and_flattens_relative_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("reports")).unwrap();
        std::fs::write(root.join("top.txt"), b"a").unwrap();
        std::fs::write(root.join("reports/q1.html"), b"b").unwrap();
        let scan = scan_artifacts(root).unwrap();
        let names: Vec<_> = scan.files.iter().map(|f| f.name.clone()).collect();
        assert!(names.contains(&"top.txt".to_string()));
        assert!(names.contains(&"reports/q1.html".to_string()));
    }

    #[test]
    fn scan_skips_symlinks_with_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let target = tmp.path().parent().unwrap().join("victim");
        std::fs::write(&target, b"secret").unwrap();
        symlink(&target, root.join("link.txt")).unwrap();

        let scan = scan_artifacts(root).unwrap();
        assert!(scan.files.is_empty());
        assert!(scan.warnings.iter().any(|w| w.contains("symlink")));
    }

    #[test]
    fn scan_rejects_path_traversal_in_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("..weird")).unwrap();
        // ".." segments and control chars trigger rejection.
        let bad = root.join("a\x01bad");
        std::fs::write(&bad, b"x").unwrap();
        let scan = scan_artifacts(root).unwrap();
        assert!(scan.files.iter().all(|f| !f.name.contains('\x01')));
    }

    #[test]
    fn sniff_falls_back_to_octet_stream() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n"), "image/png");
        assert_eq!(sniff(b"random opaque bytes"), "application/octet-stream");
    }

    #[test]
    fn sniff_extension_fallback_for_text() {
        // infer doesn't strongly detect plain text; we fall back to extension for known textual types
        let ct = sniff_with_name(b"hello", "notes.md");
        assert_eq!(ct, "text/markdown");
    }

    /// Cover every extension branch in `sniff_with_name`'s fallback. `infer`
    /// returns `application/octet-stream` for these bytes (plain ASCII has no
    /// magic header), so the fallback table is what gets exercised. A bug in
    /// any single arm (e.g. typo in the MIME string, missing alias) would
    /// surface here rather than as a silent serve-as-octet-stream regression.
    #[test]
    fn sniff_extension_fallback_covers_each_known_type() {
        // Plain text bytes — infer won't match any magic header.
        let bytes = b"plain content with no magic";

        assert_eq!(sniff_with_name(bytes, "data.json"), "application/json");
        assert_eq!(sniff_with_name(bytes, "cfg.yaml"), "application/yaml");
        assert_eq!(sniff_with_name(bytes, "cfg.yml"), "application/yaml");
        assert_eq!(sniff_with_name(bytes, "rows.csv"), "text/csv");
        assert_eq!(sniff_with_name(bytes, "run.log"), "text/plain");
        assert_eq!(sniff_with_name(bytes, "notes.txt"), "text/plain");
        assert_eq!(sniff_with_name(bytes, "report.html"), "text/html");
        assert_eq!(sniff_with_name(bytes, "frag.htm"), "text/html");
        assert_eq!(sniff_with_name(bytes, "logo.svg"), "image/svg+xml");
        // Sanity: unknown extension still falls back to octet-stream.
        assert_eq!(
            sniff_with_name(bytes, "thing.unknown"),
            "application/octet-stream"
        );
    }

    /// Regression: WalkDir entry errors (e.g. unreadable subdirectory) must
    /// surface as warnings in `scan.warnings` rather than be silently
    /// dropped by `filter_map(|e| e.ok())`. We make a subdirectory mode 000
    /// so WalkDir trips when it tries to descend into it.
    #[test]
    #[cfg(unix)]
    fn scan_surfaces_walk_errors_as_warnings() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("ok.txt"), b"x").unwrap();
        let locked = root.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("inside.txt"), b"y").unwrap();
        // Strip all perms so WalkDir cannot list the directory.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Probe whether the perm strip actually denies access — running as
        // root (some CI containers) or on filesystems that ignore mode bits
        // makes this test meaningless. We check by trying to list the dir
        // ourselves, then short-circuit if it succeeded.
        let access_denied = std::fs::read_dir(&locked).is_err();

        let scan = scan_artifacts(root).unwrap();

        // Restore perms before any asserts so the temp dir can be cleaned up.
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700));

        if !access_denied {
            // Environment doesn't enforce mode 000 — skip the warning assertion.
            return;
        }
        assert!(
            scan.warnings.iter().any(|w| w.contains("walk error")),
            "expected a `walk error` warning, got {:?}",
            scan.warnings
        );
        // The readable sibling must still be reported.
        assert!(scan.files.iter().any(|f| f.name == "ok.txt"));
    }
}
