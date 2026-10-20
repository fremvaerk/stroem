# A frozen leader must not keep the leader lock

Status: revision 1, draft for review (2026-10-20)

Code: `crates/stroem-server/src/leader.rs`, `web/health.rs`, `recovery.rs`,
`main.rs`; tests `crates/stroem-server/tests/ha_test.rs`; test image
`crates/stroem-test-support/src/lib.rs`.

## Revision history

**Revision 1 (2026-10-20).** First draft.

## 1. Problem

### 1.1 Incident (prod, 2026-10-19/20, server 0.18.1 → 0.18.3)

No replica was leader from 2026-10-19 ~09:50 UTC to 2026-10-20 02:17:23 UTC
(16.5 h). Every leader-gated loop stopped: the scheduler (every cron
trigger — `jobs/recalc-nightly` at 01:00 UTC and about six other daily
triggers were missed and never replayed), the recovery sweeper (with
retention) and the event-source manager.

- The leader pod (IP 172.31.73.52) ran on EKS node `ip-172-31-73-167`
  (i-0c2aedc2a1b2a2066). From ~09:45 UTC the node went silent to Kubernetes:
  EC2 `NetworkPacketsIn` fell from ~30k to ~67, CPU stayed flat at 15 %,
  status checks stayed OK.
- Kubernetes started a replacement pod at ~09:55. The old process stayed
  alive and very slow: it still created its last trigger job at 09:59:59 UTC
  (the 09:50 `refresh-search-index` fire, ~10 min late; earlier fires 33 s
  and 52 s late; the 09:50 and 09:55 `zd-cu-sync` fires were never created).
- The node's kernel kept the `stroem-leader` TCP session to RDS open and
  answered keepalives, so RDS (`tcp_keepalives_idle=300, interval=30,
  count=2` — a dead peer is closed in ~6 min) never closed it. The advisory
  lock `0x5354524D4C445201` stayed held and blocked every new replica,
  including the 0.18.3 deploy at 13:10 UTC.
- Karpenter (`NodeRepair=false`) removed the node only at 02:10–02:16 UTC.
  The lock came free at 02:17:23; pod `stroem-server-6b8597bb97-bnvrw` logged
  `Acquired leader lock`.

Evidence: server logs (no `Acquired leader lock` on any replica until 02:17),
RDS `error/postgresql.log.*` (each failed follower acquire logs `stroem@stroem
… could not receive data from client: Connection reset by peer`; 12/min from
ONE IP normally, 12/min from EVERY replica IP during the outage), CloudTrail
`TerminateInstances`, Karpenter logs, the `job` table. Prod database:
PostgreSQL 15.17 on RDS (`aux`); every replica connects as role `stroem` to
database `stroem`.

### 1.2 Why the code allowed it

`leader.rs` today:

- `try_acquire` opens a dedicated `PgConnection` (`application_name =
  stroem-leader`, 5 s connect timeout) and runs `pg_try_advisory_lock`.
- While leader, `run_loop` runs `SELECT 1` every `RETRY_INTERVAL` (5 s) and
  flips to follower only when that ping returns an error. The ping has no
  timeout.
- `is_leader()` is an `AtomicBool` set on acquire and cleared on a ping
  error or on shutdown.

Nothing bounds how long a session that has stopped talking keeps the lock:
PostgreSQL releases it only when the session ends, and the session ends only
when the TCP connection dies. A process that is frozen or crawling on a node
whose kernel still answers TCP keeps the lock for as long as the node lives.

Two further gaps, found while reading the code for this spec:

- **The ping can hang.** `SELECT 1` has no timeout. On a half-open
  connection it waits for the kernel's retransmission limit (Linux
  `tcp_retries2`, ~15 min), and `is_leader()` stays `true` all that time —
  even after PostgreSQL has closed the session by keepalive (~6 min) and a
  follower has taken the lock. Today, a leader partitioned from the database
  can therefore overlap a new leader by several minutes.
- **A stale `true` survives a thaw.** A process that is frozen past the
  point where its session dies still reads `is_leader() == true` when it
  resumes, until its leader task runs the next ping. Any gated loop that runs
  first acts as leader.

## 2. Goals and non-goals

Goals:

