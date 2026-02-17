use anyhow::{Context, Result};
use std::path::Path;

/// Check if a file path is a SOPS-encrypted file (by naming convention).
/// Matches `*.sops.yaml` and `*.sops.yml` but NOT `.sops.yaml` (the SOPS config file).
pub fn is_sops_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    (name.ends_with(".sops.yaml") || name.ends_with(".sops.yml")) && !name.starts_with('.')
}

/// Decrypt a SOPS-encrypted file by shelling out to the `sops` CLI.
/// Returns the decrypted YAML content as a string.
pub fn decrypt_sops_file(path: &Path) -> Result<String> {
    let output = std::process::Command::new("sops")
        .arg("-d")
        .arg(path)
        .output()
        .context("Failed to run sops — is it installed and on PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "sops decryption failed for {}: {}",
            path.display(),
            stderr.trim()
        );
    }

    String::from_utf8(output.stdout).context(format!(
        "sops output for {} is not valid UTF-8",
        path.display()
    ))
}

/// Read a YAML file, decrypting it first if it's a SOPS file.
pub fn read_yaml_file(path: &Path) -> Result<String> {
    if is_sops_file(path) {
        decrypt_sops_file(path)
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read file: {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_is_sops_file_sops_yaml() {
        assert!(is_sops_file(&PathBuf::from("secrets.sops.yaml")));
    }

    #[test]
    fn test_is_sops_file_sops_yml() {
        assert!(is_sops_file(&PathBuf::from("config.sops.yml")));
    }

    #[test]
    fn test_is_sops_file_dot_sops_yaml_is_config() {
        // .sops.yaml is the SOPS config file, not an encrypted file
        assert!(!is_sops_file(&PathBuf::from(".sops.yaml")));
    }

    #[test]
    fn test_is_sops_file_regular_yaml() {
        assert!(!is_sops_file(&PathBuf::from("regular.yaml")));
    }

    #[test]
    fn test_is_sops_file_regular_yml() {
        assert!(!is_sops_file(&PathBuf::from("workflow.yml")));
    }

    #[test]
    fn test_is_sops_file_wrong_extension() {
        assert!(!is_sops_file(&PathBuf::from("foo.sops.json")));
    }

    #[test]
    fn test_is_sops_file_with_directory_path() {
        assert!(is_sops_file(&PathBuf::from(
            "/workspace/.workflows/secrets.sops.yaml"
        )));
    }

    #[test]
    fn test_is_sops_file_dot_prefix_sops_yml() {
        assert!(!is_sops_file(&PathBuf::from(".sops.yml")));
    }

    #[test]
    fn test_read_yaml_file_plain() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.yaml");
        std::fs::write(&path, "key: value\n").unwrap();

        let content = read_yaml_file(&path).unwrap();
        assert_eq!(content, "key: value\n");
    }

    #[test]
    fn test_read_yaml_file_missing() {
        let result = read_yaml_file(Path::new("/nonexistent/file.yaml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_read_yaml_file_sops_named_without_sops_binary() {
        // A file named *.sops.yaml triggers the SOPS decryption path.
        // When sops is not installed, we get a clear error.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("secrets.sops.yaml");
        std::fs::write(&path, "key: value\n").unwrap();

        let result = read_yaml_file(&path);
        // Either sops is not installed (command not found) or sops fails
        // because the file is not actually encrypted — either way, it's an error.
        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_sops_file_error_message_is_helpful() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.sops.yaml");
        std::fs::write(&path, "key: value\n").unwrap();

        let err = decrypt_sops_file(&path).unwrap_err();
        let msg = format!("{:#}", err);
        // Should mention either sops or the file path
        assert!(
            msg.contains("sops") || msg.contains("test.sops.yaml"),
            "Error message should be helpful: {}",
            msg
        );
    }

    #[test]
    #[ignore] // Requires `sops` binary on PATH
    fn test_decrypt_sops_file_not_encrypted() {
        // Calling sops -d on a non-encrypted file should fail
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("plain.sops.yaml");
        std::fs::write(&path, "key: value\n").unwrap();

        let result = decrypt_sops_file(&path);
        assert!(result.is_err());
    }
}
