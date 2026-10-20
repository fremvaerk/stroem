//! Leader election via Postgres advisory locks.
//!
//! At most one server replica holds the leader role at any time. The leader is
//! the only replica that runs the scheduler, event-source manager, and recovery
//! sweeper background tasks. Followers serve API/WebSocket traffic only.
//!
//! The lock is `pg_try_advisory_lock(LEADER_LOCK_KEY)` held on a **dedicated**
//! Postgres connection (outside the shared pool). Postgres releases the lock
//! when that session ends. Three things bound how long a leader that stopped
//! working keeps it (spec `docs/superpowers/specs/2026-10-20-leader-stale-lock-design.md`):
//!
//! - **Lease**: the session carries `idle_session_timeout` ([`LeaderTiming::lease`]).
//!   The leader pings far more often, so PostgreSQL ends only a session whose
//!   process stopped talking — even when the node's kernel keeps the TCP
//!   session alive (prod 2026-10-19: 16.5 h without a leader). From the lock
//!   query on, the session sends only simple-protocol queries (`raw_sql`): an
//!   extended-protocol exchange cut before its Sync would leave the session
//!   waiting with no idle timer armed.
//! - **Validity**: [`LeaderElection::is_leader`] is true only while the last
//!   successful ping was sent less than [`LeaderTiming::validity`] ago, so a
//!   process that thaws after losing its session does not act on a stale flag.
//! - **Self-release**: when a leader-gated loop has been `Stalled`, or ran
//!   and then exited (the `/livez` rules), for [`LeaderTiming::release_after`],
//!   the leader drops the lock and does not try again while that lasts.
//!
//! The timing comes from the `leader:` server config
//! ([`LeaderTiming::from_lease`]).
//!
//! For tests and single-replica deployments, [`LeaderElection::always`] returns
//! a leader that always reports `true` without contacting the database.

use crate::state::monotonic_ms;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgConnectOptions, PgRow};
use sqlx::{Connection, PgConnection, Row};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Postgres advisory lock key for the leader role.
///
/// Hex `0x5354524D_4C44_5201` = ASCII "STRMLDR" + version byte `0x01`.
/// Bumping the version byte guarantees a clean leader handoff during rolling
/// upgrades that change the leader contract (e.g. additional gated tasks).
///
/// **Operational requirement:** use a dedicated Postgres role for Strøm so
/// no co-tenant application can hold this key and starve our leader. The
/// advisory-lock namespace is global per database.
pub const LEADER_LOCK_KEY: i64 = 0x5354_524D_4C44_5201;

/// The lock query. A constant literal so it can go out as ONE simple-protocol
/// `Query` message (see the module docs); a test checks it names
/// [`LEADER_LOCK_KEY`].
const LOCK_SQL: &str = "SELECT pg_try_advisory_lock(x'5354524D4C445201'::bigint), pg_backend_pid()";

/// The leader's ping, also sent with the simple protocol.
const PING_SQL: &str = "SELECT 1";

/// The parameter that enforces the lease (PostgreSQL ≥ 14).
const LEASE_PARAMETER: &str = "idle_session_timeout";

/// `application_name` of every leader-election connection.
pub const APPLICATION_NAME: &str = "stroem-leader";

/// SQLSTATE `undefined_object`: "unrecognized configuration parameter".
const SQLSTATE_UNDEFINED_OBJECT: &str = "42704";

/// SQLSTATE `idle_session_timeout`: PostgreSQL ended an idle session.
const SQLSTATE_IDLE_SESSION_TIMEOUT: &str = "57P05";

/// Bound on opening the dedicated connection, as before the lease existed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a follower reads who holds the lock.
const HOLDER_REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// How often an unchanged, healthy holder is logged again at INFO.
const HOLDER_REPEAT_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Timing of the election loop. Production derives it from the `leader:`
/// config with [`LeaderTiming::from_lease`]; tests pass shorter values to
/// [`LeaderElection::start_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaderTiming {
    /// Pause between loop iterations: a ping when leader, an acquire attempt
    /// when follower.
    pub retry_interval: Duration,
    /// Bound on one ping. A ping that takes longer drops the connection.
    pub ping_timeout: Duration,
    /// Bound on one whole acquire attempt (connect, lease, lock, holder report).
    pub acquire_timeout: Duration,
    /// `is_leader()` is true only while the last successful ping (or the
    /// acquire) was sent less than this long ago.
    pub validity: Duration,
    /// `idle_session_timeout` of the leader session.
    pub lease: Duration,
    /// How long a leader-gated loop must stay unfit before the leader
    /// releases the lock. `None`: self-release off — the fitness probe is
    /// not called.
    pub release_after: Option<Duration>,
}

