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
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("upload failed: {} — {}", status, body);
    }
    println!("{}", body);
    Ok(())
}

fn build_tarball(files: &[std::path::PathBuf]) -> Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
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
