use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Job execution status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
    Skipped,
}

impl fmt::Display for JobStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

impl AsRef<str> for JobStatus {
    fn as_ref(&self) -> &str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }
}

impl std::str::FromStr for JobStatus {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "skipped" => Ok(Self::Skipped),
            other => anyhow::bail!("Unknown job status: {}", other),
        }
    }
}

/// Job step execution status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Ready,
    Claimed,
    Running,
    Completed,
    Failed,
    Skipped,
    Cancelled,
    /// Approval gate is waiting for a human to approve or reject.
    Suspended,
}

impl fmt::Display for StepStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

impl AsRef<str> for StepStatus {
    fn as_ref(&self) -> &str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Claimed => "claimed",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
            Self::Suspended => "suspended",
        }
    }
}

impl std::str::FromStr for StepStatus {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "ready" => Ok(Self::Ready),
            "claimed" => Ok(Self::Claimed),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "skipped" => Ok(Self::Skipped),
            "cancelled" => Ok(Self::Cancelled),
            "suspended" => Ok(Self::Suspended),
            other => anyhow::bail!("Unknown step status: {}", other),
        }
    }
}

/// Source that triggered a job
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    Api,
    User,
    Trigger,
    Webhook,
    Hook,
    Task,
    /// Job created by an event source trigger (long-running queue consumer).
    EventSource,
    /// Job created as a retry of a failed job (task-level retry).
    Retry,
}

impl fmt::Display for SourceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

impl AsRef<str> for SourceType {
    fn as_ref(&self) -> &str {
        match self {
            Self::Api => "api",
            Self::User => "user",
            Self::Trigger => "trigger",
            Self::Webhook => "webhook",
            Self::Hook => "hook",
            Self::Task => "task",
            Self::EventSource => "event_source",
            Self::Retry => "retry",
        }
    }
}

impl std::str::FromStr for SourceType {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "api" => Ok(Self::Api),
            "user" => Ok(Self::User),
            "trigger" => Ok(Self::Trigger),
            "webhook" => Ok(Self::Webhook),
            "hook" => Ok(Self::Hook),
            "task" => Ok(Self::Task),
            "event_source" => Ok(Self::EventSource),
            "retry" => Ok(Self::Retry),
            other => anyhow::bail!("Unknown source type: {}", other),
        }
    }
}

/// Type of action a step executes
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionType {
    Script,
    Docker,
    Pod,
    Task,
    Agent,
    /// Human approval gate — suspends the job until a reviewer approves or rejects.
    Approval,
}

impl fmt::Display for ActionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

impl AsRef<str> for ActionType {
    fn as_ref(&self) -> &str {
        match self {
            Self::Script => "script",
            Self::Docker => "docker",
            Self::Pod => "pod",
            Self::Task => "task",
            Self::Agent => "agent",
            Self::Approval => "approval",
        }
    }
}

