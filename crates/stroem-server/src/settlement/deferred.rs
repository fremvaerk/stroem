//! The deferred-step pass (spec `docs/superpowers/specs/2026-10-15-templated-refs-design.md`
//! § 5.3). A flow step whose `ref:` is a template waits at `ready` as
//! `action_type = 'deferred'` until this pass renders the template, resolves
//! the commit (one per `(job, owner, ref)`, table `job_ref_pin`), checks the
//! value against the job's ref secret set before and after the commit's
//! config loads, and writes the step's real columns with the code job
//! creation uses. Runs in `Settlement::advance` and `dispatch::init`, after
//! the cascade and before task dispatch. `cascade::run` stays pure.
//!
//! A rendered value is private until both secret checks pass: no span, log
//! line or error text carries it (§ 5.5). Every step failure is a
//! `RenderedRefError`; only DB errors escape.

use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use sqlx::PgPool;
use stroem_common::git_ref::check_rendered_ref;
use stroem_common::models::job::StepStatus;
use stroem_common::models::workflow::{FlowStep, TaskDef, WorkspaceConfig};
use stroem_common::template::render_ref_template;
use stroem_db::{
    DeferredWrite, JobRefPinRepo, JobRepo, JobRow, JobStepRepo, JobStepRow, NewJobStep,
};
use uuid::Uuid;

use crate::config::JobDefaults;
use crate::job_creator::{ref_world_sets, resolve_step_row, RefMemo, StepRowCtx};
use crate::refs::RefWorld;
use crate::render_context::{self, JobContext, LoopSlot, Scope, Snapshots};
use crate::templated_ref::{RenderedRefError, RenderedRefKind};
use crate::workspace::pins::PinRef;
use crate::workspace::WorkspaceManager;
use crate::workspace_set::{collect_redaction_values, WorkspaceSet};

/// Lease length (D12): a resolver whose process dies loses its lease in this time.
pub const DEFERRED_LEASE_SECS: u64 = 60;
/// Renewal period of a held lease.
pub const DEFERRED_LEASE_RENEW_SECS: u64 = 20;

/// Lease length and renewal period. Production uses the default; tests
/// shorten both.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct LeaseTiming {
    pub lease: Duration,
    pub renew: Duration,
}

impl Default for LeaseTiming {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(DEFERRED_LEASE_SECS),
            renew: Duration::from_secs(DEFERRED_LEASE_RENEW_SECS),
        }
    }
}

/// One step's step-failure-or-row: the inner `Err` fails the step.
type Resolution<T> = std::result::Result<T, RenderedRefError>;

/// Owns the renewal task of one resolution lease. `Drop` aborts the task, so
/// renewal stops on EVERY exit — success, error, panic, or the resolver future
/// being dropped (a cancelled HTTP request during `init`). A detached renewal
/// task would keep a `ready` row leased forever, out of Phase 4.6's reach.
struct LeaseGuard {
    token: Uuid,
    lost: Arc<AtomicBool>,
    renew: tokio::task::JoinHandle<()>,
}

impl LeaseGuard {
    /// § 5.3 step 0. `None`: another resolver holds a live lease, or the step
    /// is no longer a deferred `ready` row.
    async fn take(
        pool: &PgPool,
        job_id: Uuid,
        step: &str,
        timing: LeaseTiming,
    ) -> Result<Option<Self>> {
        let token = Uuid::new_v4();
        let secs = timing.lease.as_secs_f64();
        if !JobStepRepo::take_resolve_lease(pool, job_id, step, token, secs).await? {
            return Ok(None);
        }
        let lost = Arc::new(AtomicBool::new(false));
        let (pool, step, flag) = (pool.clone(), step.to_string(), Arc::clone(&lost));
        let renew = tokio::spawn(async move {
            let mut tick = tokio::time::interval(timing.renew);
            tick.tick().await; // the first tick completes at once
            loop {
                tick.tick().await;
                match JobStepRepo::renew_resolve_lease(&pool, job_id, &step, token, secs).await {
                    Ok(true) => {}
                    Ok(false) => {
                        flag.store(true, Ordering::SeqCst);
                        return;
                    }
                    Err(e) => tracing::warn!(
                        %job_id,
                        step = %step,
                        "renewing a resolution lease failed: {e:#}"
                    ),
                }
            }
        });
        Ok(Some(Self { token, lost, renew }))
    }