impl Default for LeaderTiming {
    fn default() -> Self {
        Self::from_lease(Duration::from_secs(30), Some(Duration::from_secs(30)))
    }
}

/// Caps of the values derived from the lease (spec § 4.1): they detect a
/// broken connection and pace the follower, independent of the lease.
const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const MAX_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

impl LeaderTiming {
    /// Derive the timing from the lease (spec § 4.1), in whole milliseconds:
    /// retry and ping timeout `min(lease/6, 5 s)`, acquire timeout
    /// `min(lease/3, 10 s)`, validity `2 × lease/3`.
    pub fn from_lease(lease: Duration, release_after: Option<Duration>) -> Self {
        let ms = u64::try_from(lease.as_millis()).unwrap_or(u64::MAX);
        let sixth = Duration::from_millis(ms / 6).min(MAX_RETRY_INTERVAL);
        Self {
            retry_interval: sixth,
            ping_timeout: sixth,
            acquire_timeout: Duration::from_millis(ms / 3).min(MAX_ACQUIRE_TIMEOUT),
            validity: Duration::from_millis(ms.saturating_mul(2) / 3),
            lease,
            release_after,
        }
    }

    /// The invariants of spec § 4.1.
    pub fn check(&self) -> Result<(), String> {
        if 2 * self.ping_timeout + self.retry_interval >= self.validity {
            return Err(format!(
                "2 × ping_timeout + retry_interval ({:?}) must be below validity ({:?}): \
                 a leader whose pings succeed would lose validity between two of them",
                2 * self.ping_timeout + self.retry_interval,
                self.validity
            ));
        }
        if self.validity >= self.lease {
            return Err(format!(
                "validity ({:?}) must be below lease ({:?})",
                self.validity, self.lease
            ));
        }
        if self.lease < 3 * self.retry_interval {
            return Err(format!(
                "lease ({:?}) must be at least 3 × retry_interval ({:?})",
                self.lease,
                3 * self.retry_interval
            ));
        }
        Ok(())
    }
}

/// Why a leader-gated loop does no leader work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopFault {
    /// Alive, but it has not come round for longer than its threshold.
    Stalled,
    /// It ran and then exited (a panic: nothing respawns it).
    Stopped,
}

/// A leader-gated loop that cannot do leader work, as the fitness probe
/// reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnfitLoop {
    pub name: &'static str,
    pub fault: LoopFault,
}

impl std::fmt::Display for UnfitLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self.fault {
            LoopFault::Stalled => "is stalled",
            LoopFault::Stopped => "has stopped",
        };
        write!(f, "the {} loop {}", self.name, state)
    }
}

/// Returns a leader-gated loop that is unfit, if any. Installed by `main`
/// with [`LeaderElection::set_fitness_probe`].
pub type FitnessProbe = Arc<dyn Fn() -> Option<UnfitLoop> + Send + Sync>;

/// Leader election handle shared across background tasks.
///
/// All gated background tasks call [`LeaderElection::is_leader`] each tick to
/// decide whether to do work. The state is updated by the loop spawned in
/// [`LeaderElection::start`].
pub struct LeaderElection {
    /// `is_leader()` holds while `monotonic_ms()` is below this.
    /// `i64::MIN` = not leader, `i64::MAX` = [`LeaderElection::always`].
    valid_until_ms: AtomicI64,
    /// Test hook: the loop skips its iterations (see [`Self::pause_for_test`]).
    paused: AtomicBool,
    fitness: OnceLock<FitnessProbe>,
}

impl LeaderElection {
    fn with_valid_until(valid_until_ms: i64) -> Arc<Self> {
        Arc::new(Self {
            valid_until_ms: AtomicI64::new(valid_until_ms),
            paused: AtomicBool::new(false),
            fitness: OnceLock::new(),
        })
    }

    /// Returns a leader handle that always reports `true`.
    ///
    /// Use for unit tests, single-replica deployments, or any context where the
    /// HA coordinator isn't running.
    pub fn always() -> Arc<Self> {
        Self::with_valid_until(i64::MAX)
    }

    /// A handle that always reports `false` (tests of replica-local work
    /// that must also run on followers).
    pub fn never() -> Arc<Self> {
        Self::with_valid_until(i64::MIN)
    }

    /// Spawn the leader-election loop with the default timing. Returns a
    /// shared handle plus the task `JoinHandle` so the caller can await
    /// shutdown.
    #[tracing::instrument(skip(db_url, cancel))]
    pub fn start(db_url: String, cancel: CancellationToken) -> (Arc<Self>, JoinHandle<()>) {
        Self::start_with(db_url, LeaderTiming::default(), cancel)
    }

