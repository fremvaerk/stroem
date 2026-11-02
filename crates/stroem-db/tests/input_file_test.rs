use crate::common::{create_job, setup_db};
use chrono::{Duration, Utc};
use sqlx::PgPool;
use std::time::Duration as StdDuration;
use stroem_db::{
    AttachRule, InputFileRepo, JobInputFileRepo, JobRepo, NewInputFile, UploadTokenRepo, UserRepo,
};
use uuid::Uuid;

fn new_id() -> String {
    format!("file_{}", Uuid::new_v4().simple())
}

async fn make_user(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    UserRepo::create(pool, id, &format!("{id}@example.com"), None, None)
        .await
        .unwrap();
    id
}

async fn make_file(pool: &PgPool, ws: &str, task: &str, user: Option<Uuid>) -> String {
    let id = new_id();
    InputFileRepo::insert(
        pool,
        &NewInputFile {
            file_id: &id,
            workspace: ws,
            task_name: task,
            uploaded_by_user_id: user,
            origin: "api",
            original_name: "My File.csv",
            safe_name: "My_File.csv",
            content_type: "text/csv",
            size_bytes: 12,
            sha256: "abc",
            storage_key: &format!("inputs/{id}"),
        },
    )
    .await
    .unwrap();
    id
}

async fn attach(
    pool: &PgPool,
    job: Uuid,
    field: &str,
    file: &str,
    rules: &[AttachRule<'_>],
) -> bool {
    let mut tx = pool.begin().await.unwrap();
    let ok = JobInputFileRepo::attach_tx(&mut tx, job, field, file, rules)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    ok
}

fn own<'a>(ws: &'a str, task: &'a str, user: Option<Uuid>, ttl: i64) -> AttachRule<'a> {
    AttachRule::Own {
        workspace: ws,
        task_name: task,
        user_id: user,
        ttl_secs: ttl,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_own_upload_matches_user_task_ttl() {
    let pool = setup_db().await;
    let u1 = make_user(&pool).await;
    let u2 = make_user(&pool).await;
    let job = create_job(&pool, "ws", "t").await;
    let f = make_file(&pool, "ws", "t", Some(u1)).await;

    assert!(!attach(&pool, job, "a", &f, &[own("ws", "t", Some(u2), 3600)]).await);
    assert!(!attach(&pool, job, "a", &f, &[own("ws", "other", Some(u1), 3600)]).await);
    assert!(!attach(&pool, job, "a", &f, &[own("other", "t", Some(u1), 3600)]).await);
    assert!(!attach(&pool, job, "a", &f, &[own("ws", "t", None, 3600)]).await);
    assert!(attach(&pool, job, "a", &f, &[own("ws", "t", Some(u1), 3600)]).await);

    // NULL owner matches user_id None only.
    let fnull = make_file(&pool, "ws", "t", None).await;
    assert!(!attach(&pool, job, "b", &fnull, &[own("ws", "t", Some(u1), 3600)]).await);
    assert!(attach(&pool, job, "b", &fnull, &[own("ws", "t", None, 3600)]).await);

    // Older than the ttl.
    let old = make_file(&pool, "ws", "t", Some(u1)).await;
    sqlx::query("UPDATE input_file SET created_at = NOW() - INTERVAL '2 days' WHERE file_id = $1")
        .bind(&old)
        .execute(&pool)
        .await
        .unwrap();
    assert!(!attach(&pool, job, "c", &old, &[own("ws", "t", Some(u1), 86400)]).await);

    // Unknown file.
    assert!(
        !attach(
            &pool,
            job,
            "d",
            &new_id(),
            &[own("ws", "t", Some(u1), 86400)]
        )
        .await
    );
    // No rules.
    assert!(!attach(&pool, job, "e", &f, &[]).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_lineage_requires_attachment_on_source() {
    let pool = setup_db().await;
    let u = make_user(&pool).await;
    let source = create_job(&pool, "ws", "t").await;
    let newjob = create_job(&pool, "ws", "t").await;
    let f = make_file(&pool, "ws", "t", Some(u)).await;
    let rule = [AttachRule::Lineage { job_id: source }];

    // The source holds a DIFFERENT file: the lineage rule must bind file_id.
    let g = make_file(&pool, "ws", "t", Some(u)).await;
    assert!(attach(&pool, source, "g", &g, &[own("ws", "t", Some(u), 3600)]).await);
    assert!(!attach(&pool, newjob, "a", &f, &rule).await);
    assert!(attach(&pool, source, "a", &f, &[own("ws", "t", Some(u), 3600)]).await);
    assert!(attach(&pool, newjob, "a", &f, &rule).await);

    // Several rules: any match wins, in either order (Re-run shape).
    let u2 = make_user(&pool).await;
    let j2 = create_job(&pool, "ws", "t").await;
    let j3 = create_job(&pool, "ws", "t").await;
    let no_own = own("ws", "t", Some(u2), 3600);
    assert!(
        attach(
            &pool,
            j2,
            "a",
            &f,
            &[no_own, AttachRule::Lineage { job_id: source }]
        )
        .await
    );
    // A job holding no attachment: a non-matching lineage source.
    let empty = create_job(&pool, "ws", "t").await;
    assert!(
        attach(
            &pool,
            j3,
            "a",
            &f,
            &[
                AttachRule::Lineage { job_id: empty },
                own("ws", "t", Some(u), 3600)
            ]
        )
        .await
    );
    // None matching -> false.
    let j4 = create_job(&pool, "ws", "t").await;
    assert!(
        !attach(
            &pool,
            j4,
            "a",
            &f,
            &[
                AttachRule::Lineage { job_id: empty },
                own("ws", "t", Some(u2), 3600)
            ]
        )
        .await
    );
    let got = JobInputFileRepo::get_attached(&pool, newjob, &f)
        .await
        .unwrap();
    assert!(got.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_refuses_deleting_file() {
    let pool = setup_db().await;
    let job = create_job(&pool, "ws", "t").await;
    let f = make_file(&pool, "ws", "t", None).await;
    assert!(InputFileRepo::mark_deleting(&pool, &f)
        .await
        .unwrap()
        .is_some());
    assert!(!attach(&pool, job, "a", &f, &[own("ws", "t", None, 3600)]).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_for_job_joins_file_metadata() {
    let pool = setup_db().await;
    let job = create_job(&pool, "ws", "t").await;
    let f1 = make_file(&pool, "ws", "t", None).await;
    let f2 = make_file(&pool, "ws", "t", None).await;
    let r = [own("ws", "t", None, 3600)];
    assert!(attach(&pool, job, "zeta", &f1, &r).await);
    assert!(attach(&pool, job, "alpha", &f2, &r).await);

    let rows = JobInputFileRepo::list_for_job(&pool, job).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].field, "alpha");
    assert_eq!(rows[0].file_id, f2);
    assert_eq!(rows[0].safe_name, "My_File.csv");
    assert_eq!(rows[0].original_name, "My File.csv");
    assert_eq!(rows[0].content_type, "text/csv");
    assert_eq!(rows[0].size_bytes, 12);
    assert_eq!(rows[0].sha256, "abc");
    assert_eq!(rows[0].storage_key, format!("inputs/{f2}"));
    assert_eq!(rows[1].field, "zeta");

    let one = JobInputFileRepo::get_for_field(&pool, job, "zeta")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(one.file_id, f1);
    assert!(JobInputFileRepo::get_for_field(&pool, job, "nope")
        .await
        .unwrap()
        .is_none());
    assert!(JobInputFileRepo::get_attached(&pool, job, &new_id())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_token_single_use_and_expiry() {
    let pool = setup_db().await;
    let u = make_user(&pool).await;
    UploadTokenRepo::insert(
        &pool,
        "tok1",
        u,
        "ws",
        "t",
        "a.csv",
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    let row = UploadTokenRepo::consume(&pool, "tok1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.user_id, u);
    assert_eq!(row.workspace, "ws");
    assert_eq!(row.task_name, "t");
    assert_eq!(row.original_name, "a.csv");
    assert!(UploadTokenRepo::consume(&pool, "tok1")
        .await
        .unwrap()
        .is_none());
    assert!(UploadTokenRepo::consume(&pool, "missing")
        .await
        .unwrap()
        .is_none());

    UploadTokenRepo::insert(
        &pool,
        "tok2",
        u,
        "ws",
        "t",
        "a.csv",
        Utc::now() - Duration::hours(1),
    )
    .await
    .unwrap();
    assert!(UploadTokenRepo::consume(&pool, "tok2")
        .await
        .unwrap()
        .is_none());
    assert!(UploadTokenRepo::delete_expired(&pool).await.unwrap() >= 1);
    let left: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM input_file_upload_token WHERE token_sha256 = 'tok2'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn job_delete_removes_attachments_not_files() {
    let pool = setup_db().await;
    let job = create_job(&pool, "ws", "t").await;
    let f = make_file(&pool, "ws", "t", None).await;
    assert!(attach(&pool, job, "a", &f, &[own("ws", "t", None, 3600)]).await);
    JobRepo::delete(&pool, job).await.unwrap();
    assert!(JobInputFileRepo::list_for_job(&pool, job)
        .await
        .unwrap()
        .is_empty());
    assert!(InputFileRepo::get(&pool, &f).await.unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_vs_creation_creation_first() {
    let pool = setup_db().await;
    let job = create_job(&pool, "ws", "t").await;
    let f = make_file(&pool, "ws", "t", None).await;

    let mut a = pool.begin().await.unwrap();
    assert!(
        JobInputFileRepo::attach_tx(&mut a, job, "a", &f, &[own("ws", "t", None, 3600)])
            .await
            .unwrap()
    );
    let p2 = pool.clone();
    let f2 = f.clone();
    let mut handle = tokio::spawn(async move { InputFileRepo::mark_deleting(&p2, &f2).await });
    assert!(
        tokio::time::timeout(StdDuration::from_millis(300), &mut handle)
            .await
            .is_err(),
        "mark_deleting must block on the FOR SHARE lock"
    );
    a.commit().await.unwrap();
    assert!(handle.await.unwrap().unwrap().is_none());
    let row = InputFileRepo::get(&pool, &f).await.unwrap().unwrap();
    assert!(row.deleting_at.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_vs_creation_sweep_first() {
    let pool = setup_db().await;
    let job = create_job(&pool, "ws", "t").await;
    let f = make_file(&pool, "ws", "t", None).await;

    let mut s = pool.begin().await.unwrap();
    assert!(InputFileRepo::mark_deleting_tx(&mut s, &f)
        .await
        .unwrap()
        .is_some());
    let p2 = pool.clone();
    let f2 = f.clone();
    let mut handle = tokio::spawn(async move {
        let mut tx = p2.begin().await.unwrap();
        let r = JobInputFileRepo::attach_tx(
            &mut tx,
            job,
            "a",
            &f2,
            &[AttachRule::Own {
                workspace: "ws",
                task_name: "t",
                user_id: None,
                ttl_secs: 3600,
            }],
        )
        .await;
        tx.commit().await.unwrap();
        r
    });
    assert!(
        tokio::time::timeout(StdDuration::from_millis(300), &mut handle)
            .await
            .is_err(),
        "attach_tx must block on the FOR UPDATE lock"
    );
    s.commit().await.unwrap();
    assert!(!handle.await.unwrap().unwrap());
    assert!(JobInputFileRepo::list_for_job(&pool, job)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_candidates_skips_attached_and_young() {
    let pool = setup_db().await;
    let job = create_job(&pool, "ws", "t").await;
    let young = make_file(&pool, "ws", "t", None).await;
    let old_free = make_file(&pool, "ws", "t", None).await;
    let old_attached = make_file(&pool, "ws", "t", None).await;
    let marked = make_file(&pool, "ws", "t", None).await;
    assert!(
        attach(
            &pool,
            job,
            "a",
            &old_attached,
            &[own("ws", "t", None, 86400)]
        )
        .await
    );
    for f in [&old_free, &old_attached] {
        sqlx::query(
            "UPDATE input_file SET created_at = NOW() - INTERVAL '2 days' WHERE file_id = $1",
        )
        .bind(f)
        .execute(&pool)
        .await
        .unwrap();
    }
    InputFileRepo::mark_deleting(&pool, &marked).await.unwrap();

    let c = InputFileRepo::sweep_candidates(&pool, 86400, 100)
        .await
        .unwrap();
    assert!(c.contains(&old_free));
    assert!(c.contains(&marked));
    assert!(!c.contains(&young));
    assert!(!c.contains(&old_attached));

    // delete: only a marked, unattached row.
    assert!(!InputFileRepo::delete(&pool, &young).await.unwrap());
    assert!(!InputFileRepo::delete(&pool, &old_free).await.unwrap());
    assert!(InputFileRepo::delete(&pool, &marked).await.unwrap());
    assert!(InputFileRepo::get(&pool, &marked).await.unwrap().is_none());
    let limited = InputFileRepo::sweep_candidates(&pool, 86400, 1)
        .await
        .unwrap();
    assert_eq!(limited.len(), 1);
}
