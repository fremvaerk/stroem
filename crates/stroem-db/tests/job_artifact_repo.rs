use stroem_db::repos::job_artifact::{JobArtifactRepo, NewArtifactRow};

mod common;
use common::{create_job, setup_db};

#[tokio::test(flavor = "multi_thread")]
async fn upsert_then_list_returns_artifacts_for_job() {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "ws1", "task1").await;
    let repo = JobArtifactRepo::new(pool.clone());

    repo.upsert(NewArtifactRow {
        job_id,
        step_name: "build".into(),
        name: "report.html".into(),
        content_type: "text/html".into(),
        size_bytes: 1234,
        storage_key: "artifacts/ws1/{}/build/report.html".replace("{}", &job_id.to_string()),
    })
    .await
    .unwrap();

    let rows = repo.list_for_job(job_id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "report.html");
    assert_eq!(rows[0].size_bytes, 1234);
}

#[tokio::test(flavor = "multi_thread")]
async fn upsert_same_name_replaces_row() {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "ws1", "task1").await;
    let repo = JobArtifactRepo::new(pool.clone());

    let make_row = |step: &str, size: i64| NewArtifactRow {
        job_id,
        step_name: step.into(),
        name: "out.txt".into(),
        content_type: "text/plain".into(),
        size_bytes: size,
        storage_key: format!("artifacts/ws1/{job_id}/{step}/out.txt"),
    };
    repo.upsert(make_row("a", 1)).await.unwrap();
    repo.upsert(make_row("b", 2)).await.unwrap();

    let rows = repo.list_for_job(job_id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].step_name, "b");
    assert_eq!(rows[0].size_bytes, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_for_job_removes_all_rows() {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "ws1", "task1").await;
    let repo = JobArtifactRepo::new(pool.clone());

    repo.upsert(NewArtifactRow {
        job_id,
        step_name: "s".into(),
        name: "a.txt".into(),
        content_type: "text/plain".into(),
        size_bytes: 1,
        storage_key: format!("artifacts/ws1/{job_id}/s/a.txt"),
    })
    .await
    .unwrap();
    repo.upsert(NewArtifactRow {
        job_id,
        step_name: "s".into(),
        name: "b.txt".into(),
        content_type: "text/plain".into(),
        size_bytes: 2,
        storage_key: format!("artifacts/ws1/{job_id}/s/b.txt"),
    })
    .await
    .unwrap();

    let deleted = repo.delete_for_job(job_id).await.unwrap();
    assert_eq!(deleted, 2);
    assert_eq!(repo.list_for_job(job_id).await.unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_for_step_removes_only_that_step() {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "ws1", "task1").await;
    let repo = JobArtifactRepo::new(pool.clone());

    repo.upsert(NewArtifactRow {
        job_id,
        step_name: "build".into(),
        name: "out.txt".into(),
        content_type: "text/plain".into(),
        size_bytes: 1,
        storage_key: format!("artifacts/ws1/{job_id}/build/out.txt"),
    })
    .await
    .unwrap();
    repo.upsert(NewArtifactRow {
        job_id,
        step_name: "test".into(),
        name: "report.html".into(),
        content_type: "text/html".into(),
        size_bytes: 2,
        storage_key: format!("artifacts/ws1/{job_id}/test/report.html"),
    })
    .await
    .unwrap();

    let removed = repo.delete_for_step(job_id, "build").await.unwrap();
    assert_eq!(removed, 1);
    let rows = repo.list_for_job(job_id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].step_name, "test");
}
