//! Templated refs (spec 2026-10-15) — DB layer: the deferred row, the ref
//! memo, the resolution lease and guarded writes, the recovery listing.

use crate::common::{create_job, setup_db};
use anyhow::Result;
use stroem_db::{JobStepRepo, NewJobStep};
use uuid::Uuid;

/// A deferred `ready` step as job creation writes it (spec § 5.1).
pub(crate) fn deferred_step(job_id: Uuid, name: &str) -> NewJobStep {
    NewJobStep {
        job_id,
        step_name: name.to_string(),
        action_name: "hello".to_string(),
        action_type: "deferred".to_string(),
        status: "ready".to_string(),
        required_ability: "deferred".to_string(),
        runner: "none".to_string(),
        action_workspace: Some("etl".to_string()),
        ref_template: Some("{{ input.branch }}".to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn deferred_row_round_trips_with_its_template() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "run")]).await?;
    let row = JobStepRepo::get_step(&pool, job_id, "run").await?.unwrap();
    assert_eq!(row.action_type, "deferred");
    assert_eq!(row.required_ability, "deferred");
    assert_eq!(row.runner, "none");
    assert_eq!(row.ref_template.as_deref(), Some("{{ input.branch }}"));
    assert!(row.action_spec.is_none() && row.action_ref.is_none());
    Ok(())
}