    /// [`Self::start`] with explicit timing (the `leader:` config, or tests).
    #[tracing::instrument(skip(db_url, cancel))]
    pub fn start_with(
        db_url: String,
        timing: LeaderTiming,
        cancel: CancellationToken,
    ) -> (Arc<Self>, JoinHandle<()>) {
        debug_assert!(timing.check().is_ok(), "{:?}", timing.check());
        let leader = Self::never();
        let handle = tokio::spawn(run_loop(db_url, leader.clone(), timing, cancel));
        (leader, handle)
    }

    /// Returns `true` while this replica holds the leader lock AND its last
    /// successful ping was sent less than [`LeaderTiming::validity`] ago.
    #[inline]
    pub fn is_leader(&self) -> bool {
        monotonic_ms() < self.valid_until_ms.load(Ordering::Relaxed)
    }

    /// Install the probe that tells the loop a leader-gated loop is unfit.
    /// Only the first call takes effect. Not called when
    /// [`LeaderTiming::release_after`] is `None`.
    pub fn set_fitness_probe(&self, probe: FitnessProbe) {
        if self.fitness.set(probe).is_err() {
            tracing::warn!("Leader fitness probe already installed; ignoring the new one");
        }
    }

    /// Make the election loop skip its iterations — no ping, no acquire —
    /// which is what PostgreSQL sees from a frozen process. An iteration in
    /// progress completes. For tests.
    #[doc(hidden)]
    pub fn pause_for_test(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    fn renew(&self, sent_ms: i64, validity: Duration) {
        let validity_ms = i64::try_from(validity.as_millis()).unwrap_or(i64::MAX);
        self.valid_until_ms
            .store(sent_ms.saturating_add(validity_ms), Ordering::Relaxed);
    }

    fn clear(&self) {
        self.valid_until_ms.store(i64::MIN, Ordering::Relaxed);
    }
}

/// What one loop iteration does (spec § 4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Drop the lock: a gated loop has been unfit for `release_after`.
    Release,
    Ping,
    /// Follower with an unfit gated loop: do not try to become leader.
    StayFollower,
    Acquire,
}

fn next_action(holding: bool, unfit_for: Option<Duration>, release_after: Duration) -> Action {
    match (holding, unfit_for) {
        (true, Some(d)) if d >= release_after => Action::Release,
        (true, _) => Action::Ping,
        (false, Some(_)) => Action::StayFollower,
        (false, None) => Action::Acquire,
    }
}

/// Whether the lease could be set on the leader session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseStatus {
    Enforced,
    /// The server does not know the parameter (PostgreSQL < 14).
    Unsupported,
}

/// `idle_session_timeout` value for `lease`, in milliseconds.
fn lease_setting(lease: Duration) -> String {
    format!("{}ms", lease.as_millis())
}

fn sqlstate(err: &sqlx::Error) -> Option<String> {
    err.as_database_error()
        .and_then(|e| e.code())
        .map(|c| c.into_owned())
}

/// Set `parameter` (normally `idle_session_timeout`) to `lease` for the rest
/// of the session. An unknown parameter is [`LeaseStatus::Unsupported`]; any
/// other error is returned. Uses the extended protocol: the lock is not held
/// yet. `pub` for the integration tests, which pass an unknown parameter.
#[doc(hidden)]
#[tracing::instrument(skip(conn))]
pub async fn apply_lease(
    conn: &mut PgConnection,
    parameter: &str,
    lease: Duration,
) -> Result<LeaseStatus> {
    let result = sqlx::query("SELECT set_config($1, $2, false)")
        .persistent(false)
        .bind(parameter)
        .bind(lease_setting(lease))
        .execute(&mut *conn)
        .await;
    match result {
        Ok(_) => Ok(LeaseStatus::Enforced),
        Err(e) if sqlstate(&e).as_deref() == Some(SQLSTATE_UNDEFINED_OBJECT) => {
            Ok(LeaseStatus::Unsupported)
        }
        Err(e) => Err(e).context("set the leader lease"),
    }
}

/// The session holding the leader lock in the current database, as seen in
/// `pg_stat_activity`.
#[derive(Debug, Clone, PartialEq)]
pub struct LockHolder {
    pub pid: i32,
    /// `None` for a local socket, or when the row is hidden from this role.
    pub client_addr: Option<String>,
    pub application_name: Option<String>,
    pub backend_start: Option<DateTime<Utc>>,
    pub state: Option<String>,
    /// Seconds since `state` last changed.
    pub state_age_secs: Option<f64>,
}

