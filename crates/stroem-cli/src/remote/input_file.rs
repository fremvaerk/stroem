//! `stroem-api input-file <JOB_ID> <FIELD> [-o PATH]`: download the file
//! given to a job's `type: file` input field
//! (`GET /api/jobs/{id}/input-files/{field}`), streamed to disk.

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

use super::client::api_url;
use super::client::check_response;

#[derive(clap::Args)]
pub struct InputFileArgs {
    /// Job ID
    pub job_id: String,
    /// The `type: file` input field
    pub field: String,
    /// Output path, overwritten if it exists (default: the file's name, else
    /// the field name, in the current directory; never overwrites)
    #[arg(long, short = 'o')]
    pub output: Option<String>,
}

pub async fn run(client: &Client, server: &str, args: InputFileArgs) -> Result<()> {
    let (path, n) = download(client, server, &args, Path::new(".")).await?;
    eprintln!("Wrote {n} bytes to {}", path.display());
    Ok(())
}

/// Stream the file to its output path, chunk by chunk; nothing is written
/// when the server refuses. Without `-o` the name comes from the server, so
/// the file is created in `dir` and never replaces an existing one. Returns
/// the path and the byte count.
pub async fn download(
    client: &Client,
    server: &str,
    args: &InputFileArgs,
    dir: &Path,
) -> Result<(PathBuf, u64)> {
    let mut resp = client
        .get(api_url(
            server,
            &["api", "jobs", &args.job_id, "input-files", &args.field],
        )?)
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    if !status.is_success() {
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        check_response(&status, &body)?;
        anyhow::bail!("Server returned {status}");
    }

    let mut open = tokio::fs::OpenOptions::new();
    open.write(true);
    let path = match &args.output {
        Some(o) => {
            open.create(true).truncate(true);
            PathBuf::from(o)
        }
        None => {
            open.create_new(true);
            dir.join(default_name(
                resp.headers()
                    .get(reqwest::header::CONTENT_DISPOSITION)
                    .and_then(|v| v.to_str().ok()),
                &args.field,
            ))
        }
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let mut file = open.open(&path).await.with_context(|| {
        if args.output.is_none() {
            format!(
                "create {} (an existing file is never replaced without -o)",
                path.display()
            )
        } else {
            format!("create {}", path.display())
        }
    })?;
    let mut written: u64 = 0;
    let copied: Result<()> = async {
        while let Some(chunk) = resp.chunk().await.context("read response body")? {
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        file.flush().await?;
        Ok(())
    }
    .await;
    if let Err(e) = copied {
        drop(file);
        let _ = tokio::fs::remove_file(&path).await;
        return Err(e.context(format!("download to {}", path.display())));
    }
    Ok((path, written))
}

/// The `filename="…"` of a `Content-Disposition`, reduced to a plain file
/// name (no directory, no `.` / `..`, no control characters, a leading `.`
/// made `_` so it is never a hidden file); else `field`.
fn default_name(disposition: Option<&str>, field: &str) -> String {
    let name = plain_name(disposition, field);
    match name.strip_prefix('.') {
        Some(rest) => format!("_{rest}"),
        None => name,
    }
}

fn plain_name(disposition: Option<&str>, field: &str) -> String {
    let quoted = disposition.and_then(|d| {
        d.split(';').map(str::trim).find_map(|part| {
            part.strip_prefix("filename=")
                .map(|v| v.trim_matches('"').to_string())
        })
    });
    let name = quoted.as_deref().and_then(|n| {
        let base = n.rsplit(['/', '\\']).next().unwrap_or("");
        let clean: String = base.chars().filter(|c| !c.is_control()).collect();
        (!clean.is_empty() && clean != "." && clean != "..").then_some(clean)
    });
    let fallback = || {
        Path::new(field)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .filter(|f| f != "..")
            .unwrap_or_else(|| "input-file".to_string())
    };
    name.unwrap_or_else(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const JOB: &str = "00000000-0000-0000-0000-000000000042";

    fn args(output: Option<&Path>) -> InputFileArgs {
        InputFileArgs {
            job_id: JOB.to_string(),
            field: "data_file".to_string(),
            output: output.map(|p| p.to_string_lossy().into_owned()),
        }
    }

    #[test]
    fn default_name_is_a_plain_file_name() {
        let d = |v: &str| default_name(Some(v), "data_file");
        assert_eq!(
            d("attachment; filename=\"report.csv\"; filename*=UTF-8''report.csv"),
            "report.csv"
        );
        assert_eq!(d("attachment; filename=\"../../etc/passwd\""), "passwd");
        assert_eq!(d("attachment; filename=\"a\\\\b.txt\""), "b.txt");
        assert_eq!(d("attachment; filename=\"..\""), "data_file");
        assert_eq!(d("attachment"), "data_file");
        assert_eq!(default_name(None, "data_file"), "data_file");
        assert_eq!(default_name(None, ".."), "input-file");
        assert_eq!(d("attachment; filename=\".bashrc\""), "_bashrc");
        assert_eq!(default_name(None, ".hidden"), "_hidden");
    }

    #[tokio::test]
    async fn a_server_given_name_never_replaces_a_file() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/api/jobs/{JOB}/input-files/data_file")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-disposition", "attachment; filename=\"notes.txt\"")
                    .set_body_bytes(b"from the server".to_vec()),
            )
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir()?;
        let (path, _) = download(&Client::new(), &server.uri(), &args(None), tmp.path()).await?;
        assert_eq!(path, tmp.path().join("notes.txt"));
        assert_eq!(tokio::fs::read(&path).await?, b"from the server");

        tokio::fs::write(&path, b"mine").await?;
        let err = download(&Client::new(), &server.uri(), &args(None), tmp.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("never replaced"), "{err:#}");
        assert_eq!(tokio::fs::read(&path).await?, b"mine");

        // An explicit -o path is the user's choice and is replaced.
        let out = tmp.path().join("notes.txt");
        download(&Client::new(), &server.uri(), &args(Some(&out)), tmp.path()).await?;
        assert_eq!(tokio::fs::read(&out).await?, b"from the server");
        Ok(())
    }

    #[tokio::test]
    async fn streams_the_body_to_the_output_path() -> Result<()> {
        let server = MockServer::start().await;
        let body = vec![b'z'; 300 * 1024];
        Mock::given(method("GET"))
            .and(path(format!("/api/jobs/{JOB}/input-files/data_file")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/csv")
                    .set_body_bytes(body.clone()),
            )
            .expect(1)
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir()?;
        let out = tmp.path().join("nested").join("out.csv");
        let (path, n) =
            download(&Client::new(), &server.uri(), &args(Some(&out)), tmp.path()).await?;
        assert_eq!(path, out);
        assert_eq!(n, body.len() as u64);
        assert_eq!(tokio::fs::read(&out).await?, body);
        Ok(())
    }

    #[tokio::test]
    async fn a_refusal_writes_no_file() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/api/jobs/{JOB}/input-files/data_file")))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(json!({"error": "Input file not found"})),
            )
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir()?;
        let out = tmp.path().join("out.csv");
        let err = download(&Client::new(), &server.uri(), &args(Some(&out)), tmp.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Input file not found"), "{err}");
        assert!(!out.exists());
        Ok(())
    }

    #[derive(clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        command: super::super::Commands,
    }

    #[test]
    fn parses_job_field_and_output() {
        use clap::Parser;
        let cli =
            Cli::try_parse_from(["stroem-api", "input-file", "j1", "data_file", "-o", "x.csv"])
                .unwrap();
        match cli.command {
            super::super::Commands::InputFile(a) => {
                assert_eq!((a.job_id.as_str(), a.field.as_str()), ("j1", "data_file"));
                assert_eq!(a.output.as_deref(), Some("x.csv"));
            }
            _ => panic!("unexpected command variant"),
        }
        assert!(Cli::try_parse_from(["stroem-api", "input-file", "j1"]).is_err());
    }
}