- G1. A leader that stops talking to PostgreSQL loses the lock within a
  bounded time — **≤ 35 s** (lease + one follower retry interval) — even
  when its node's kernel keeps the TCP session alive and the kubelet cannot
  restart the pod. A follower then takes over as today.
- G2. A replica never reports `is_leader() == true` on a belief older than
  the lease: no two replicas report `true` at the same time (§ 5.2).
- G3. A leader whose leader-gated loops have stopped making progress gives
  the lock up instead of holding it for a process that does no leader work
  (§ 4.4).
- G4. Operators can see who holds the lock, and whether that holder is
  stale, from the follower logs, without database access.

Non-goals (documented in `operations/high-availability.md`, not
implemented):

- Karpenter `NodeRepair=true` (cluster configuration).
- The alert rule itself (documented: `sum(stroem_leader_status) == 0 for
  5m`).
- RDS keepalive tuning.
- **Missed-fire replay.** Scheduler state is in memory; a fire that no
  replica made is never replayed (already in `docs/internal/TODO.md`, stays
  open). This design shortens the gap from hours to seconds; it does not
  close it (§ 5.3).
- Write-level fencing of leader work (§ 5.2.3).

## 3. Overview

Four changes, all in the leader-election path:

1. **Server-enforced lease** — the leader session carries
   `idle_session_timeout = 30s`. The leader pings every 5 s, so a healthy
   leader is never idle that long; PostgreSQL ends a silent leader's session
   and with it the lock.
2. **Leader validity** — `is_leader()` is `true` only while the last
   successful ping was SENT less than 20 s ago; every leader query gets a
   client-side timeout.
3. **Self-release on a stalled gated loop** — when the scheduler, recovery
   sweeper or event-source loop is `Stalled` (the `/livez` rule), the leader
   drops the lock and does not take it again while the stall lasts.
4. **Diagnostics** — followers log the lock holder (pid, client address,
   application name, backend start, idle time) and warn when it looks stale;
   the leader logs why it lost the lock.

Plus one narrowing change in the recovery sweeper (§ 4.5): it re-reads
`is_leader()` before each unit of work, not once per sweep.

## 4. Design

### 4.1 Timing

| Name | Value | Meaning |
|---|---|---|
| `retry_interval` | 5 s | unchanged: pause between loop iterations (ping when leader, acquire attempt when follower) |
| `ping_timeout` | 5 s | new: client-side bound on one leader ping |
| `acquire_timeout` | 10 s | new: bound on one whole acquire attempt (connect 5 s as today + lease setup + lock query + holder report) |
| `validity` (V) | 20 s | new: `is_leader()` is `true` only while the last successful ping was sent less than V ago |
| `lease` (N) | 30 s | new: `idle_session_timeout` of the leader session |

Invariants, checked by a unit test on the defaults and by a debug assertion
in `start_with`:

- `2 × ping_timeout + retry_interval < validity` (15 s < 20 s) — a leader
  whose pings succeed never loses validity between two of them: from the
  send of one ping to the answer of the next there are at most one ping
  duration, one pause and one more ping duration.
- `validity < lease` — the **cushion** `lease − validity` (10 s) is the time
  between a frozen leader's last moment of validity and the earliest moment
  PostgreSQL can end its session (§ 5.2).
- `lease ≥ 3 × retry_interval` — six consecutive pings must be missed before
  the server ends a session.

**Configurability: none.** The values are constants in `leader.rs`, bundled
in a `LeaderTiming` struct that tests construct with sub-second values
through `LeaderElection::start_with`. Reasons:

- The lease only has to beat the ping cadence, which Strøm controls. There
  is no workload-dependent pause to tune for (no garbage collector); a
  process that cannot run a 5 s timer task for 30 s is the failure this
  design exists to detect.
- A knob invites values that break the invariants above (e.g. a lease below
  the ping cadence makes every leader flap).
- No HA section exists in `ServerConfig`; adding one for an unused knob adds
  surface for nothing.

If an operator later needs it, a `leader:` section can be added without a
contract change.

### 4.2 Lease: `idle_session_timeout` on the leader session

`try_acquire` runs, on its dedicated connection, in this order:

1. connect (5 s timeout, `application_name = stroem-leader`, unchanged);
2. `SELECT set_config('idle_session_timeout', $1, false)` with
   `$1 = '30000ms'` — session scope (`is_local = false`), outside any
   transaction, a bound parameter (no dynamic SQL);
