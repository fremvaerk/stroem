//! Per-workspace watcher (spec § 4). One tick = at most one peek and at most
//! one admitted load; the loop never waits on a mutex or a permit.

use super::availability::{
    instant_after, Caller, Effect, Event, PeekObservation, Policy, ReloadSettings,
};
use super::entry::WorkspaceEntry;
use super::library::ResolvedLibrary;
use super::lifecycle::{jitter_offset, spawn_load, spawn_peek, LoadRequest, ReloadNotifier};
use super::source::Peek;
use crate::metrics::{
    STROEM_WORKSPACE_LOAD_ADMISSION_SKIPPED_TOTAL, STROEM_WORKSPACE_PEEK_FAILURES_TOTAL,
};
use metrics::counter;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use stroem_common::budget::LoadBudget;
use tokio::sync::Semaphore;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

pub(crate) struct WatcherCtx {
    pub entry: Arc<WorkspaceEntry>,
    pub libs: Arc<HashMap<String, ResolvedLibrary>>,
    pub permits: Arc<Semaphore>,
    pub settings: ReloadSettings,
    pub notifier: Option<Arc<dyn ReloadNotifier>>,
}

pub(crate) async fn run_watcher(ctx: WatcherCtx, cancel: CancellationToken) {
    let poll = ctx.entry.poll_interval();
    let policy = ctx.settings.policy(poll);
    let offset = if ctx.entry.availability().is_errored() {
        Duration::ZERO // startup failure: retry on the first tick (spec § 4.4 startup row)
    } else {
        jitter_offset(&ctx.entry.name, poll)
    };
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + offset, poll);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tracing::info!(
        "Watcher started for workspace '{}' (poll interval: {}s, first check in {:?})",
        ctx.entry.name,
        poll.as_secs(),
        offset
    );
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            () = cancel.cancelled() => {
                tracing::info!("Watcher for workspace '{}' stopping (shutdown)", ctx.entry.name);
                break;
            }
        }
        watcher_tick(&ctx, &policy).await;
    }
}

pub(crate) async fn watcher_tick(ctx: &WatcherCtx, policy: &Policy) {
    let entry = &ctx.entry;
    let a = entry.availability();
    if !a.is_errored() && a.peek_in_flight.is_some() {
        // The previous peek's observer gave up; this tick counts as a failure.
        // (Only while Fresh — an Errored entry doesn't peek, so a stale
        // in-flight record there is not a hung-peek failure.)
        counter!(STROEM_WORKSPACE_PEEK_FAILURES_TOTAL, "workspace" => entry.name.clone())
            .increment(1);
    }
    let effect = match entry.transition(
        Event::Tick {
            now: Instant::now(),
        },
        policy,
    ) {
        Effect::Peek => peek_once(ctx, policy).await,
        other => other,
    };
    match effect {
        Effect::AttemptLoad => attempt_watcher_load(ctx, policy).await,
        Effect::PeekFailureSkipped { first: true } => tracing::warn!(
            "Workspace '{}': could not check for changes; keeping the last loaded config \
             (a forced reload follows after {} consecutive failures)",
            entry.name,
            policy.peek_failure_threshold
        ),
        _ => {}
    }
}

/// Observer side of a peek (spec § 4.5 (7)).
async fn peek_once(ctx: &WatcherCtx, policy: &Policy) -> Effect {
    let entry = &ctx.entry;
    let op_id = entry.next_op_id();
    let started_at = Instant::now();
    let deadline = instant_after(started_at, ctx.settings.peek_timeout);
    entry.transition(
        Event::PeekStarted {
            op_id,
            started_at,
            deadline,
        },
        policy,
    );
    let handle = spawn_peek(
        Arc::clone(entry),
        *policy,
        op_id,
        LoadBudget::until(deadline),
    );
    let event =
        match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), handle).await {
            Ok(Ok(peek)) => {
                if let Peek::Failed(e) = &peek {
                    tracing::debug!("Workspace '{}': peek failed: {:#}", entry.name, e);
                }
                Event::PeekCompleted {
                    observation: observe(&peek, entry.published().revision.as_deref()),
                }
            }
            Ok(Err(join_err)) => {
                tracing::error!(
                    "Workspace '{}': peek finalizer failed: {}",
                    entry.name,
                    join_err
                );
                Event::PeekCompleted {
                    observation: PeekObservation::Failed,
                }
            }
            Err(_elapsed) => Event::PeekTimedOut,
        };
    if matches!(
        event,
        Event::PeekTimedOut
            | Event::PeekCompleted {
                observation: PeekObservation::Failed
            }
    ) {
        counter!(STROEM_WORKSPACE_PEEK_FAILURES_TOTAL, "workspace" => entry.name.clone())
            .increment(1);
    }
    entry.transition(event, policy)
}