/// Read who holds [`LEADER_LOCK_KEY`]; `None` when nobody does.
#[tracing::instrument(skip(conn))]
pub async fn lock_holder(conn: &mut PgConnection) -> Result<Option<LockHolder>> {
    // A bigint advisory key is split into classid (high 32 bits) and objid
    // (low 32 bits), objsubid 1. Advisory locks are per database.
    let high = (LEADER_LOCK_KEY >> 32) & 0xFFFF_FFFF;
    let low = LEADER_LOCK_KEY & 0xFFFF_FFFF;
    let row: Option<PgRow> = sqlx::query(
        "SELECT a.pid, host(a.client_addr) AS client_addr, a.application_name, \
                a.backend_start, a.state, \
                EXTRACT(EPOCH FROM now() - a.state_change)::float8 AS state_age_secs \
         FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid \
         WHERE l.locktype = 'advisory' AND l.granted \
           AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
           AND l.classid = $1::bigint::oid AND l.objid = $2::bigint::oid \
           AND l.objsubid = 1",
    )
    .persistent(false)
    .bind(high)
    .bind(low)
    .fetch_optional(&mut *conn)
    .await
    .context("read the leader lock holder")?;
    row.map(|r| {
        Ok(LockHolder {
            pid: r.try_get("pid")?,
            client_addr: r.try_get("client_addr")?,
            application_name: r.try_get("application_name")?,
            backend_start: r.try_get("backend_start")?,
            state: r.try_get("state")?,
            state_age_secs: r.try_get("state_age_secs")?,
        })
    })
    .transpose()
}

/// How a follower logs the holder it just read (spec § 4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HolderReport {
    /// Stale or foreign holder.
    Warn,
    /// New holder, or the periodic repeat.
    Info,
    Quiet,
}

fn holder_report(
    holder: &LockHolder,
    lease: Duration,
    changed: bool,
    since_last_info: Option<Duration>,
) -> HolderReport {
    let foreign = holder.application_name.as_deref() != Some(APPLICATION_NAME);
    let stale = holder.state.as_deref() == Some("idle")
        && holder
            .state_age_secs
            .is_some_and(|age| age > lease.as_secs_f64());
    if foreign || stale {
        HolderReport::Warn
    } else if changed || since_last_info.is_none_or(|d| d >= HOLDER_REPEAT_INTERVAL) {
        HolderReport::Info
    } else {
        HolderReport::Quiet
    }
}

fn describe_holder(h: &LockHolder) -> String {
    fn or_unknown(v: Option<&str>) -> &str {
        v.unwrap_or("unknown")
    }
    format!(
        "pid {} (client {}, application {}, backend_start {}, state {}, idle {})",
        h.pid,
        or_unknown(h.client_addr.as_deref()),
        or_unknown(h.application_name.as_deref()),
        h.backend_start
            .map_or_else(|| "unknown".to_string(), |t| t.to_rfc3339()),
        or_unknown(h.state.as_deref()),
        h.state_age_secs
            .map_or_else(|| "unknown".to_string(), |s| format!("{s:.0}s")),
    )
}

/// The result of one acquire attempt.
enum Attempt {
    Acquired {
        conn: PgConnection,
        /// `monotonic_ms()` just before the lock query was sent.
        sent_ms: i64,
        backend_pid: i32,
        lease: LeaseStatus,
    },
    /// Another session holds the lock. `holder` when it was asked for and read.
    Held { holder: Option<LockHolder> },
}

/// Open a fresh connection, set the lease and try to take the lock.
///
/// A connection that did not get the lock is dropped before returning. On
/// any error the connection is dropped too, so no lock is left behind.
async fn try_acquire(db_url: &str, lease: Duration, want_holder: bool) -> Result<Attempt> {
    let opts = PgConnectOptions::from_str(db_url)
        .context("parse leader DB URL")?
        .application_name(APPLICATION_NAME);
    let mut conn = tokio::time::timeout(CONNECT_TIMEOUT, PgConnection::connect_with(&opts))
        .await
        .map_err(|_| anyhow::anyhow!("leader connection timed out ({CONNECT_TIMEOUT:?})"))?
        .context("connect dedicated leader connection")?;

    // Before the lock: the session never holds it without the lease.
    let lease = apply_lease(&mut conn, LEASE_PARAMETER, lease).await?;

    let sent_ms = monotonic_ms();
    let row = sqlx::raw_sql(LOCK_SQL)
        .fetch_one(&mut conn)
        .await
        .context("pg_try_advisory_lock")?;
    let acquired: bool = row.try_get(0).context("decode lock result")?;
    if acquired {
        let backend_pid: i32 = row.try_get(1).context("decode backend pid")?;
        return Ok(Attempt::Acquired {
            conn,
            sent_ms,
            backend_pid,
            lease,
        });
    }
    let holder = if want_holder {
        match lock_holder(&mut conn).await {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!("Could not read the leader lock holder: {:#}", e);
                None
            }
        }
    } else {
        None
    };
    Ok(Attempt::Held { holder })
}

/// Identifies one holder session: `(pid, backend_start)`.
type HolderKey = (i32, Option<DateTime<Utc>>);