    /// A renewal changed no row: another resolver took the lease over. The
    /// token fence would refuse this holder's write anyway.
    fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.renew.abort();
    }
}

/// The dispatch round of `Settlement::advance` and `dispatch::init` (spec
/// § 5.3): deferred pass → task dispatch → (advance only) reconcile →
/// approval dispatch. A task or approval dispatch failure re-cascades
/// (`fail_task_step`, an approval failure) and can promote a deferred
/// dependent AFTER this round's deferred pass ran; without another round it
/// would wait for recovery Phase 4.6. The callers run the round again while a
/// round leaves a NEW deferred `ready` row behind.
///
/// "New" is against every row seen so far: a row another resolver holds the
/// lease of stays `ready` and deferred, and must not keep the loop going
/// (Phase 4.6 owns it). A row never re-enters the set once it leaves it (a
/// resolved row is no longer deferred, a failed or cancelled one is
/// terminal), so the seen set only grows. The bound — at most the job's step
/// count plus one rounds, the count re-read before every repeat because a
/// `for_each` expansion adds rows — is a safety net. A job with no deferred
/// row pays one read per round that matches nothing.
pub(crate) struct DispatchRounds {
    /// `None`: the first read failed — run one round only (advance logs it).
    seen: Option<HashSet<String>>,
    repeats: usize,
}

impl DispatchRounds {
    /// Read the deferred `ready` rows before the first round.
    pub(crate) async fn start(pool: &PgPool, job_id: Uuid) -> Result<Self> {
        let ready = JobStepRepo::get_ready_deferred_steps(pool, job_id).await?;
        Ok(Self {
            seen: Some(ready.into_iter().map(|s| s.step_name).collect()),
            repeats: 0,
        })
    }

    /// One round, no repeat (the first read failed).
    pub(crate) fn single() -> Self {
        Self {
            seen: None,
            repeats: 0,
        }
    }

    /// After a round: whether another round is owed.
    pub(crate) async fn again(&mut self, pool: &PgPool, job_id: Uuid) -> Result<bool> {
        let Some(seen) = self.seen.as_mut() else {
            return Ok(false);
        };
        let ready = JobStepRepo::get_ready_deferred_steps(pool, job_id).await?;
        if !absorb_new(seen, ready.into_iter().map(|s| s.step_name)) {
            return Ok(false);
        }
        self.repeats += 1;
        let step_count = JobStepRepo::get_steps_for_job(pool, job_id).await?.len();
        Ok(within_bound(self.repeats, step_count))
    }
}

/// Add `now` to `seen`; whether any name was not there yet.
fn absorb_new(seen: &mut HashSet<String>, now: impl Iterator<Item = String>) -> bool {
    let mut new = false;
    for name in now {
        new |= seen.insert(name);
    }
    new
}

/// The `repeats`-th repeat is allowed while `repeats <= step_count`: at most
/// `step_count + 1` rounds in all.
fn within_bound(repeats: usize, step_count: usize) -> bool {
    repeats <= step_count
}

/// Resolve every deferred `ready` step of `job_id` (spec § 5.3). Loops like
/// `handle_task_steps`: a failure re-cascades, which can promote more
/// deferred steps; bounded by the flow size. Only DB errors escape.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip_all, fields(job_id = %job_id, workspace = %workspace_name))]
pub async fn resolve_deferred_steps(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    task: &TaskDef,
    defaults: JobDefaults,
    snapshots: &Snapshots,
) -> Result<()> {
    resolve_deferred_steps_with(
        workspaces,
        pool,
        workspace_config,
        workspace_name,
        job_id,
        task,
        defaults,
        snapshots,
        LeaseTiming::default(),
    )
    .await
}