fn observe(peek: &Peek, published: Option<&str>) -> PeekObservation {
    match peek {
        Peek::Revision(r) if Some(r.as_str()) == published => PeekObservation::Matches,
        Peek::Revision(_) => PeekObservation::Differs,
        Peek::Unsupported | Peek::LocalInvalid(_) => PeekObservation::NeedsLoad,
        Peek::Failed(_) => PeekObservation::Failed,
    }
}

/// Ordered, non-blocking admission; then observe the load (spec § 4.5 (1)–(2)).
pub(crate) async fn attempt_watcher_load(ctx: &WatcherCtx, policy: &Policy) {
    let entry = &ctx.entry;
    let was_errored = entry.availability().is_errored();
    let Ok(guard) = entry.exec().try_lock_owned() else {
        counter!(STROEM_WORKSPACE_LOAD_ADMISSION_SKIPPED_TOTAL, "workspace" => entry.name.clone(), "reason" => "busy").increment(1);
        return;
    };
    let Ok(permit) = Arc::clone(&ctx.permits).try_acquire_owned() else {
        drop(guard);
        counter!(STROEM_WORKSPACE_LOAD_ADMISSION_SKIPPED_TOTAL, "workspace" => entry.name.clone(), "reason" => "saturated").increment(1);
        return;
    };
    let op_id = entry.next_op_id();
    let started_at = Instant::now();
    let deadline = instant_after(started_at, ctx.settings.load_timeout);
    entry.transition(
        Event::LoadStarted {
            op_id,
            started_at,
            deadline,
        },
        policy,
    );
    tracing::info!("Workspace '{}': reloading", entry.name);

    let handle = spawn_load(LoadRequest {
        entry: Arc::clone(entry),
        libs: Arc::clone(&ctx.libs),
        policy: *policy,
        guard,
        permit: Some(permit),
        caller: Caller::Watcher,
        op_id: Some(op_id),
        budget: LoadBudget::until(deadline),
        notifier: ctx.notifier.clone(),
    });
    match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), handle).await {
        Ok(Ok(Ok(success))) => {
            if was_errored {
                tracing::info!("Workspace '{}' recovered from load error", entry.name);
            } else {
                tracing::info!(
                    "Workspace '{}' reloaded (revision: {:?}, changed: {})",
                    entry.name,
                    entry.published().revision.as_deref().map(|s| &s[..8.min(s.len())]),
                    success.revision_changed
                );
            }
        }
        Ok(Ok(Err(e))) => tracing::warn!("Failed to reload workspace '{}': {:#}", entry.name, e),
        Ok(Err(join_err)) => tracing::error!("Workspace '{}': load finalizer failed: {}", entry.name, join_err),
        Err(_elapsed) => tracing::warn!(
            "Workspace '{}': reload exceeded {:?}; it keeps running and still holds its slot (overdue)",
            entry.name,
            ctx.settings.load_timeout
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::availability::Freshness;
    use crate::workspace::source::WorkspaceSource;
    use crate::workspace::test_support::{config_with, TestLoad, TestPeek, TestSource};

    fn settings() -> ReloadSettings {
        ReloadSettings {
            peek_failure_threshold: 3,
            peek_timeout: Duration::from_millis(100),
            load_timeout: Duration::from_millis(200),
            max_backoff: Duration::from_secs(900),
        }
    }

    fn ctx_with(source: Arc<TestSource>, permits: usize) -> (WatcherCtx, Policy) {
        let entry = Arc::new(WorkspaceEntry::new(
            "ws",
            source,
            config_with("a"),
            Some("rev-a".to_string()),
        ));
        let s = settings();
        let policy = s.policy(entry.poll_interval());
        let ctx = WatcherCtx {
            entry,
            libs: Arc::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(permits)),
            settings: s,
            notifier: None,
        };
        (ctx, policy)
    }

    fn src(load: TestLoad, peek: TestPeek) -> Arc<TestSource> {
        Arc::new(TestSource::new(load, peek))
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_peek_skips_and_keeps_serving() {
        let source = src(
            TestLoad::Ok {
                action: "b",
                revision: "rev-b",
            },
            TestPeek::Failed,
        );
        let (ctx, policy) = ctx_with(source.clone(), 8);
        watcher_tick(&ctx, &policy).await;
        assert_eq!(
            source.load_count(),
            0,
            "a peek failure must not trigger a load"
        );
        assert!(ctx.entry.published().config.actions.contains_key("a"));
        assert_eq!(
            ctx.entry.availability().freshness,
            Freshness::Fresh {
                consecutive_peek_failures: 1
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn k_consecutive_peek_failures_force_exactly_one_load() {
        let source = src(
            TestLoad::Ok {
                action: "a",
                revision: "rev-a",
            },
            TestPeek::Failed,
        );
        let (ctx, policy) = ctx_with(source.clone(), 8);
        for _ in 0..3 {
            watcher_tick(&ctx, &policy).await;
        }
        assert_eq!(source.load_count(), 1);
        assert_eq!(
            ctx.entry.availability().freshness,
            Freshness::Fresh {
                consecutive_peek_failures: 0
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_changed_revision_reloads_and_a_matching_one_does_not() {
        let source = src(
            TestLoad::Ok {
                action: "b",
                revision: "rev-b",
            },
            TestPeek::Revision("rev-a"),
        );
        let (ctx, policy) = ctx_with(source.clone(), 8);
        watcher_tick(&ctx, &policy).await;
        assert_eq!(source.load_count(), 0);
        source.set_peek(TestPeek::Revision("rev-b"));
        watcher_tick(&ctx, &policy).await;
        assert_eq!(source.load_count(), 1);
        assert_eq!(ctx.entry.published().revision.as_deref(), Some("rev-b"));
        // A source that cannot decide locally (e.g. a corrupted clone) also
        // forces an immediate load, same as a genuine revision change.
        source.set_peek(TestPeek::LocalInvalid);
        watcher_tick(&ctx, &policy).await;
        assert_eq!(
            source.load_count(),
            2,
            "LocalInvalid forces a load even though the local peek can't compare revisions"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hung_peek_times_out_blocks_new_peeks_and_escalates() {
        let source = src(
            TestLoad::Ok {
                action: "a",
                revision: "rev-a",
            },
            TestPeek::Hang(Duration::from_millis(600)),
        );
        let (ctx, policy) = ctx_with(source.clone(), 8);
        watcher_tick(&ctx, &policy).await; // times out after 100 ms → failure 1
        assert!(ctx.entry.availability().peek_in_flight.is_some());
        watcher_tick(&ctx, &policy).await; // still in flight → failure 2, no new peek
        watcher_tick(&ctx, &policy).await; // failure 3 = K → forced load
        assert_eq!(source.load_count(), 1);
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(
            ctx.entry.availability().peek_in_flight.is_none(),
            "finalizer clears the record"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn busy_and_saturated_admission_change_nothing() {
        let source = src(
            TestLoad::Ok {
                action: "b",
                revision: "rev-b",
            },
            TestPeek::Revision("rev-b"),
        );
        let (ctx, policy) = ctx_with(source.clone(), 8);
        let held = ctx.entry.exec().lock_owned().await;
        let before = ctx.entry.availability();
        attempt_watcher_load(&ctx, &policy).await;
        assert_eq!(ctx.entry.availability(), before, "Busy is not a transition");
        drop(held);

        let (sat, policy) = ctx_with(source.clone(), 0);
        let before = sat.entry.availability();
        attempt_watcher_load(&sat, &policy).await;
        assert_eq!(
            sat.entry.availability(),
            before,
            "Saturated is not a transition"
        );
        assert!(
            sat.entry.exec().try_lock().is_ok(),
            "Saturated must release the mutex"
        );
        assert_eq!(source.load_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_overdue_load_keeps_its_slot_and_its_late_success_publishes() {
        let source = src(
            TestLoad::Ok {
                action: "b",
                revision: "rev-b",
            },
            TestPeek::Failed,
        );
        source.set_load_sleep(Duration::from_millis(500));
        let (ctx, policy) = ctx_with(source.clone(), 1);
        attempt_watcher_load(&ctx, &policy).await; // observer gives up at 200 ms
        assert!(ctx.entry.availability().load_overdue(Instant::now()));
        assert_eq!(
            ctx.permits.available_permits(),
            0,
            "permit held until real completion"
        );
        assert!(
            ctx.entry.exec().try_lock().is_err(),
            "mutex held until real completion"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(ctx.entry.published().revision.as_deref(), Some("rev-b"));
        assert!(!ctx.entry.availability().load_overdue(Instant::now()));
        assert_eq!(ctx.permits.available_permits(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_late_failure_enters_errored() {
        let source = src(TestLoad::Err("boom"), TestPeek::Failed);
        source.set_load_sleep(Duration::from_millis(400));
        let (ctx, policy) = ctx_with(source, 8);
        attempt_watcher_load(&ctx, &policy).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(ctx.entry.availability().is_errored());
        assert!(ctx.entry.published().error.is_some());
    }

    /// Round-3 regression: the watchdog must fire on a ONE-worker runtime
    /// while the load blocks (it runs on the blocking pool).
    #[test]
    fn the_observer_times_out_on_a_single_worker_runtime() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let source = src(
                TestLoad::Ok {
                    action: "b",
                    revision: "rev-b",
                },
                TestPeek::Failed,
            );
            source.set_load_sleep(Duration::from_secs(2));
            let (ctx, policy) = ctx_with(source, 8);
            let started = Instant::now();
            attempt_watcher_load(&ctx, &policy).await;
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "took {:?}",
                started.elapsed()
            );
        });
        rt.shutdown_timeout(Duration::from_millis(10));
    }

    struct StalledNotifier;
    impl ReloadNotifier for StalledNotifier {
        fn notify(&self, _ws: String) -> futures_util::future::BoxFuture<'static, ()> {
            Box::pin(std::future::pending())
        }
    }

    /// Round-5 major: a stalled notification must not hold the permit or mutex.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stalled_notification_holds_nothing() {
        let source = src(
            TestLoad::Ok {
                action: "b",
                revision: "rev-b",
            },
            TestPeek::Failed,
        );
        let (mut ctx, policy) = ctx_with(source, 1);
        ctx.notifier = Some(Arc::new(StalledNotifier));
        attempt_watcher_load(&ctx, &policy).await;
        settle().await;
        assert_eq!(ctx.entry.published().revision.as_deref(), Some("rev-b"));
        assert_eq!(ctx.permits.available_permits(), 1);
        assert!(ctx.entry.exec().try_lock().is_ok());
    }

    /// Round-2 regression, end to end: an external failure is retried by the watcher.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_external_failure_is_retried_on_the_backoff_ladder() {
        let source = src(
            TestLoad::Err("secret render failed"),
            TestPeek::Revision("rev-a"),
        );
        let (ctx, policy) = ctx_with(source.clone(), 8);
        let _ = ctx.entry.apply_load_result(
            Caller::External,
            None,
            Err(anyhow::anyhow!("secret render failed")),
            &HashMap::new(),
            &policy,
            Instant::now()
                .checked_sub(policy.poll_interval)
                .unwrap_or_else(Instant::now), // next_attempt is now
        );
        source.set_load(TestLoad::Ok {
            action: "a",
            revision: "rev-a",
        });
        watcher_tick(&ctx, &policy).await;
        assert_eq!(
            source.load_count(),
            1,
            "errored workspace must be retried without peeking"
        );
        assert!(!ctx.entry.availability().is_errored());
    }

    /// Spec § 11 E2E substitute: a failing peek keeps serving the last config.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_workspace_whose_peek_keeps_failing_stays_servable() {
        let source = src(TestLoad::Err("fetch failed"), TestPeek::Failed);
        let (ctx, policy) = ctx_with(source, 8);
        for _ in 0..2 {
            watcher_tick(&ctx, &policy).await;
        }
        assert!(
            ctx.entry.is_healthy(),
            "config must still be served below K"
        );
        assert!(ctx.entry.published().config.actions.contains_key("a"));
    }

    /// Fix round 1, Important finding: a workspace that failed at startup
    /// must retry on its FIRST tick, without waiting out the per-workspace
    /// jitter offset (spec § 4.4 startup row). Uses a huge poll interval so
    /// jitter, if applied, would otherwise delay the first tick by up to an
    /// hour.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_errored_workspace_is_retried_on_the_first_tick_without_jitter() {
        let mut source = TestSource::new(
            TestLoad::Ok {
                action: "a",
                revision: "rev-a",
            },
            TestPeek::Failed,
        );
        source.poll_secs = 3600;
        let source = Arc::new(source);
        let s = settings();
        let poll = Duration::from_secs(source.poll_interval_secs().max(1));
        let policy = s.policy(poll);
        let entry = WorkspaceEntry::pending("ws".to_string(), source.clone());
        let _ = entry.apply_load_result(
            Caller::Startup,
            None,
            Err(anyhow::anyhow!("boom")),
            &HashMap::new(),
            &policy,
            Instant::now(),
        );
        let entry = Arc::new(entry);
        let ctx = WatcherCtx {
            entry: entry.clone(),
            libs: Arc::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(8)),
            settings: s,
            notifier: None,
        };
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_watcher(ctx, cancel.clone()));

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if source.load_count() >= 1 && entry.is_healthy() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("an errored entry must retry on the first tick, not after a jittered delay");

        cancel.cancel();
        let _ = handle.await;
    }
}