/// Per-loop state that is not shared with readers of [`LeaderElection`].
#[derive(Default)]
struct LoopState {
    /// The lock-holding connection while leader.
    conn: Option<PgConnection>,
    /// The PostgreSQL < 14 warning was logged.
    lease_warned: bool,
    /// The unfit loop and when this unfit stretch was first seen.
    unfit: Option<(UnfitLoop, Instant)>,
    /// When a follower last asked who holds the lock.
    holder_asked_at: Option<Instant>,
    /// The holder last logged at INFO: `(pid, backend_start)` and when.
    holder_logged: Option<(HolderKey, Instant)>,
}

async fn run_loop(
    db_url: String,
    leader: Arc<LeaderElection>,
    timing: LeaderTiming,
    cancel: CancellationToken,
) {
    tracing::info!(
        "LeaderElection started (lease={:?}, validity={:?}, retry={:?}, ping_timeout={:?}, \
         acquire_timeout={:?}, {})",
        timing.lease,
        timing.validity,
        timing.retry_interval,
        timing.ping_timeout,
        timing.acquire_timeout,
        timing.release_after.map_or_else(
            || "self-release off".to_string(),
            |d| format!("self-release after {d:?}")
        )
    );
    let mut st = LoopState::default();

    loop {
        // At shutdown the gated loops exit, which reads as `Stopped`: no new
        // iteration once `cancel` has fired (spec § 4.4, "Shutdown").
        if !leader.paused.load(Ordering::Relaxed) && !cancel.is_cancelled() {
            iterate(&db_url, &leader, &timing, &mut st).await;
        }

        tokio::select! {
            _ = tokio::time::sleep(timing.retry_interval) => {}
            _ = cancel.cancelled() => {
                tracing::info!("LeaderElection shutting down");
                leader.clear();
                // Dropping `st.conn` closes the session and releases the
                // advisory lock so the next replica can take over immediately.
                return;
            }
        }
    }
}

/// The probe's answer, or `None` without calling it when self-release is
/// off (`release_after = None`).
fn observe_fitness(
    probe: Option<&FitnessProbe>,
    release_after: Option<Duration>,
) -> Option<UnfitLoop> {
    release_after?;
    probe.and_then(|probe| probe())
}

async fn iterate(db_url: &str, leader: &LeaderElection, timing: &LeaderTiming, st: &mut LoopState) {
    track_fitness(
        observe_fitness(leader.fitness.get(), timing.release_after),
        st,
    );
    let unfit_for = st.unfit.map(|(_, since)| since.elapsed());
    // With self-release off nothing is unfit, so the value is never compared.
    let release_after = timing.release_after.unwrap_or(Duration::MAX);

    match next_action(st.conn.is_some(), unfit_for, release_after) {
        Action::Release => {
            let (unfit, since) = st.unfit.expect("Release implies an unfit loop");
            leader.clear();
            st.conn = None;
            tracing::warn!(
                "Releasing leader lock: {} ({}s); not taking the lock again while that lasts",
                unfit,
                since.elapsed().as_secs()
            );
        }
        Action::Ping => ping(leader, timing, st).await,
        Action::StayFollower => {}
        Action::Acquire => acquire(db_url, leader, timing, st).await,
    }
}

/// Log the transitions of the fitness probe and remember since when the
/// replica has been unfit. A new loop or fault within one unfit stretch keeps
/// the stretch's start: the replica stayed unfit.
fn track_fitness(unfit: Option<UnfitLoop>, st: &mut LoopState) {
    match (unfit, st.unfit) {
        (Some(now), None) => {
            if st.conn.is_some() {
                tracing::warn!(
                    "Leader fitness: {}; this leader will release the lock if that lasts",
                    now
                );
            } else {
                tracing::warn!("Not trying to become leader: {}", now);
            }
            st.unfit = Some((now, Instant::now()));
        }
        (Some(now), Some((before, since))) if now != before => {
            tracing::warn!("Leader fitness: {} (before: {})", now, before);
            st.unfit = Some((now, since));
        }
        (None, Some((before, _))) => {
            tracing::info!(
                "The {} loop makes progress again; eligible for leadership",
                before.name
            );
            st.unfit = None;
        }
        _ => {}
    }
}

async fn ping(leader: &LeaderElection, timing: &LeaderTiming, st: &mut LoopState) {
    let conn = st.conn.as_mut().expect("Ping implies a held connection");
    let sent_ms = monotonic_ms();
    let outcome = tokio::time::timeout(
        timing.ping_timeout,
        sqlx::raw_sql(PING_SQL).execute(&mut *conn),
    )
    .await;
    match outcome {
        Ok(Ok(_)) => leader.renew(sent_ms, timing.validity),
        Ok(Err(e)) => {
            if sqlstate(&e).as_deref() == Some(SQLSTATE_IDLE_SESSION_TIMEOUT) {
                tracing::warn!(
                    "Leader lease expired: PostgreSQL ended the leader session after {:?} \
                     without a ping (idle_session_timeout). Now a follower.",
                    timing.lease
                );
            } else {
                tracing::warn!("Leader connection lost: {:#}. Releasing leader role.", e);
            }
            leader.clear();
            st.conn = None;
        }
        Err(_) => {
            tracing::warn!(
                "Leader ping timed out after {:?}; dropping the leader connection",
                timing.ping_timeout
            );
            leader.clear();
            st.conn = None;
        }
    }
}