impl std::str::FromStr for ActionType {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "script" => Ok(Self::Script),
            "docker" => Ok(Self::Docker),
            "pod" => Ok(Self::Pod),
            "task" => Ok(Self::Task),
            "agent" => Ok(Self::Agent),
            "approval" => Ok(Self::Approval),
            other => anyhow::bail!("Unknown action type: {}", other),
        }
    }
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
    fn test_job_status_display() {
        assert_eq!(JobStatus::Pending.to_string(), "pending");
        assert_eq!(JobStatus::Running.to_string(), "running");
        assert_eq!(JobStatus::Completed.to_string(), "completed");
        assert_eq!(JobStatus::Failed.to_string(), "failed");
        assert_eq!(JobStatus::Cancelled.to_string(), "cancelled");
        assert_eq!(JobStatus::Skipped.to_string(), "skipped");
    }

    #[test]
    fn test_job_status_from_str() {
        assert_eq!("pending".parse::<JobStatus>().unwrap(), JobStatus::Pending);
        assert_eq!("running".parse::<JobStatus>().unwrap(), JobStatus::Running);
        assert_eq!(
            "completed".parse::<JobStatus>().unwrap(),
            JobStatus::Completed
        );
        assert_eq!("failed".parse::<JobStatus>().unwrap(), JobStatus::Failed);
        assert_eq!(
            "cancelled".parse::<JobStatus>().unwrap(),
            JobStatus::Cancelled
        );
        assert_eq!("skipped".parse::<JobStatus>().unwrap(), JobStatus::Skipped);
        assert!("invalid".parse::<JobStatus>().is_err());
    }

    #[test]
    fn test_job_status_serde_roundtrip() {
        let status = JobStatus::Completed;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""completed""#);
        let parsed: JobStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, status);

        let skipped = JobStatus::Skipped;
        let json_skipped = serde_json::to_string(&skipped).unwrap();
        assert_eq!(json_skipped, r#""skipped""#);
        let parsed_skipped: JobStatus = serde_json::from_str(&json_skipped).unwrap();
        assert_eq!(parsed_skipped, skipped);
    }

    #[test]
    fn test_step_status_display() {
        assert_eq!(StepStatus::Pending.to_string(), "pending");
        assert_eq!(StepStatus::Ready.to_string(), "ready");
        assert_eq!(StepStatus::Claimed.to_string(), "claimed");
        assert_eq!(StepStatus::Running.to_string(), "running");
        assert_eq!(StepStatus::Completed.to_string(), "completed");
        assert_eq!(StepStatus::Failed.to_string(), "failed");
        assert_eq!(StepStatus::Skipped.to_string(), "skipped");
        assert_eq!(StepStatus::Cancelled.to_string(), "cancelled");
        assert_eq!(StepStatus::Suspended.to_string(), "suspended");
    }

    #[test]
    fn test_step_status_from_str() {
        assert_eq!(
            "pending".parse::<StepStatus>().unwrap(),
            StepStatus::Pending
        );
        assert_eq!("ready".parse::<StepStatus>().unwrap(), StepStatus::Ready);
        assert_eq!(
            "claimed".parse::<StepStatus>().unwrap(),
            StepStatus::Claimed
        );
        assert_eq!(
            "running".parse::<StepStatus>().unwrap(),
            StepStatus::Running
        );
        assert_eq!(
            "completed".parse::<StepStatus>().unwrap(),
            StepStatus::Completed
        );
        assert_eq!("failed".parse::<StepStatus>().unwrap(), StepStatus::Failed);
        assert_eq!(
            "skipped".parse::<StepStatus>().unwrap(),
            StepStatus::Skipped
        );
        assert_eq!(
            "cancelled".parse::<StepStatus>().unwrap(),
            StepStatus::Cancelled
        );
        assert_eq!(
            "suspended".parse::<StepStatus>().unwrap(),
            StepStatus::Suspended
        );
        assert!("invalid".parse::<StepStatus>().is_err());
    }

    #[test]
    fn test_step_status_serde_roundtrip() {
        let status = StepStatus::Ready;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""ready""#);
        let parsed: StepStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, status);
    }

    #[test]
    fn test_step_status_cancelled_serialization() {
        let status = StepStatus::Cancelled;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""cancelled""#);

        let deserialized: StepStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, StepStatus::Cancelled);
    }

    #[test]
    fn test_source_type_display() {
        assert_eq!(SourceType::Api.to_string(), "api");
        assert_eq!(SourceType::User.to_string(), "user");
        assert_eq!(SourceType::Trigger.to_string(), "trigger");
        assert_eq!(SourceType::Webhook.to_string(), "webhook");
        assert_eq!(SourceType::Hook.to_string(), "hook");
        assert_eq!(SourceType::Task.to_string(), "task");
        assert_eq!(SourceType::EventSource.to_string(), "event_source");
        assert_eq!(SourceType::Retry.to_string(), "retry");
    }

    #[test]
    fn test_source_type_from_str() {
        assert_eq!("api".parse::<SourceType>().unwrap(), SourceType::Api);
        assert_eq!("user".parse::<SourceType>().unwrap(), SourceType::User);
        assert_eq!(
            "trigger".parse::<SourceType>().unwrap(),
            SourceType::Trigger
        );
        assert_eq!(
            "webhook".parse::<SourceType>().unwrap(),
            SourceType::Webhook
        );
        assert_eq!("hook".parse::<SourceType>().unwrap(), SourceType::Hook);
        assert_eq!("task".parse::<SourceType>().unwrap(), SourceType::Task);
        assert_eq!(
            "event_source".parse::<SourceType>().unwrap(),
            SourceType::EventSource
        );
        assert_eq!("retry".parse::<SourceType>().unwrap(), SourceType::Retry);
        assert!("invalid".parse::<SourceType>().is_err());
    }

    #[test]
    fn test_source_type_event_source_roundtrip() {
        let source = SourceType::EventSource;
        let json = serde_json::to_string(&source).unwrap();
        assert_eq!(json, r#""event_source""#);
        let parsed: SourceType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, source);
    }

    #[test]
    fn test_source_type_serde_roundtrip() {
        let source = SourceType::Webhook;
        let json = serde_json::to_string(&source).unwrap();
        assert_eq!(json, r#""webhook""#);
        let parsed: SourceType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, source);
    }

    #[test]
    fn test_action_type_display() {
        assert_eq!(ActionType::Script.to_string(), "script");
        assert_eq!(ActionType::Docker.to_string(), "docker");
        assert_eq!(ActionType::Pod.to_string(), "pod");
        assert_eq!(ActionType::Task.to_string(), "task");
        assert_eq!(ActionType::Agent.to_string(), "agent");
        assert_eq!(ActionType::Approval.to_string(), "approval");
    }

    #[test]
    fn test_action_type_from_str() {
        assert_eq!("script".parse::<ActionType>().unwrap(), ActionType::Script);
        assert_eq!("docker".parse::<ActionType>().unwrap(), ActionType::Docker);
        assert_eq!("pod".parse::<ActionType>().unwrap(), ActionType::Pod);
        assert_eq!("task".parse::<ActionType>().unwrap(), ActionType::Task);
        assert_eq!("agent".parse::<ActionType>().unwrap(), ActionType::Agent);
        assert_eq!(
            "approval".parse::<ActionType>().unwrap(),
            ActionType::Approval
        );
        assert!("invalid".parse::<ActionType>().is_err());
    }

    #[test]
    fn test_action_type_serde_roundtrip() {
        let action_type = ActionType::Script;
        let json = serde_json::to_string(&action_type).unwrap();
        assert_eq!(json, r#""script""#);
        let parsed: ActionType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, action_type);

        let agent_type = ActionType::Agent;
        let json = serde_json::to_string(&agent_type).unwrap();
        assert_eq!(json, r#""agent""#);
        let parsed: ActionType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, agent_type);

        let approval_type = ActionType::Approval;
        let json = serde_json::to_string(&approval_type).unwrap();
        assert_eq!(json, r#""approval""#);
        let parsed: ActionType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, approval_type);
    }

    #[test]
    fn test_step_status_suspended_serialization() {
        let status = StepStatus::Suspended;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""suspended""#);

        let deserialized: StepStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, StepStatus::Suspended);
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
    fn test_job_status_serialization() {
        let status = JobStatus::Running;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""running""#);

        let deserialized: JobStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, JobStatus::Running);
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
            action_type: "script".to_string(),
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
        assert_eq!(step.action_type, "script");
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