3. `SELECT pg_try_advisory_lock($1), pg_backend_pid()`.

The lease is set BEFORE the lock is taken, so there is no moment in which
the session holds the lock without the lease, and a failure of step 2 never
leaves a lock behind. A follower's short-lived attempt connection carries
the lease too; it is closed right after the attempt either way.

PostgreSQL semantics relied on (`idle_session_timeout`, PostgreSQL ≥ 14):
the backend is terminated when it has waited for a client query, outside a
transaction, for longer than the setting. The leader never opens a
transaction on this connection. Termination is a `FATAL` (SQLSTATE `57P05`,
"terminating connection due to idle-session timeout"); the backend exits and
releases its session-level advisory locks, independently of whether the
client ever reads the message. A frozen leader's session therefore ends at
most N after its last query.

**PostgreSQL < 14.** `set_config` raises `undefined_object` (SQLSTATE
`42704`, "unrecognized configuration parameter"). `apply_lease` maps exactly
that code to `LeaseStatus::Unsupported`; the attempt continues to step 3 and
the replica runs as today, without a lease. The first time this happens the
process logs one WARN:

> Leader lease not enforced: the database does not support
> idle_session_timeout (PostgreSQL < 14). A frozen leader keeps the leader
> lock until its TCP session dies.

Any other error from step 2 is an acquire failure (the connection is
dropped; nothing is held; the loop retries next tick). Validity (§ 4.3),
the ping timeout and self-release (§ 4.4) still work on PostgreSQL < 14;
only the server-side bound of G1 is missing. Documented as a version
requirement in `operations/high-availability.md`.

A transaction-pooling proxy (PgBouncer in transaction mode, RDS Proxy with
multiplexing) between Strøm and PostgreSQL already breaks session-level
advisory locks; the lease does not change that. Not supported, as today.

### 4.3 Leader validity

`LeaderElection` replaces its `AtomicBool` with one `AtomicI64`
`valid_until_ms`, on the process-local monotonic clock already used by
`state::Heartbeat` (`monotonic_ms`, made `pub(crate)`):

- `is_leader()` is `monotonic_ms() < valid_until_ms`.
- Not leader: `i64::MIN`. `LeaderElection::always()`: `i64::MAX`.
  `never()`: `i64::MIN`.
- On a successful acquire or ping: `valid_until_ms = sent_ms + V`, where
  `sent_ms` is read immediately BEFORE the query is sent. PostgreSQL's idle
  timer starts after it has answered that query, so it starts after
  `sent_ms` (§ 5.2.1).
- On a ping error or timeout, a release, or shutdown: `i64::MIN`, and the
  connection is dropped.

Validity is soft while the session lives: if the leader task is late but
its next ping succeeds, the session was never ended (PostgreSQL would have
ended it first), the lock is still ours, and `is_leader()` becomes `true`
again. Only a ping error, a ping timeout, a release or shutdown drops the
connection.

Every query on the leader connection gets a client-side bound: the ping
`ping_timeout` (5 s), the whole acquire attempt `acquire_timeout` (10 s). A
timed-out query leaves the sqlx connection in an unknown protocol state, so
it is dropped; dropping it closes the socket, and if the lock had been
granted, PostgreSQL releases it when the backend sees the close or, at the
latest, when the lease ends it.

`is_leader()` is read by the three gated loops, `/livez`, `/healthz/detail`
and the `stroem_leader_status` gauge; all now see validity. No caller
changes.

### 4.4 Self-release when a leader-gated loop is stalled

**Decision: implement, for `Stalled` only.**

`LeaderElection` gets an optional fitness probe, installed once by `main.rs`
after `AppState` exists (the election is started before `AppState`, so a
setter, not a constructor argument):

```rust
pub type FitnessProbe = Arc<dyn Fn() -> Option<&'static str> + Send + Sync>;
impl LeaderElection { pub fn set_fitness_probe(&self, probe: FitnessProbe); }
```

The probe returns the name of a leader-gated loop that is `Stalled` —
`health::task_health(alive, beat_age, stall_after) == Stalled`, with the
same three loops and thresholds as `/livez` (scheduler 300 s, event source
600 s, recovery `max(1800, 5 × sweep_interval_secs)`). `health.rs` factors
`background_report` into a function of `(&BackgroundTasks,
recovery_stall_secs)` so the probe can own clones of the `Arc`s it reads and
`/livez` keeps calling the same code.