async fn acquire(db_url: &str, leader: &LeaderElection, timing: &LeaderTiming, st: &mut LoopState) {
    let want_holder = st
        .holder_asked_at
        .is_none_or(|at| at.elapsed() >= HOLDER_REPORT_INTERVAL);
    if want_holder {
        st.holder_asked_at = Some(Instant::now());
    }
    let attempt = tokio::time::timeout(
        timing.acquire_timeout,
        try_acquire(db_url, timing.lease, want_holder),
    )
    .await;
    match attempt {
        Err(_) => tracing::warn!(
            "Leader acquire attempt timed out after {:?}",
            timing.acquire_timeout
        ),
        Ok(Err(e)) => tracing::warn!("Leader acquire failed: {:#}", e),
        Ok(Ok(Attempt::Acquired {
            conn,
            sent_ms,
            backend_pid,
            lease,
        })) => {
            leader.renew(sent_ms, timing.validity);
            st.conn = Some(conn);
            st.holder_logged = None;
            match lease {
                LeaseStatus::Enforced => tracing::info!(
                    "Acquired leader lock (key={:#x}, backend pid={}, lease={:?})",
                    LEADER_LOCK_KEY,
                    backend_pid,
                    timing.lease
                ),
                LeaseStatus::Unsupported => {
                    tracing::info!(
                        "Acquired leader lock (key={:#x}, backend pid={}, lease not enforced)",
                        LEADER_LOCK_KEY,
                        backend_pid
                    );
                    if !st.lease_warned {
                        st.lease_warned = true;
                        tracing::warn!(
                            "Leader lease not enforced: the database does not support \
                             idle_session_timeout (PostgreSQL < 14). A frozen leader keeps \
                             the leader lock until its TCP session dies."
                        );
                    }
                }
            }
        }
        Ok(Ok(Attempt::Held { holder })) => {
            tracing::debug!("Leader lock held by another replica; staying follower");
            if let Some(holder) = holder {
                log_holder(&holder, timing.lease, st);
            }
        }
    }
}

