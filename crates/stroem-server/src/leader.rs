//! Leader election via Postgres advisory locks.
//!
//! At most one server replica holds the leader role at any time. The leader is
//! the only replica that runs the scheduler, event-source manager, and recovery
//! sweeper background tasks. Followers serve API/WebSocket traffic only.
//!
//! The lock is `pg_try_advisory_lock($LEADER_LOCK_KEY)` held on a **dedicated**
//! Postgres connection (outside the shared pool). Postgres releases the lock
//! automatically when the connection closes — so leader failover happens
//! whenever the leader's connection drops (pod restart, network partition, DB
//! restart). A follower picks the lock up on its next retry tick.
//!
//! For tests and single-replica deployments, [`LeaderElection::always`] returns
//! a leader that always reports `true` without contacting the database.

use anyhow::{Context, Result};
use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, PgConnection};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
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

/// How often to retry lock acquisition when not leader, and to ping the
/// connection when leader.
///
/// This constant also bounds how quickly `is_leader()` can flip to `false`
/// after primary loss: up to `RETRY_INTERVAL` (5 s) may elapse before the
/// ping detects the dead connection and clears the flag. This is within k8s
/// liveness-probe tolerance and avoids a more complex dual-interval design.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Leader election handle shared across background tasks.
///
/// All gated background tasks call [`LeaderElection::is_leader`] each tick to
/// decide whether to do work. The flag is updated by the loop spawned in
/// [`LeaderElection::start`].
pub struct LeaderElection {
    is_leader: Arc<AtomicBool>,
}

impl LeaderElection {
    /// Returns a leader handle that always reports `true`.
    ///
    /// Use for unit tests, single-replica deployments, or any context where the
    /// HA coordinator isn't running.
    pub fn always() -> Arc<Self> {
        Arc::new(Self {
            is_leader: Arc::new(AtomicBool::new(true)),
        })
    }

    /// Spawn the leader-election loop. Returns a shared handle plus the task
    /// `JoinHandle` so the caller can await shutdown.
    ///
    /// The loop opens a dedicated `PgConnection`, repeatedly tries
    /// `pg_try_advisory_lock(LEADER_LOCK_KEY)`, and pings the connection while
    /// leader to detect drops. On disconnect, the lock is released
    /// server-side and the next retry will re-acquire (or another replica will
    /// win).
    #[tracing::instrument(skip(db_url, cancel))]
    pub fn start(db_url: String, cancel: CancellationToken) -> (Arc<Self>, JoinHandle<()>) {
        let is_leader = Arc::new(AtomicBool::new(false));
        let leader = Arc::new(Self {
            is_leader: is_leader.clone(),
        });
        let handle = tokio::spawn(run_loop(db_url, is_leader, cancel));
        (leader, handle)
    }

    /// Returns `true` if this replica currently holds the leader lock.
    #[inline]
    pub fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::Relaxed)
    }
}

async fn run_loop(db_url: String, is_leader: Arc<AtomicBool>, cancel: CancellationToken) {
    tracing::info!("LeaderElection started");

    // Connection held while leader. None when not leader.
    let mut conn: Option<PgConnection> = None;

    loop {
        if conn.is_none() {
            // Try to become leader.
            match try_acquire(&db_url).await {
                Ok(Some(new_conn)) => {
                    is_leader.store(true, Ordering::Relaxed);
                    conn = Some(new_conn);
                    tracing::info!("Acquired leader lock (key={:#x})", LEADER_LOCK_KEY);
                }
                Ok(None) => {
                    // Another replica holds the lock — stay follower.
                    tracing::debug!("Leader lock held by another replica; staying follower");
                }
                Err(e) => {
                    tracing::warn!("Leader acquire failed: {:#}", e);
                }
            }
        } else {
            // Already leader — verify the connection is still alive. If the
            // ping fails Postgres has dropped our session and released the
            // lock; flip back to follower so we retry from scratch.
            let c = conn.as_mut().expect("checked");
            if let Err(e) = sqlx::query("SELECT 1").execute(&mut *c).await {
                tracing::warn!("Leader connection lost: {:#}. Releasing leader role.", e);
                is_leader.store(false, Ordering::Relaxed);
                conn = None;
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(RETRY_INTERVAL) => {}
            _ = cancel.cancelled() => {
                tracing::info!("LeaderElection shutting down");
                is_leader.store(false, Ordering::Relaxed);
                // Dropping `conn` closes the session and releases the advisory
                // lock so the next replica can take over immediately.
                return;
            }
        }
    }
}

/// Open a fresh connection and try to acquire the leader lock.
///
/// Returns:
/// - `Ok(Some(conn))` if the lock was acquired — caller must hold the
///   connection alive to retain the lock.
/// - `Ok(None)` if another replica already holds the lock (connection is
///   dropped immediately).
/// - `Err(_)` if the connection or query failed (e.g. DB outage). Retried
///   by the loop.
async fn try_acquire(db_url: &str) -> Result<Option<PgConnection>> {
    // Parse URL and annotate the connection with an application_name so
    // pg_stat_activity rows are easy to identify.
    //
    // sqlx 0.8's `PgConnectOptions` doesn't expose a per-options connect
    // timeout directly, so we wrap the entire attempt in `tokio::time::timeout`
    // (5 s). This ensures the leader loop fails fast during an RDS Multi-AZ
    // failover or network partition instead of blocking for the OS TCP timeout
    // (~2 min).
    let opts = PgConnectOptions::from_str(db_url)
        .context("parse leader DB URL")?
        .application_name("stroem-leader");
    let mut conn = tokio::time::timeout(Duration::from_secs(5), PgConnection::connect_with(&opts))
        .await
        .map_err(|_| anyhow::anyhow!("leader connection timed out (5s)"))?
        .context("connect dedicated leader connection")?;

    let (acquired,): (bool,) = sqlx::query_as("SELECT pg_try_advisory_lock($1)")
        .bind(LEADER_LOCK_KEY)
        .fetch_one(&mut conn)
        .await
        .context("pg_try_advisory_lock")?;

    if acquired {
        Ok(Some(conn))
    } else {
        // Don't hold the connection — close it so we don't keep an idle session
        // on the DB while waiting. The lock isn't ours either way.
        drop(conn);
        Ok(None)
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
    fn leader_lock_key_is_strmldr_v1() {
        // Sanity check: documenting the key encoding so a future refactor
        // can't silently change it (which would split-brain across versions
        // during a rolling upgrade).
        let bytes = LEADER_LOCK_KEY.to_be_bytes();
        assert_eq!(&bytes[..7], b"STRMLDR");
        assert_eq!(bytes[7], 0x01);
    }
}