Each loop iteration, before pinging or acquiring:

| Holding the lock | Probe | Action |
|---|---|---|
| yes | `Some(loop)` | **release**: validity `i64::MIN`, drop the connection, WARN `Releasing leader lock: the {loop} loop is stalled (no progress for {age}s)` |
| no | `Some(loop)` | **do not attempt** to acquire; log once per transition (WARN) and once when the stall clears (INFO) |
| yes | `None` | ping (§ 4.3) |
| no | `None` | try to acquire (§ 4.2) |

The decision is a pure function (`leader::next_action(holding, unfit)`),
unit-tested.

Reasoning:

- **Why it helps.** In this incident, and in the 2026-09-16 scheduler hang,
  the process kept pinging (or could have) while doing no leader work. The
  lease (§ 4.2) covers only a leader task that stops pinging; a leader task
  that keeps pinging for a process whose scheduler is stuck holds the lock
  for as long as the process lives. `/livez` already reports that state, but
  only the kubelet acts on `/livez`, and in this incident the kubelet could
  not reach the node.
- **Why `Stalled` only.** `Stalled` already fails `/livez` on every replica,
  leader or follower, so a replica that released stays visible to the
  kubelet and is restarted if the kubelet can act. `Stopped` (a loop task
  exited) fails `/livez` only on the leader; if the leader released on
  `Stopped`, it would become a follower whose `/livez` is green — never
  restarted and never eligible again. A leader whose loop exited keeps the
  lock and fails `/livez` as today.
