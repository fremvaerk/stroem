//! CLI subcommands for uploading state snapshots.

use super::StateAction;
use anyhow::{Context, Result};
use reqwest::Client;

pub async fn dispatch(client: &Client, server: &str, action: StateAction) -> Result<()> {
    match action {
        StateAction::Upload {
            workspace,
            task,
            mode,
            state,
            files,
        } => {
            cmd_upload(
                client,
                server,
                &workspace,
                Some(&task),
                &mode,
                &state,
                &files,
            )
            .await
        }
        StateAction::UploadGlobal {
            workspace,
            mode,
            state,
            files,
        } => cmd_upload(client, server, &workspace, None, &mode, &state, &files).await,
    }
}

async fn cmd_upload(
    client: &Client,
    server: &str,
    workspace: &str,
    task: Option<&str>,
    mode: &str,
    state: &[(String, String)],
    files: &[std::path::PathBuf],
) -> Result<()> {
    let tarball = build_tarball(files).context("build tarball")?;

    let url = match task {
        Some(t) => format!("{}/api/workspaces/{}/tasks/{}/state", server, workspace, t),
        None => format!("{}/api/workspaces/{}/state", server, workspace),
    };

    // Assemble query string: mode + each state pair
    let mut params: Vec<(&str, &str)> = Vec::with_capacity(state.len() + 1);
    params.push(("mode", mode));
    for (k, v) in state {
        params.push((k.as_str(), v.as_str()));
    }

    let response = client
        .post(&url)
        .query(&params)
        .header("content-type", "application/gzip")
        .body(tarball)
        .send()
        .await
        .context("POST state")?;

    let status = response.status();
    let body = response.text().await.context("read response body")?;
    if !status.is_success() {
        anyhow::bail!("upload failed: {} — {}", status, body);
    }
    println!("{}", body);
    Ok(())
}

fn build_tarball(files: &[std::path::PathBuf]) -> Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::collections::HashSet;

    // Client-side guard: reject state.json before the server does so the
    // user gets a clear, actionable error message.
    for path in files {
        let name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("path '{}' has no filename", path.display()))?
            .to_string_lossy();
        if name == "state.json" {
            anyhow::bail!(
                "state.json cannot be uploaded as a file — pass state values via --state KEY=VALUE flags instead"
            );
        }
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut builder = tar::Builder::new(&mut encoder);
        for path in files {
            let bytes =
                std::fs::read(path).with_context(|| format!("read file {}", path.display()))?;
            let name = path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("path '{}' has no filename", path.display()))?
                .to_string_lossy()
                .into_owned();
            if !seen.insert(name.clone()) {
                anyhow::bail!(
                    "Duplicate filename '{}' in upload set — two input paths share the same basename. \
                     Rename one of the files or use distinct basenames.",
                    name
                );
            }
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            builder
                .append_data(&mut header, &name, &bytes[..])
                .context("append tar entry")?;
        }
        builder.finish().context("finish tar")?;
    }
    Ok(encoder.finish()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn build_tarball_rejects_duplicate_basenames() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir(&a).unwrap();
        let b = tmp.path().join("b");
        std::fs::create_dir(&b).unwrap();
        let f1 = a.join("cert.pem");
        let f2 = b.join("cert.pem");
        std::fs::write(&f1, b"A").unwrap();
        std::fs::write(&f2, b"B").unwrap();

        let err = build_tarball(&[f1, f2]).unwrap_err();
        assert!(
            err.to_string().contains("Duplicate filename"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn build_tarball_accepts_distinct_basenames() {
        let tmp = TempDir::new().unwrap();
        let f1 = tmp.path().join("cert.pem");
        let f2 = tmp.path().join("privkey.pem");
        std::fs::write(&f1, b"A").unwrap();
        std::fs::write(&f2, b"B").unwrap();

        let bytes = build_tarball(&[f1, f2]).unwrap();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn build_tarball_handles_empty_file_list() {
        let bytes = build_tarball(&[]).unwrap();
        assert!(
            !bytes.is_empty(),
            "empty tarball should still be valid gzip"
        );
    }

    #[test]
    fn build_tarball_rejects_state_json_filename() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("state.json");
        std::fs::write(&f, b"{}").unwrap();

        let err = build_tarball(&[f]).unwrap_err();
        assert!(
            err.to_string().contains("state values via --state"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn build_tarball_handles_unicode_and_spaces_in_basenames() {
        let tmp = TempDir::new().unwrap();
        let f1 = tmp.path().join("my file.pem");
        let f2 = tmp.path().join("证书.pem");
        std::fs::write(&f1, b"A").unwrap();
        std::fs::write(&f2, b"B").unwrap();

        let bytes = build_tarball(&[f1, f2]).unwrap();

        // Unpack to verify the filenames round-trip correctly.
        use flate2::read::GzDecoder;
        use tar::Archive;
        let mut archive = Archive::new(GzDecoder::new(&bytes[..]));
        let mut names: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["my file.pem".to_string(), "证书.pem".to_string()]
        );
    }
}
