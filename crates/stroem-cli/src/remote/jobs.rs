use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use super::client::{check_response, jobs_url};

/// The `stroem-api jobs` filters, sent as `GET /api/jobs` query parameters.
#[derive(Default)]
pub struct JobsFilter<'a> {
    pub limit: i64,
    pub workspace: Option<&'a str>,
    pub task: Option<&'a str>,
    pub status: Option<&'a str>,
    pub search: Option<&'a str>,
    pub offset: i64,
}

/// `GET /api/jobs` with every set filter as a percent-encoded query value.
pub fn jobs_filter_url(server: &str, f: &JobsFilter<'_>) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&jobs_url(server, f.limit)).context("Invalid server URL")?;
    {
        let mut q = url.query_pairs_mut();
        for (key, value) in [
            ("workspace", f.workspace),
            ("task_name", f.task),
            ("status", f.status),
            ("search", f.search),
        ] {
            if let Some(v) = value {
                q.append_pair(key, v);
            }
        }
        if f.offset != 0 {
            q.append_pair("offset", &f.offset.to_string());
        }
    }
    Ok(url)
}

pub async fn cmd_jobs(client: &Client, server: &str, filter: &JobsFilter<'_>) -> Result<()> {
    let resp = client
        .get(jobs_filter_url(server, filter)?)
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    check_response(&status, &body)?;

    let jobs = body["items"]
        .as_array()
        .context("Expected items array in response")?;

    if jobs.is_empty() {
        println!("No jobs found.");
        return Ok(());
    }

    println!(
        "{:36} {:20} {:15} {:12} CREATED",
        "JOB ID", "TASK", "WORKSPACE", "STATUS"
    );
    println!("{}", "-".repeat(100));
    for job in jobs {
        let id = job.get("job_id").and_then(|v| v.as_str()).unwrap_or("-");
        let task = job.get("task_name").and_then(|v| v.as_str()).unwrap_or("-");
        let ws = job.get("workspace").and_then(|v| v.as_str()).unwrap_or("-");
        let st = job.get("status").and_then(|v| v.as_str()).unwrap_or("-");
        let created = job
            .get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        println!("{:36} {:20} {:15} {:12} {}", id, task, ws, st, created);
    }
    if let Some(total) = body["total"].as_i64() {
        println!("\n{}", page_note(jobs.len(), filter.offset, total));
    }

    Ok(())
}

/// `Jobs 21-40 of 95`; the next `--offset` when more are left.
fn page_note(shown: usize, offset: i64, total: i64) -> String {
    let first = offset + 1;
    let last = offset + shown as i64;
    let mut note = format!("Jobs {first}-{last} of {total}");
    if last < total {
        note.push_str(&format!(" (next page: --offset {last})"));
    }
    note
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn filter_url_encodes_every_set_filter() {
        let f = JobsFilter {
            limit: 10,
            workspace: Some("data team"),
            task: Some("etl"),
            status: Some("failed"),
            search: None,
            offset: 20,
        };
        assert_eq!(
            jobs_filter_url("http://localhost:8080", &f).unwrap().as_str(),
            "http://localhost:8080/api/jobs?limit=10&workspace=data+team&task_name=etl&status=failed&offset=20"
        );
        let f = JobsFilter {
            limit: 20,
            search: Some("a&b=c"),
            ..Default::default()
        };
        assert_eq!(
            jobs_filter_url("http://localhost:8080", &f)
                .unwrap()
                .as_str(),
            "http://localhost:8080/api/jobs?limit=20&search=a%26b%3Dc"
        );
    }

    #[test]
    fn page_note_names_the_next_offset() {
        assert_eq!(
            page_note(20, 0, 95),
            "Jobs 1-20 of 95 (next page: --offset 20)"
        );
        assert_eq!(page_note(15, 80, 95), "Jobs 81-95 of 95");
    }

    #[tokio::test]
    async fn sends_the_filters_to_the_server() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/jobs"))
            .and(query_param("search", "nightly"))
            .and(query_param("offset", "5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [{"job_id": "j1", "task_name": "nightly", "workspace": "default",
                           "status": "failed", "created_at": "2026-11-21T00:00:00Z"}],
                "total": 6
            })))
            .expect(1)
            .mount(&server)
            .await;
        let f = JobsFilter {
            limit: 20,
            search: Some("nightly"),
            offset: 5,
            ..Default::default()
        };
        cmd_jobs(&Client::new(), &server.uri(), &f).await
    }
}