- **False positives.** The probe uses `/livez`'s thresholds, and a `Stalled`
  verdict already restarts the pod today (Helm liveness: period 10 s,
  failure threshold 3). Releasing the lock is strictly milder than that
  restart, so the change adds no new false-positive exposure: any spurious
  release is preceded by a spurious restart verdict. The known
  false-positive source — a single scheduler fire whose `ref:` target
  resolves cold for longer than 300 s (TODO.md, "A scheduler fire with a
  cold or slow ref resolves inline") — now hands the lock to a healthy
  replica a few seconds before the kubelet restarts the pod.
- **No flapping back.** A stalled replica does not attempt to acquire, so it
  cannot win the lock back from a healthy follower in the next 5 s. Once its
  loops beat again it is eligible again, but the lock is held by the new
  leader, so it stays follower. Leadership moves at most once per stall.
- **Single-replica deployments.** With no follower, a release leaves no
  leader until the loop recovers or the kubelet restarts the pod: the two
  healthy loops (e.g. recovery and event source, when the scheduler is the
  stalled one) pause too. Today the same state ends in a restart of the
  whole process, which pauses them as well. Accepted.
- **Granularity.** The release takes effect on the next leader-loop
  iteration (≤ 5 s after the threshold), i.e. 5 min + 5 s for the
  scheduler, longer for the other two. That is slower than G1's 35 s; it is
  a different failure (the process still talks to PostgreSQL) and the
  thresholds are the ones operators already know from `/livez`.

The fitness probe is optional: `LeaderElection::start` without a probe
behaves exactly like § 4.2–4.3 (every existing test).

### 4.5 Recovery sweeper re-reads leadership per unit

`recovery::run_loop` checks `is_leader()` once and then runs a whole sweep;
phases 2.5 and 4 fail steps with `expected = []` (no prior-status guard), so
an ex-leader that thaws mid-sweep and a new leader sweeping the same rows
could each consume a retry for the same step. The sweep now re-reads
`is_leader()` before each phase and before each per-step unit inside phases
1, 2, 2.5, 3 and 4 (next to the existing per-unit `recovery_beat.beat()`),
and before phases 4.5, 4.6 and retention; when it reads `false` it returns
`Ok(())` with a DEBUG line. This narrows the in-flight window to one unit;
it does not close it (§ 5.2.2).

The scheduler deliberately does NOT re-read per fire. Its due batch was
computed while this replica was leader; every other replica advanced those
triggers "as if fired" when they came due (`advance_state_after_tick`), so
nobody else will fire them. If a thawed ex-leader skipped the rest of its
batch, those occurrences would be lost; letting it finish fires each
occurrence once (§ 5.2.2). The event-source manager re-reads per reconcile
(unchanged); its reconciler already cancels duplicate consumers on the next
cycle.

### 4.6 Diagnostics

Leader side (`leader.rs`):

- Acquire: `Acquired leader lock (key=0x5354524d4c445201, backend pid=…,
  lease=30s)` or `… lease not enforced`.
- Loss, by cause:
  - ping returned `57P05`: WARN `Leader lease expired: PostgreSQL ended the
    leader session after 30s without a ping (idle_session_timeout). Now a
    follower.`
  - other ping error: WARN `Leader connection lost: {err:#}` (as today);
  - ping timeout: WARN `Leader ping timed out after 5s; dropping the leader
    connection`;
  - release on stall: § 4.4.

Follower side: after a failed `pg_try_advisory_lock`, on the same attempt
connection and at most once per 60 s, the follower reads the holder:

```sql
SELECT a.pid, host(a.client_addr), a.application_name, a.backend_start,
       a.state, EXTRACT(EPOCH FROM now() - a.state_change)::float8
FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
WHERE l.locktype = 'advisory' AND l.granted
  AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database())
  AND l.classid = $1::bigint::oid AND l.objid = $2::bigint::oid
  AND l.objsubid = 1
```

(`$1`/`$2` = the high and low 32 bits of `LEADER_LOCK_KEY`; advisory locks
are per database, hence the `database` filter.) Logging:

- INFO `Leader lock held by pid … (client …, application …, backend_start
  …, state …, idle …s)` when the holder `(pid, backend_start)` changes, and
  again every 15 min while it does not — so a log search at any time finds
  the current holder.
- WARN with the same fields when the holder looks stale: `state = 'idle'`
  for longer than the lease (the lease is not enforced — PostgreSQL < 14 —
  or the holder is not a Strøm leader), or `application_name` is not
  `stroem-leader` (a co-tenant holds the key — see "Advisory lock namespace
  protection" in the HA guide).

`pg_stat_activity` shows `client_addr`, `state` and `state_change` of
another session only to the same role or `pg_read_all_stats`; every replica
uses one role (prod: `stroem`). Hidden fields are logged as `unknown`.

The client address of the holder is the old pod IP; in this incident it
would have named 172.31.73.52 in every follower's log from the first
minute. The query is a read of two catalog views; failures are logged at
DEBUG and never affect the acquire result.

Metrics: none new. `stroem_leader_status` now reflects validity. The HA
guide documents the alert `sum(stroem_leader_status) == 0` for 5 min (the
gauge is only exported when `metrics.public` is `false`).

### 4.7 Unchanged

- The lock key `0x5354524D4C445201`. Bumping its version byte would let an
  old-version and a new-version replica both be leader during the rolling
  upgrade; the new contract is compatible with the old one (§ 7).
- `application_name = stroem-leader` (operators' holder queries and
  `leader_connection_lost_triggers_reflag_to_follower` select on it).
- `/livez`, `/healthz`, `/healthz/detail` semantics; the follower loops.

## 5. Failure analysis

### 5.1 Scenarios

Times are from the moment the failure starts; "ping" = the leader task's
`SELECT 1`.

| Scenario | Before | After |
|---|---|---|
| Leader process frozen (SIGSTOP, cgroup freeze, CPU starvation, node hang), kernel answers TCP (this incident) | lock held until the node dies (16.5 h) | session ended by PostgreSQL ≤ 30 s after the last ping; a follower acquires ≤ 5 s later. On thaw the ex-leader reads `is_leader() == false` (validity lapsed at ≤ 20 s), its next ping fails, it stays follower |
| Leader crawling: leader task pings at < 30 s intervals but the scheduler loop does not come round | lock held while the process lives | scheduler `Stalled` after 300 s → leader releases within 5 s → a follower acquires |
| Leader crawling: pings < 30 s apart, loops beat within their thresholds | lock held, work slow | unchanged: slow but making progress is not a failure this design detects |
| Network partition between leader and DB (both directions) | DB closes session by keepalive after ~6 min; ex-leader's ping hangs ~15 min, `is_leader()` stays `true` → up to ~9 min of overlap | session ended at 30 s; ex-leader's validity lapses at ≤ 20 s and its ping times out at 5 s → no overlap |
| DB restart / failover | session dies, ping errors, followers race to reacquire | unchanged |
| Healthy leader, runtime starved for 20–30 s | n/a | `is_leader()` false for the starved stretch, lock kept; next successful ping restores it. Gated loops starved too, so little is lost; scheduler triggers that came due in the stretch are advanced as by a follower (missed) |
| Healthy leader, runtime starved > 30 s | n/a | session ended; failover to a follower (the same outcome as a frozen leader) |
| PostgreSQL < 14 | as row 1 "before" | no lease: frozen leader keeps the lock (WARN at startup); validity, ping timeout and self-release still apply |
| Stalled scheduler on a single replica | `/livez` restart | release first (recovery and event source pause too), then the `/livez` restart |

### 5.2 Split brain

#### 5.2.1 Two replicas reporting `is_leader() == true`

Claim: with `V < N`, at no real time do two replicas both read `true`.

Let the old leader send its last successful query at local monotonic time
`s` (read before sending). PostgreSQL receives it at real time `≥ s`,
answers, and becomes idle at `i ≥ s`. If no later query arrives, the session
ends at `i + N`; the lock is free from then on, and no other replica can
read `true` before `i + N`. The old leader reads `true` only while its
monotonic clock is below `s + V`. Monotonic clocks on two hosts advance at
the same rate within NTP's slew limit (500 ppm, i.e. 15 ms over 30 s), so in
real time the old leader's validity ends before `s + V + 0.015 s < s + N ≤
i + N`. The 10 s cushion makes the margin three orders of magnitude larger
than the drift.

Requirement on the clock: it must advance while the process is frozen.
Linux `CLOCK_MONOTONIC` (Rust's `Instant`) advances during SIGSTOP, cgroup
freezes, CPU starvation and hypervisor steal; it does not advance across a
system suspend (`CLOCK_BOOTTIME` does). Server nodes do not suspend;
accepted and documented here. After a suspend the TCP sessions are dead, so
the first ping fails within `ping_timeout` (5 s) — the remaining exposure is
the gated loops that run in those first seconds.

#### 5.2.2 Work that started while the replica was leader

`is_leader()` is read, then the work happens. A unit that starts at the last
valid moment `s + V` overlaps a new leader only if it is still running at
`s + N`, i.e. if it lasts longer than the 10 s cushion — in practice, if the
process freezes inside the unit for longer than that. Per loop:

- **Scheduler.** No duplicate fire. Triggers in the ex-leader's in-flight
  batch came due while it was leader; every follower advanced them "as if
  fired" at that time, so the new leader does not fire them. The exception
  needs the new leader's own scheduler loop to be more than the 10 s cushion
  late in processing that due time — two simultaneously sick replicas.
  Accepted.
- **Recovery.** Overlap limited to one unit (§ 4.5). The units of phases 1
  and 2 are guarded by the observed claim (`claimed_step_failed`); a
  duplicate in phase 2.5 or 4 (`expected = []`) can consume one extra retry
  for one step. Accepted; closing it needs write-level fencing.
- **Event source.** An in-flight reconcile can create a second consumer job;
  the next reconcile of the leader cancels the duplicate (existing
  behaviour).

#### 5.2.3 Not done: write-level fencing

A real fence makes every leader-gated write conditional on the lock-holding
session still being alive — e.g. run gated writes on the leader connection
itself, or check in the same transaction that the leader's backend pid still
holds the lock in `pg_locks`. Every gated code path (job creation, settlement,
retention) would have to take the leader session or the check. Out of scope;
the residual of § 5.2.2 is small and bounded by a freeze inside one unit.

### 5.3 Missed fires during a handover

Followers advance triggers without firing. A trigger that comes due between
the moment the old leader stops firing and the moment the new leader
acquires is never fired by anyone: after a frozen leader, ≤ ~35 s
(validity lapse to acquire); after a stalled scheduler, the stall (≥ 300 s)
plus ≤ 5 s. Today: the whole outage (16.5 h). Replay of missed fires stays
an open TODO item.

## 6. Testing

### 6.1 Test database version

`idle_session_timeout` needs PostgreSQL ≥ 14; the shared test container is
`postgres:11-alpine`. **Decision: bump the shared image to
`postgres:15-alpine`** in `stroem-test-support` (prod runs 15.17, so every
integration test becomes more faithful), not a second container. The whole
workspace suite must pass on it; if an unrelated test breaks, stop and ask
the user before changing that test. `docker-compose.yml` (e2e) already uses
`postgres:17`.

### 6.2 Unit tests (in-module)

- `leader.rs`: default `LeaderTiming` satisfies the invariants of § 4.1;
  validity arithmetic (`is_leader` false at/after `valid_until`, `always()`,
  `never()`); `next_action` table of § 4.4; `lease_setting(Duration)` →
  `"30000ms"`; SQLSTATE classification (`42704` → `Unsupported`, other codes
  → error); `57P05` loss message.
- `health.rs`: the factored report — a backdated scheduler beat yields
  `Some("scheduler")`, fresh beats or a not-yet-started loop yield `None`,
  `Stopped` yields `None`.

### 6.3 Integration tests (`crates/stroem-server/tests/ha_test.rs`)

All `#[serial]` with the existing leader tests; short timing through
`start_with` (`retry_interval` 200 ms, `ping_timeout` 1 s,
`acquire_timeout` 2 s, `validity` 1 s, `lease` 2 s). A test-only
`#[doc(hidden)] pub fn pause_for_test(&self, paused: bool)` makes the leader
loop skip its iterations — no ping, no acquire — which is what PostgreSQL
sees from a frozen process.

1. `frozen_leader_loses_lock_to_follower_after_lease`: A leads, B follows;
   pause A; A reads `false` within validity + 0.5 s, before B acquires; B
   acquires within lease + 3 s; sampled every 20 ms, A and B are never both
   `true`.
2. `thawed_ex_leader_stays_follower`: continue 1 — resume A; for 2 s A reads
   `false` at every sample and B `true`.
3. `healthy_leader_keeps_lock_past_lease`: A leads for 3 × lease; the
   holder pid (from `lock_holder`) never changes; B never reads `true`.
4. `lease_setup_maps_unrecognized_parameter_to_unsupported`: `apply_lease`
   with a non-existent parameter name on PostgreSQL 15 returns
   `Unsupported` — the real server error code, not a hand-made one; with
   `idle_session_timeout` it returns `Enforced` and `SHOW
   idle_session_timeout` on that connection reads the value set.
5. `stalled_loop_releases_lock_and_blocks_reacquire`: A with a probe backed
   by an `AtomicBool`; set it → A reads `false` within one tick and its
   session is gone; B acquires; while set, A never reads `true` even after B
   is cancelled; clear it → A acquires.
6. `follower_reports_lock_holder`: `lock_holder` on a second connection
   returns A's backend pid with `application_name = stroem-leader`.
7. `recovery_sweep_stops_when_leadership_lost`: `sweep_once` with
   `LeaderElection::never()` leaves a stale worker `active` and its running
   step untouched.

Existing tests keep the default timing and must pass unchanged.

## 7. Rollout

- No migration, no configuration, no API change. Lock key unchanged.
- Mixed fleet during a rolling upgrade: old replicas lead without a lease,
  new ones with it; both interoperate. The incident can recur until every
  replica runs the release.
- PostgreSQL ≥ 14 required for the lease (§ 4.2). Prod (RDS 15.17)
  qualifies.
- Operators: enable Karpenter `NodeRepair` and the
  `sum(stroem_leader_status) == 0 for 5m` alert (documented, not part of the
  release).
- A lease expiry appears in the PostgreSQL log as `FATAL: terminating
  connection due to idle-session timeout` for the `stroem-leader` session;
  documented so it is not mistaken for an error.

## 8. Docs

- `docs/src/content/docs/operations/high-availability.md`: the lease and
  validity, PostgreSQL ≥ 14, self-release, the frozen-node failure mode,
  holder logs, Karpenter `NodeRepair`, the alert, RDS keepalives,
  missed-fire replay still open.
- `CLAUDE.md` § High Availability: the lease, validity, self-release,
  diagnostics, test image.
- `CONTEXT.md`: **Leader lease**, **Leader validity**.
- `docs/internal/TODO.md`: the incident under Bugs, `[x]` with this fix;
  missed-fire replay stays open; write-level fencing added as open.

## 9. Open questions for the user

- Q1. Self-release (§ 4.4) is in. Confirm, or cut it to diagnostics only.
- Q2. Lease 30 s / validity 20 s, not configurable (§ 4.1).