/// [`resolve_deferred_steps`] with an explicit lease timing (tests).
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn resolve_deferred_steps_with(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    task: &TaskDef,
    defaults: JobDefaults,
    snapshots: &Snapshots,
    timing: LeaseTiming,
) -> Result<()> {
    // Every failing pass retires at least one step, like `handle_task_steps`.
    // Spec § 5.3: bounded by the number of steps in the JOB, re-read each
    // pass. The flow size is too small: a `for_each` expansion adds instance
    // rows, and a sequential loop with `continue_on_failure` whose instances
    // all fail resolution needs one pass per instance.
    let mut passes = 0usize;
    loop {
        let (failed_any, step_count) = resolve_pass(
            workspaces,
            pool,
            workspace_config,
            workspace_name,
            job_id,
            task,
            defaults,
            snapshots,
            timing,
        )
        .await?;
        passes += 1;
        if !failed_any || passes > step_count {
            break;
        }
    }
    Ok(())
}

/// One pass over the deferred `ready` steps. Returns whether a step was
/// failed, and the job's step count as this pass read it (0 when the job has
/// no deferred `ready` step).
#[allow(clippy::too_many_arguments)]
async fn resolve_pass(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    task: &TaskDef,
    defaults: JobDefaults,
    snapshots: &Snapshots,
    timing: LeaseTiming,
) -> Result<(bool, usize)> {
    let deferred = JobStepRepo::get_ready_deferred_steps(pool, job_id).await?;
    if deferred.is_empty() {
        return Ok((false, 0));
    }
    let Some(job) = JobRepo::get(pool, job_id).await? else {
        return Ok((false, 0));
    };
    let steps = JobStepRepo::get_steps_for_job(pool, job_id).await?;
    let mut failed_any = false;
    for step in &deferred {
        let Some(lease) = LeaseGuard::take(pool, job_id, &step.step_name, timing).await? else {
            continue;
        };
        let outcome = resolve_one(
            workspaces,
            pool,
            workspace_config,
            workspace_name,
            &job,
            &steps,
            step,
            task,
            defaults,
            snapshots,
        )
        .await?;
        if lease.is_lost() {
            continue;
        }
        match outcome {
            Ok(row) => {
                if JobStepRepo::resolve_deferred_tx(
                    pool,
                    job_id,
                    &step.step_name,
                    lease.token,
                    &row,
                )
                .await?
                    == DeferredWrite::Applied
                {
                    tracing::info!(%job_id, step = %step.step_name, "[ref] deferred step resolved");
                }
            }
            Err(err) => {
                let message = format!("[ref] step '{}': {}", step.step_name, err);
                if JobStepRepo::fail_deferred_tx(
                    pool,
                    job_id,
                    &step.step_name,
                    lease.token,
                    &message,
                )
                .await?
                    == DeferredWrite::Applied
                {
                    tracing::warn!(%job_id, "{message}");
                    // The `fail_task_step` pattern: re-cascade so dependents are
                    // skipped and the job can settle.
                    if let Err(e) = crate::settlement::cascade_and_settle(
                        pool,
                        job_id,
                        task,
                        workspace_config,
                        snapshots,
                    )
                    .await
                    {
                        tracing::error!(
                            "Failed to orchestrate after deferred step '{}' failed in job {}: {:#}",
                            step.step_name,
                            job_id,
                            e
                        );
                    }
                    failed_any = true;
                }
            }
        }
    }
    Ok((failed_any, steps.len()))
}