fn log_holder(holder: &LockHolder, lease: Duration, st: &mut LoopState) {
    let key = (holder.pid, holder.backend_start);
    let changed = st.holder_logged.is_none_or(|(k, _)| k != key);
    let since_last_info = st.holder_logged.map(|(_, at)| at.elapsed());
    match holder_report(holder, lease, changed, since_last_info) {
        HolderReport::Warn => tracing::warn!(
            "Leader lock held by {} — the holder looks stale (idle longer than the {:?} lease, \
             or not a Strøm leader)",
            describe_holder(holder),
            lease
        ),
        HolderReport::Info => {
            tracing::info!("Leader lock held by {}", describe_holder(holder));
            st.holder_logged = Some((key, Instant::now()));
        }
        HolderReport::Quiet => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_leader_reports_true() {
        let leader = LeaderElection::always();
        assert!(leader.is_leader());
    }

    #[test]
    fn never_leader_reports_false() {
        assert!(!LeaderElection::never().is_leader());
    }

    #[test]
    fn leader_lock_key_is_strmldr_v1() {
        // Sanity check: documenting the key encoding so a future refactor
        // can't silently change it (which would split-brain across versions
        // during a rolling upgrade).
        let bytes = LEADER_LOCK_KEY.to_be_bytes();
        assert_eq!(&bytes[..7], b"STRMLDR");
        assert_eq!(bytes[7], 0x01);
    }

    /// The lock query is a literal (simple protocol, no bind parameter); it
    /// must name the same key as the constant.
    #[test]
    fn lock_sql_names_the_leader_lock_key() {
        let literal = format!("x'{:016X}'::bigint", LEADER_LOCK_KEY);
        assert!(LOCK_SQL.contains(&literal), "{LOCK_SQL} lacks {literal}");
    }

    #[test]
    fn default_timing_satisfies_the_invariants() {
        let t = LeaderTiming::default();
        assert_eq!(t.check(), Ok(()));
        assert_eq!(t.lease, Duration::from_secs(30));
        assert_eq!(t.validity, Duration::from_secs(20));
        // The cushion between a frozen leader's last valid moment and the
        // earliest end of its idle session (spec § 5.2).
        assert_eq!(t.lease - t.validity, Duration::from_secs(10));
    }

    /// The defaults are the fixed values of spec revision 5.
    #[test]
    fn default_timing_is_the_lease_of_30s() {
        assert_eq!(
            LeaderTiming::default(),
            LeaderTiming {
                retry_interval: Duration::from_secs(5),
                ping_timeout: Duration::from_secs(5),
                acquire_timeout: Duration::from_secs(10),
                validity: Duration::from_secs(20),
                lease: Duration::from_secs(30),
                release_after: Some(Duration::from_secs(30)),
            }
        );
        assert_eq!(
            LeaderTiming::from_lease(Duration::from_secs(30), Some(Duration::from_secs(30))),
            LeaderTiming::default()
        );
    }

    /// Every lease `leader.lease_secs` accepts (spec § 4.1).
    #[test]
    fn from_lease_satisfies_the_invariants_over_the_allowed_range() {
        for secs in 10..=3600 {
            let t = LeaderTiming::from_lease(Duration::from_secs(secs), None);
            assert_eq!(t.check(), Ok(()), "lease {secs}s: {t:?}");
        }
    }

    #[test]
    fn from_lease_scales_below_30s_and_caps_above() {
        let ms = Duration::from_millis;
        let short = LeaderTiming::from_lease(Duration::from_secs(10), None);
        assert_eq!(short.retry_interval, ms(1666));
        assert_eq!(short.ping_timeout, ms(1666));
        assert_eq!(short.acquire_timeout, ms(3333));
        assert_eq!(short.validity, ms(6666));
        assert_eq!(short.lease, Duration::from_secs(10));
        assert_eq!(short.release_after, None);
        // Validity is floor(2 × L / 3), not 2 × floor(L / 3).
        assert_eq!(
            LeaderTiming::from_lease(Duration::from_secs(11), None).validity,
            ms(7333)
        );

        let long = LeaderTiming::from_lease(Duration::from_secs(3600), Some(ms(60_000)));
        // The caps (spec § 4.1): only the validity scales past 30 s.
        assert_eq!(long.retry_interval, Duration::from_secs(5));
        assert_eq!(long.ping_timeout, Duration::from_secs(5));
        assert_eq!(long.acquire_timeout, Duration::from_secs(10));
        assert_eq!(long.validity, Duration::from_secs(2400));
        assert_eq!(long.lease, Duration::from_secs(3600));
        assert_eq!(long.release_after, Some(ms(60_000)));
    }

    #[test]
    fn timing_check_rejects_each_broken_invariant() {
        let ok = LeaderTiming::default();
        let slow_pings = LeaderTiming {
            ping_timeout: Duration::from_secs(8),
            ..ok
        };
        assert!(slow_pings.check().is_err());
        let validity_past_lease = LeaderTiming {
            validity: Duration::from_secs(30),
            ..ok
        };
        assert!(validity_past_lease.check().is_err());
        let short_lease = LeaderTiming {
            retry_interval: Duration::from_millis(100),
            ping_timeout: Duration::from_millis(100),
            validity: Duration::from_millis(500),
            lease: Duration::from_millis(250),
            ..ok
        };
        assert!(short_lease.check().is_err());
    }

    /// The integration tests' timings (ha_test.rs `fast` and
    /// `long_validity`) must pass the same check as production, or
    /// `start_with`'s debug assertion fires.
    #[test]
    fn integration_test_timing_satisfies_the_invariants() {
        let fast = LeaderTiming {
            retry_interval: Duration::from_millis(200),
            ping_timeout: Duration::from_millis(500),
            acquire_timeout: Duration::from_millis(1500),
            validity: Duration::from_millis(1500),
            lease: Duration::from_millis(2500),
            release_after: Some(Duration::from_millis(600)),
        };
        assert_eq!(fast.check(), Ok(()));
        let long_validity = LeaderTiming {
            validity: Duration::from_secs(4),
            lease: Duration::from_secs(5),
            ..fast
        };
        assert_eq!(long_validity.check(), Ok(()));
    }

    #[test]
    fn validity_lapses_at_the_deadline() {
        let leader = LeaderElection::never();
        let now = monotonic_ms();
        leader.renew(now, Duration::from_secs(20));
        assert!(leader.is_leader());
        // A ping sent 20 s ago: its validity has just ended.
        leader.renew(now - 20_000, Duration::from_secs(20));
        assert!(!leader.is_leader());
        leader.renew(now - 19_000, Duration::from_secs(20));
        assert!(leader.is_leader());
        leader.clear();
        assert!(!leader.is_leader());
    }

    #[test]
    fn next_action_follows_the_spec_table() {
        let after = Duration::from_secs(30);
        let short = Some(Duration::from_secs(29));
        let long = Some(Duration::from_secs(30));
        assert_eq!(next_action(true, None, after), Action::Ping);
        assert_eq!(next_action(true, short, after), Action::Ping);
        assert_eq!(next_action(true, long, after), Action::Release);
        assert_eq!(next_action(false, None, after), Action::Acquire);
        assert_eq!(next_action(false, short, after), Action::StayFollower);
        assert_eq!(next_action(false, long, after), Action::StayFollower);
    }

    #[test]
    fn lease_setting_is_in_milliseconds() {
        assert_eq!(lease_setting(Duration::from_secs(30)), "30000ms");
        assert_eq!(lease_setting(Duration::from_millis(2500)), "2500ms");
    }

    fn holder(app: &str, state: &str, age: f64) -> LockHolder {
        LockHolder {
            pid: 42,
            client_addr: Some("10.0.0.1".into()),
            application_name: Some(app.into()),
            backend_start: None,
            state: Some(state.into()),
            state_age_secs: Some(age),
        }
    }

    #[test]
    fn holder_report_warns_on_stale_or_foreign_holder() {
        let lease = Duration::from_secs(30);
        let fresh = Some(Duration::from_secs(1));
        assert_eq!(
            holder_report(&holder(APPLICATION_NAME, "idle", 31.0), lease, false, fresh),
            HolderReport::Warn
        );
        assert_eq!(
            holder_report(&holder("psql", "idle", 1.0), lease, false, fresh),
            HolderReport::Warn
        );
        // Active for a long time is a running statement, not an idle session.
        assert_eq!(
            holder_report(
                &holder(APPLICATION_NAME, "active", 31.0),
                lease,
                false,
                fresh
            ),
            HolderReport::Quiet
        );
    }

    #[test]
    fn holder_report_logs_changes_and_repeats() {
        let lease = Duration::from_secs(30);
        let h = holder(APPLICATION_NAME, "idle", 2.0);
        assert_eq!(holder_report(&h, lease, true, None), HolderReport::Info);
        assert_eq!(
            holder_report(&h, lease, true, Some(Duration::from_secs(1))),
            HolderReport::Info
        );
        assert_eq!(
            holder_report(&h, lease, false, Some(Duration::from_secs(60))),
            HolderReport::Quiet
        );
        assert_eq!(
            holder_report(&h, lease, false, Some(HOLDER_REPEAT_INTERVAL)),
            HolderReport::Info
        );
    }

    #[test]
    fn describe_holder_marks_hidden_fields_unknown() {
        let h = LockHolder {
            pid: 7,
            client_addr: None,
            application_name: None,
            backend_start: None,
            state: None,
            state_age_secs: None,
        };
        assert_eq!(
            describe_holder(&h),
            "pid 7 (client unknown, application unknown, backend_start unknown, \
             state unknown, idle unknown)"
        );
    }

    #[test]
    fn fitness_transitions_record_the_first_unfit_sighting() {
        let stalled = UnfitLoop {
            name: "scheduler",
            fault: LoopFault::Stalled,
        };
        let stopped = UnfitLoop {
            name: "scheduler",
            fault: LoopFault::Stopped,
        };
        let mut st = LoopState::default();
        track_fitness(Some(stalled), &mut st);
        let first = st.unfit.expect("unfit loop recorded").1;
        track_fitness(Some(stalled), &mut st);
        assert_eq!(st.unfit, Some((stalled, first)));
        // A new fault within one unfit stretch keeps the stretch's start.
        track_fitness(Some(stopped), &mut st);
        assert_eq!(st.unfit, Some((stopped, first)));
        track_fitness(None, &mut st);
        assert!(st.unfit.is_none());
    }

    /// Self-release off (`release_after = None`): the probe is not called,
    /// so nothing is unfit and the loop pings or acquires as without a probe.
    #[test]
    fn self_release_off_does_not_call_the_probe() {
        use std::sync::atomic::AtomicUsize;
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let unfit = UnfitLoop {
            name: "recovery",
            fault: LoopFault::Stopped,
        };
        let probe: FitnessProbe = Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Some(unfit)
        });
        assert_eq!(observe_fitness(Some(&probe), None), None);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            observe_fitness(Some(&probe), Some(Duration::from_secs(30))),
            Some(unfit)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(observe_fitness(None, Some(Duration::from_secs(30))), None);
    }

    #[test]
    fn unfit_loop_wording() {
        let stalled = UnfitLoop {
            name: "scheduler",
            fault: LoopFault::Stalled,
        };
        let stopped = UnfitLoop {
            name: "recovery",
            fault: LoopFault::Stopped,
        };
        assert_eq!(stalled.to_string(), "the scheduler loop is stalled");
        assert_eq!(stopped.to_string(), "the recovery loop has stopped");
    }
}
