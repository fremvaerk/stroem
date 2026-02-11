use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Job execution status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// Job step execution status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Ready,
    Running,
    Completed,
    Failed,
    Skipped,
}

/// Source of job execution
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    Trigger,
    User,
    Api,
    Webhook,
}

/// Job represents a single task execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub job_id: Uuid,
    pub workspace: String,
    pub task_name: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    pub status: JobStatus,
    pub source_type: SourceType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
}

/// JobStep represents a single step execution within a job
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStep {
    pub job_id: Uuid,
    pub step_name: String,
    pub action_name: String,
    pub action_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_spec: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    pub status: StepStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_job_status_serialization() {
        let status = JobStatus::Running;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""running""#);

        let deserialized: JobStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, JobStatus::Running);
    }

    #[test]
    fn test_step_status_serialization() {
        let status = StepStatus::Ready;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""ready""#);

        let deserialized: StepStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, StepStatus::Ready);
    }

    #[test]
    fn test_source_type_serialization() {
        let source = SourceType::Webhook;
        let json = serde_json::to_string(&source).unwrap();
        assert_eq!(json, r#""webhook""#);

        let deserialized: SourceType = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, SourceType::Webhook);
    }

    #[test]
    fn test_job_creation() {
        let job = Job {
            job_id: Uuid::new_v4(),
            workspace: "default".to_string(),
            task_name: "test-task".to_string(),
            mode: "distributed".to_string(),
            input: Some(serde_json::json!({"name": "test"})),
            output: None,
            status: JobStatus::Pending,
            source_type: SourceType::User,
            source_id: None,
            worker_id: None,
            revision: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            log_path: None,
        };

        assert_eq!(job.workspace, "default");
        assert_eq!(job.task_name, "test-task");
        assert_eq!(job.status, JobStatus::Pending);
    }

    #[test]
    fn test_job_step_creation() {
        let job_id = Uuid::new_v4();
        let step = JobStep {
            job_id,
            step_name: "step1".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(serde_json::json!({"cmd": "echo hello"})),
            input: Some(serde_json::json!({"name": "world"})),
            output: None,
            status: StepStatus::Pending,
            worker_id: None,
            started_at: None,
            completed_at: None,
            error_message: None,
        };

        assert_eq!(step.job_id, job_id);
        assert_eq!(step.step_name, "step1");
        assert_eq!(step.action_type, "shell");
        assert_eq!(step.status, StepStatus::Pending);
    }

    #[test]
    fn test_job_json_serialization() {
        let job = Job {
            job_id: Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            workspace: "default".to_string(),
            task_name: "test".to_string(),
            mode: "distributed".to_string(),
            input: None,
            output: None,
            status: JobStatus::Completed,
            source_type: SourceType::Api,
            source_id: None,
            worker_id: None,
            revision: None,
            created_at: DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .into(),
            started_at: None,
            completed_at: None,
            log_path: None,
        };

        let json = serde_json::to_value(&job).unwrap();
        assert_eq!(json["workspace"], "default");
        assert_eq!(json["status"], "completed");
        assert_eq!(json["source_type"], "api");
    }
}