/// § 5.3 steps 1–6 for one step.
#[allow(clippy::too_many_arguments)]
async fn resolve_one(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job: &JobRow,
    steps: &[JobStepRow],
    step: &JobStepRow,
    task: &TaskDef,
    defaults: JobDefaults,
    snapshots: &Snapshots,
) -> Result<Resolution<NewJobStep>> {
    // The owner was decided at creation from the action name (D11).
    let owner = step.action_workspace.as_deref().unwrap_or(workspace_name);
    let unresolvable = || RenderedRefError::new(RenderedRefKind::Unresolvable, owner);
    let Some(template) = step.ref_template.as_deref() else {
        return Ok(Err(unresolvable()));
    };
    let flow_key = step.loop_source.as_deref().unwrap_or(&step.step_name);
    let Some(flow_step) = task.flow.get(flow_key) else {
        return Ok(Err(unresolvable()));
    };

    // 1. Job input, no `secret`, `each` for an instance.
    let job_ctx = JobContext {
        job_id: job.job_id,
        job_input: job.input.as_ref(),
        caller_secrets: &workspace_config.secrets,
        owner_secrets: &workspace_config.secrets,
        snapshots,
        job_revision: job.revision.as_deref(),
        job_ref: job.git_ref.as_deref(),
    };
    let ctx = render_context::build(
        &job_ctx,
        &render_context::views(steps),
        LoopSlot::of(step),
        Scope::StepRef,
    );

    // 2. Render, then the post-render check against the ref secret set.
    let rendered = match render_ref_template(template, ctx.as_value()) {
        Ok(r) => r,
        Err(e) => return Ok(Err(RenderedRefError::from_error(&e, owner))),
    };
    let secrets = match ref_secret_set(workspaces, pool, job).await? {
        Ok(s) => s,
        Err(e) => return Ok(Err(e)),
    };
    let value = match check_rendered_ref(&rendered, &secrets) {
        Ok(v) => v,
        Err(r) => return Ok(Err(RenderedRefError::from_rejection(r, owner))),
    };

    // 3–4. The commit: memo, else resolve; loaded privately; checked against
    // that commit's own secrets; only then in the memo.
    if let Err(e) = select_commit(workspaces, pool, job.job_id, owner, &value).await? {
        return Ok(Err(e));
    }

    // 5–6. `value` passed both checks: it is an identifier from here on. The
    // row is built by the code creation uses, from the row's own owner and
    // action name (decided at creation) and the job config's flow step.
    let mut written = flow_step.clone();
    written.action = written_action(owner, workspace_name, &step.action_name);
    build_row(
        workspaces,
        pool,
        workspace_config,
        workspace_name,
        job,
        &step.step_name,
        &written,
        &value,
        owner,
        &step.action_name,
        defaults,
    )
    .await
}

/// The action name to resolve a deferred row with: a spelling that
/// `refs::ref_owner` maps back to the row's `owner` and `action_name` (both
/// decided at creation, D11). `ref_owner` splits on the FIRST dot, so a dotted
/// local name (`common.slack-notify`, or `billing.local` where `billing` is a
/// configured workspace) must stay qualified with its owner, even in the
/// job's own workspace; written bare, its first segment would be read as a
/// library or another workspace. Only an undotted own-workspace name stays
/// bare, so a library that shares the workspace's name cannot capture it.
fn written_action(owner: &str, workspace_name: &str, action_name: &str) -> String {
    if owner == workspace_name && !action_name.contains('.') {
        action_name.to_string()
    } else {
        format!("{owner}.{action_name}")
    }
}

