//! API URLs with percent-encoded path segments and query values.

use anyhow::{anyhow, Context, Result};
use reqwest::Url;

/// `{server}/api/{segments…}`; every segment is percent-encoded, and a path
/// prefix of `server` (a reverse-proxy mount) is kept.
pub fn api_url(server: &str, segments: &[&str]) -> Result<Url> {
    let mut url = Url::parse(server).context("Invalid server URL")?;
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| anyhow!("Invalid server URL: {server}"))?;
        path.pop_if_empty();
        path.push("api");
        for s in segments {
            path.push(s);
        }
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_and_encodes_segments() {
        let url = api_url(
            "http://localhost:8080",
            &["workspaces", "data team", "triggers"],
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "http://localhost:8080/api/workspaces/data%20team/triggers"
        );
        let url = api_url("http://localhost:8080/", &["jobs", "a/b"]).unwrap();
        assert_eq!(url.as_str(), "http://localhost:8080/api/jobs/a%2Fb");
    }

    #[test]
    fn keeps_a_path_prefix() {
        let url = api_url("https://example.com/stroem", &["stats"]).unwrap();
        assert_eq!(url.as_str(), "https://example.com/stroem/api/stats");
    }

    #[test]
    fn rejects_a_bad_server_url() {
        assert!(api_url("not a url", &["stats"]).is_err());
    }
}