/// The ref secret set of a step before resolution (spec § 10.3): the live set
/// plus the pin secrets of the job's own pin and of every commit in its
/// `job_ref_pin`. A pin that cannot be loaded fails the step (D7).
pub(crate) async fn ref_secret_set(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    job: &JobRow,
) -> Result<Resolution<Vec<String>>> {
    let set = WorkspaceSet::load(workspaces, &job.workspace, None).await;
    let mut values = collect_redaction_values(&set);
    let mut pins: BTreeSet<(String, String)> = BTreeSet::new();
    if let Some(p) = PinRef::of_job(job) {
        pins.insert((job.workspace.clone(), p.commit));
    }
    for row in JobRefPinRepo::list_for_job(pool, job.job_id).await? {
        pins.insert((row.workspace, row.commit));
    }
    for (ws, commit) in pins {
        let pinned = match workspaces.pinned_config_private(&ws, &commit).await {
            Ok(p) => p,
            Err(e) => return Ok(Err(RenderedRefError::from_error(&e, &ws))),
        };
        values.extend(workspaces.pin_redaction_values(&ws, &pinned).await);
    }
    Ok(Ok(values))
}

/// § 5.3 steps 3–4 / § 5.2: the commit `owner@value` runs at in `job_id`.
/// Looked up in the memo, else resolved; loaded through the private path;
/// the value checked against THAT commit's own secrets; only then written to
/// the memo, first writer wins — if another resolver wrote a different
/// commit first, that one is loaded and checked instead.
pub(crate) async fn select_commit(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    job_id: Uuid,
    owner: &str,
    value: &str,
) -> Result<Resolution<PinRef>> {
    let fail = |e: anyhow::Error| RenderedRefError::from_error(&e, owner);
    let mut commit = match JobRefPinRepo::get(pool, job_id, owner, value).await? {
        Some(c) => c,
        None => match workspaces.pins().resolve(owner, value).await {
            Ok(pin) => pin.commit,
            Err(e) => return Ok(Err(fail(e.into()))),
        },
    };
    for _ in 0..3 {
        let pinned = match workspaces.pinned_config_private(owner, &commit).await {
            Ok(p) => p,
            Err(e) => return Ok(Err(fail(e))),
        };
        let own = workspaces.pin_redaction_values(owner, &pinned).await;
        if let Err(r) = check_rendered_ref(value, &own) {
            return Ok(Err(RenderedRefError::from_rejection(r, owner)));
        }
        let stored = JobRefPinRepo::insert_first_wins(pool, job_id, owner, value, &commit).await?;
        if stored == commit {
            return Ok(Ok(PinRef {
                git_ref: value.to_string(),
                commit,
            }));
        }
        commit = stored;
    }
    Ok(Err(RenderedRefError::new(
        RenderedRefKind::Unresolvable,
        owner,
    )))
}

/// § 5.3 steps 5–6 through `job_creator::resolve_step_row`, with nested
/// static refs kept in the job's memo too (contract deviation 8): the memo is
/// seeded from `job_ref_pin`, every entry the row met is written first-writer-
/// wins, and the row is rebuilt (at most 3 times) when one disagrees.
#[allow(clippy::too_many_arguments)]
async fn build_row(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job: &JobRow,
    step_name: &str,
    written: &FlowStep,
    value: &str,
    owner: &str,
    action_name: &str,
    defaults: JobDefaults,
) -> Result<Resolution<NewJobStep>> {
    let (library_names, configured, git) = ref_world_sets(workspaces);
    let world = RefWorld {
        library_names: &library_names,
        configured: &configured,
        git: &git,
    };
    let ws_set = WorkspaceSet::load(workspaces, workspace_name, Some(workspace_config)).await;
    let job_pin = PinRef::of_job(job);
    for _ in 0..3 {
        let memo = RefMemo::from_rows(&JobRefPinRepo::list_for_job(pool, job.job_id).await?);
        let ctx = StepRowCtx {
            workspaces,
            job_id: job.job_id,
            workspace_name,
            workspace_config,
            task_name: &job.task_name,
            job_pin: job_pin.as_ref(),
            world: &world,
            memo: &memo,
            ws_set: &ws_set,
            defaults,
        };
        let row = match resolve_step_row(&ctx, step_name, written, StepStatus::Ready, Some(value))
            .await
        {
            Ok(row) => row,
            Err(e) => return Ok(Err(RenderedRefError::from_error(&e, owner))),
        };
        // D11: the owner and the action name were decided at creation, and
        // the deferred row's values must win over the live config of an
        // unpinned job (which can re-point the step's `action:` before
        // promotion). `written_action` spells them so that the row keeps
        // them; this checks the result.
        if row.action_workspace.as_deref() != Some(owner) || row.action_name != action_name {
            return Ok(Err(RenderedRefError::new(
                RenderedRefKind::Unresolvable,
                owner,
            )));
        }
        let mut agreed = true;
        for entry in memo.entries() {
            let stored = JobRefPinRepo::insert_first_wins(
                pool,
                job.job_id,
                &entry.workspace,
                &entry.git_ref,
                &entry.commit,
            )
            .await?;
            agreed &= stored == entry.commit;
        }
        if agreed {
            return Ok(Ok(row));
        }
    }
    Ok(Err(RenderedRefError::new(
        RenderedRefKind::Unresolvable,
        owner,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::ref_owner;
    use std::collections::HashSet;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// `written_action` round-trips through `ref_owner`: the owner and the
    /// local name stamped at creation come back unchanged.
    #[test]
    fn written_action_round_trips_through_ref_owner() {
        let configured = set(&["etl", "billing"]);
        // `common` is a library: written bare, `common.slack-notify` would be
        // refused as a library item.
        let libraries = set(&["common"]);
        for (owner, local, spelled) in [
            ("etl", "hello", "hello"),
            ("etl", "common.slack-notify", "etl.common.slack-notify"),
            ("etl", "billing.local", "etl.billing.local"),
            ("etl", ".hidden", "etl..hidden"),
            ("etl", "trailing.", "etl.trailing."),
            ("billing", "export", "billing.export"),
            ("billing", "x.y", "billing.x.y"),
        ] {
            let written = written_action(owner, "etl", local);
            assert_eq!(written, spelled);
            let t = ref_owner("etl", &written, &libraries, &configured).unwrap();
            assert_eq!(
                (t.owner.as_str(), t.local_name.as_str()),
                (owner, local),
                "{written}"
            );
        }
        // An undotted own name stays bare, so a library named like the job's
        // workspace cannot capture it.
        let written = written_action("etl", "etl", "hello");
        let t = ref_owner("etl", &written, &set(&["etl"]), &configured).unwrap();
        assert_eq!((t.owner.as_str(), t.local_name.as_str()), ("etl", "hello"));
    }

    fn names(n: &[&str]) -> impl Iterator<Item = String> {
        n.iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn another_round_is_owed_only_for_a_new_deferred_row() {
        // A dispatch failure promoted `after`: one more round.
        let mut seen = set(&[]);
        assert!(absorb_new(&mut seen, names(&["after"])));
        // `after` is still there (leased by another resolver): no repeat.
        assert!(!absorb_new(&mut seen, names(&["after"])));
        // Nothing left: no repeat.
        assert!(!absorb_new(&mut seen, names(&[])));
        // A row seen earlier and gone does not count when it is seen again.
        assert!(!absorb_new(&mut seen, names(&["after"])));
        // A second new row does.
        assert!(absorb_new(&mut seen, names(&["after", "x"])));
        assert_eq!(seen, set(&["after", "x"]));
    }

    #[test]
    fn rounds_are_bounded_by_the_step_count_plus_one() {
        // Two steps: the first round plus two repeats.
        assert!(within_bound(1, 2));
        assert!(within_bound(2, 2));
        assert!(!within_bound(3, 2));
    }

    #[tokio::test]
    async fn a_single_round_never_repeats() {
        // `again` returns before it touches the pool.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@127.0.0.1:1/none")
            .unwrap();
        let mut rounds = DispatchRounds::single();
        assert!(!rounds.again(&pool, Uuid::new_v4()).await.unwrap());
    }
}
