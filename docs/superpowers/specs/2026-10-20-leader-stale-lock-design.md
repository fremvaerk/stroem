# A frozen leader must not keep the leader lock

Status: revision 8 (the user restored the caps of § 4.1), under Codex
review. Revision 7 (no caps) was approved and implemented, then reversed;
revisions 6 and 5 were approved in rounds 2 and 4 (2026-10-20).

Code: `crates/stroem-server/src/leader.rs`, `config.rs`, `web/health.rs`,
`scheduler.rs`, `main.rs`; tests `crates/stroem-server/tests/ha_test.rs`;
test image `crates/stroem-test-support/src/lib.rs`.

## Revision history

**Revision 8 (2026-10-20, the user's decision).** The caps of revision 6
are back: retry and ping timeout min(L/6, 5 s), acquire timeout
min(L/3, 10 s), validity floor(2L/3) (§ 4.1). The corrections found in
the revision 7 reviews stay, restated for the caps: the worst-case
takeover after an immediate release (`2 × acquire_timeout +
retry_interval`), broken-connection detection counted from the fault
(`retry_interval + ping_timeout`), and a healthy leader's idle time of up
to `2 × ping_timeout + retry_interval` in the holder report.

**Revision 7 (2026-10-20, the user's decision).** The derived values have
no caps: retry and ping timeout L/6, acquire timeout L/3, validity
floor(2L/3) (§ 4.1). Everything scales with the lease, which adds the
trade-offs listed in § 4.1 for a long lease (slower detection of a broken
connection, slower follower attempts, a wider early-end overlap,
§ 5.2.1). The defaults (L = 30 s) do not change.

**Revision 6 (2026-10-20, the user's decisions on § 9).**
- Q1: self-release stays (§ 4.4).
- Q2: the timing is configurable through two settings, `leader.lease_secs`
  (default 30) and `leader.self_release_after_secs` (default 30, `0` = off);
  every other value is derived from the lease, with caps (§ 4.1). The
  defaults are the values of revision 5.
- Q3: a gated loop that ran and then EXITED (`Stopped`) makes the leader
  release too, and `/livez` fails on such a loop on every replica, not only
  on the leader (§ 4.4, § 4.4.1). `/healthz/detail` follows.
- At shutdown the gated loops exit (`Stopped`). The leader loop does not
  start an iteration once `cancel` has fired; an iteration already running
  can still release, which is harmless (§ 4.4).
- "Off" disables only the fitness-based release and acquire suppression;
  the § 4.4.1 health rules apply either way (Codex round 1 on revision 6).

**Revision 5 (2026-10-20, Codex implementation review round 1 — tests and
docs only, no production-code finding).** Test 7 holds the ping until the
test releases it and anchors its lower bound on the proxy's forward time;
tests 5 and 6 run A with `long_validity` (validity 4 s, lease 5 s) so the
detection deadline lies before any passive validity lapse, test 6 checks
that the held successful answer reached A after the termination, and both
accept either replica as the next leader (A is healthy and may win). The
operations guide keeps the 55 s bound's anchor and network assumption.

**Revision 4 (2026-10-20, Codex review round 3, 1 finding, verified).**
The test proxy (§ 6.3) frames PostgreSQL protocol messages instead of
acting on TCP read chunks (TCP keeps no write boundaries): it cuts at a
chosen offset inside a selected `Q` message, delays whole messages, and
exposes barriers that tests 3, 6 and 7 wait on before they pause A or end
its backend, so each test exercises the case it names.

**Revision 3 (2026-10-20, Codex review round 2, 3 findings, all verified).**
- § 4.2: protocol rule — from the lock query on, the leader session sends
  only simple-protocol `Query` messages (`sqlx::raw_sql`). PostgreSQL arms
  the idle timer only at ReadyForQuery; with the extended protocol a session
  that received Bind/Execute but never Sync would hold the lock with no
  timer. A simple `Query` is one message, and a message delivered in part
  leaves the timer armed. New proxy test (§ 6.3, test 3).
- § 5.2.1: the early-session-end overlap is ≤ `2 × ping_timeout +
  retry_interval` (15 s) with the leader task running — a successful answer
  can arrive late — and never more than V (20 s); test 6 delays answers.
- § 5.1 / G1: the takeover bound is from the backend's last idle start; from
  the freeze, add the delivery delay of the one leader message that can be
  in flight (bounded by the client kernel's TCP retransmission limit). Test 7
  delays requests.

**Revision 2 (2026-10-20, Codex review round 1, 10 findings, all verified
against the code).**
- Dropped the recovery sweeper's per-unit leadership re-checks (former § 4.5).
  Aborting phase 1 after `mark_stale_inactive` would strand the steps of a
  worker no later sweep rediscovers (`status = 'active'` predicate); the
  claimed one-unit bound also ignored the multi-unit loops of phases 4.5, 4.6
  and retention, and the proposed test could not see a mid-sweep transition.
  The recovery loop is unchanged; § 5.2.2 states the residual as one whole
  in-flight sweep.
- § 5.2.1: the no-overlap proof is restricted to a takeover caused by lease
  expiry. A session that ends EARLY (terminated by an administrator, a reset
  connection) lets a follower acquire while the old leader is still valid;
  the overlap is bounded and documented, and a test pins the bound. G2
  restated.
- § 5.2.2: immediate-release handovers (shutdown, self-release, early session
  end) have no cushion; the scheduler's duplicate-fire window for them is
  analysed and accepted, not claimed away.
- § 4.4: self-release now requires the stall to persist for 30 s
  (`release_after`, matching Helm's default liveness 3 × 10 s) and the text no
  longer claims equivalence with a kubelet restart — it is more sensitive
  when liveness probes are disabled, and in-flight work continues. `Stopped`
  stays excluded with an explicit reason and is open question Q3; the
  scheduler now beats before its initial trigger load so a hang there reads
  `Stalled` instead of healthy forever.
- G1 and § 5.1: the bounds now separate session expiry from completed
  takeover, include attempt durations, and state their assumptions.
- § 6.3: test timings satisfy the § 4.1 invariants; a frozen TCP proxy test
  covers a ping that times out in flight (the incident's network shape).
- § 5.2.3: a `pg_locks` check in the write transaction is not a fence;
  replaced with mechanisms that are.

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
  bounded time, even when its node's kernel keeps the TCP session alive and
  the kubelet cannot restart the pod: PostgreSQL ends its session at most
  N (the lease, default 30 s) after the session last became idle, and a
  follower completes the takeover one attempt later (§ 5.1 gives the bound
  with attempt durations: ~35 s typical, ≤ 55 s worst case with the
  defaults, from the session's last idle start; from the freeze, add the
  delivery delay of a leader message already in flight — milliseconds on a
  working network, bounded by the client kernel's TCP retransmission limit
  on a degraded one).
- G2. A replica never acts on a leadership belief older than V (the
  validity, 2N/3, default 20 s). Two replicas never both read `is_leader()
  == true` when the takeover follows a lease expiry; when the old session
  ends early, the overlap is bounded (§ 5.2.1).
- G3. A leader whose leader-gated loop is `Stalled`, or ran and then
  exited (`Stopped`), for `release_after` (default 30 s) gives the lock up
  instead of holding it for a process that does no leader work (§ 4.4).
  Operators can turn this off (`leader.self_release_after_secs: 0`).
- G4. Operators can see who holds the lock, and whether that holder is
  stale, from the follower logs, without database access.

Non-goals (documented in `operations/high-availability.md`, not
implemented):

- Karpenter `NodeRepair=true` (cluster configuration).
- The alert rule itself (documented: `sum(stroem_leader_status) == 0 for
  5m`).
- RDS keepalive tuning.
- **Missed-fire replay** and **duplicate-fire deduplication.** Scheduler
  state is in memory; a fire that no replica made is never replayed, and two
  fires of one occurrence are not detected (already in
  `docs/internal/TODO.md`; stays open). This design shortens the leaderless
  gap from hours to tens of seconds; it does not close it (§ 5.3).
- Write-level fencing of leader work (§ 5.2.3).
- Changes to the recovery sweeper or the event-source manager.

## 3. Overview

Five changes in the leader-election path, one in the scheduler, one in the
health endpoints:

1. **Server-enforced lease** — the leader session carries
   `idle_session_timeout` = N (default 30 s). The leader pings every
   `retry_interval` (min(N/6, 5 s), default 5 s), so a healthy leader is
   never idle that long; PostgreSQL ends a silent
   leader's session and with it the lock.
2. **Leader validity** — `is_leader()` is `true` only while the last
   successful ping was SENT less than V (default 20 s) ago; every leader
   query gets a client-side timeout.
3. **Self-release on an unfit gated loop** — when the scheduler, recovery
   sweeper or event-source loop has been `Stalled` (the `/livez` rule), or
   ran and then exited (`Stopped`), for `release_after` (default 30 s), the
   leader drops the lock and does not take it again while that lasts.
4. **Diagnostics** — followers log the lock holder (pid, client address,
   application name, backend start, idle time) and warn when it looks stale;
   the leader logs why it lost the lock.
5. **Configuration** — a `leader:` section with `lease_secs` and
   `self_release_after_secs`; the other values are derived (§ 4.1).
6. **Scheduler startup beat** — the scheduler beats once before its initial
   trigger load, so a hang there is `Stalled`, not healthy forever.
7. **Health contract** — `/livez` and `/healthz/detail` report a loop that
   ran and then exited as `stopped` on every replica (§ 4.4.1).

## 4. Design

### 4.1 Timing and configuration

Two settings, in a new `leader:` section of `ServerConfig`
(`config::LeaderConfig`, `#[serde(default)]`, `deny_unknown_fields`):

```yaml
leader:
  lease_secs: 30               # N: idle_session_timeout of the leader session
  self_release_after_secs: 30  # release_after (§ 4.4); 0 = self-release off
```

Env overrides `STROEM__LEADER__LEASE_SECS` and
`STROEM__LEADER__SELF_RELEASE_AFTER_SECS` (a plain struct, so the `config`
crate's string-to-integer coercion applies; a test checks it). Every other
value is derived from the lease `L` by `LeaderTiming::from_lease(lease,
release_after)`, in integer milliseconds:

| Name | Formula | Default (L = 30 s) | Meaning |
|---|---|---|---|
| `retry_interval` | min(L/6, 5 s) | 5 s | unchanged: pause between loop iterations (ping when leader, acquire attempt when follower) |
| `ping_timeout` | min(L/6, 5 s) | 5 s | new: client-side bound on one leader ping |
| `acquire_timeout` | min(L/3, 10 s) | 10 s | new: bound on one whole acquire attempt (connect + lease setup + lock query + holder report) |
| `validity` (V) | floor(2L/3) | 20 s | new: `is_leader()` is `true` only while the last successful ping was sent less than V ago |
| `lease` (N) | L | 30 s | new: `idle_session_timeout` of the leader session |
| `release_after` | `self_release_after_secs`; `None` when 0 | 30 s | new: how long a gated loop must stay unfit before the leader releases (§ 4.4) |

`LeaderTiming::default()` is `from_lease(30 s, Some(30 s))`: the values of
revision 5. Each value is the floor of its formula in whole milliseconds
(validity is floor(2L/3), not 2 × floor(L/3)). `CONNECT_TIMEOUT` (5 s)
stays a constant inside the acquire attempt; for a lease below 15 s the
outer `acquire_timeout` (down to 3.3 s) is the effective bound.

Invariants (`LeaderTiming::check`):

- `2 × ping_timeout + retry_interval < validity` — a leader whose pings
  succeed never loses validity between two of them: from the send of one
  ping to the answer of the next there are at most one ping duration, one
  pause and one more ping duration.
- `validity < lease` — the **cushion** `lease − validity` (L/3, default
  10 s) separates a frozen leader's last valid moment from the earliest
  moment PostgreSQL can end an idle session (§ 5.2).
- `lease ≥ 3 × retry_interval` — at least two scheduled pings must be
  missed before the server ends a healthy session.

They hold for every allowed lease. For L < 30 s nothing is capped: L/2 <
2L/3 < L, and L ≥ L/2. For L ≥ 30 s: 15 s < 2L/3 (≥ 20 s) < L, and L ≥
15 s. Integer division moves each value by under 1 ms, while every gap is
at least L/6 ≥ 1.6 s. A unit test runs `check` for every whole second from
10 s to 3600 s. `ServerConfig::validate` rejects a range violation and also
runs `check` on the derived timing (a release build has no debug
assertions); `start_with` keeps its debug assertion for test timings.

Ranges, enforced by `ServerConfig::validate`:

- `lease_secs`: 10 to 3600. Below 10 s a follower opens a new attempt
  connection more often than every 1.7 s. Above 1 h a frozen leader blocks
  the takeover for over an hour, which defeats G1.
- `self_release_after_secs`: 0 (off) or 1 to 3600.

Why caps on the ping, the attempt and the retry: they detect a broken
connection and pace the follower, which has nothing to do with how long a
leader may be silent. Without caps (revision 7, reversed by the user in
revision 8), `lease_secs: 3600` would abandon a hung ping only after
10 min, keep a follower 10 min between attempts (50 min worst case after
an immediate release), and widen § 5.2.1's early-end overlap to 30 min.

Three bounds that follow from the formulas, for every lease ≥ 30 s (below
30 s nothing is capped; the bound in parentheses applies):

- **Takeover after an immediate release** (shutdown, self-release, early
  session end): the next follower attempt, typically ≤ `retry_interval`
  = 5 s (L/6) with attempts of milliseconds; worst case `2 ×
  acquire_timeout + retry_interval` = 25 s (5L/6), when an attempt that
  is refused just before the release runs to its timeout before the
  pause.
- **Broken-connection detection:** a hung ping is abandoned
  `ping_timeout` = 5 s (L/6) after it was sent; counted from the moment
  the connection breaks, detection takes up to `retry_interval +
  ping_timeout` = 10 s (L/3), because the fault can start just after a
  successful ping.
- **Early-end overlap** (§ 5.2.1): up to `2 × ping_timeout +
  retry_interval` = 15 s (L/2).

What the lease trades:

- **Smaller lease:** faster takeover of a frozen leader (≈ N + one
  attempt); a leader runtime starved longer than 2N/3 loses validity, and
  longer than N loses the lock; below 30 s also faster detection of a
  broken connection, a database answer slower than N/6 drops the
  connection, and more attempt connections per follower (one every N/6).
- **Larger lease:** slower takeover of a frozen leader; more tolerance for
  a starved leader runtime. Not more tolerance for a slow database: a
  ping slower than 5 s still drops the connection. Because of the caps,
  the three bounds above stay the same.

Each replica uses its own values. The no-overlap argument (§ 5.2.1) needs
`validity < lease` within ONE replica — the leader's validity against its
own session's lease — so replicas with different values (a configuration
rollout) are safe. A follower's holder report (§ 4.6) compares the holder's
idle time with its own lease. A healthy leader's backend is idle about
its retry interval (min(lease/6, 5 s)) on a prompt network, and up to `2 ×
ping_timeout + retry_interval` (min(lease/2, 15 s)) when the answer and
the next ping are each delayed just below the ping timeout. A follower
whose lease is shorter than that idle time logs a false "looks stale"
WARN. This needs different leases on the two replicas (with equal leases
the idle time stays below the lease) and a follower lease below 15 s; it
is a log line only, and it ends with the rollout.

### 4.2 Lease: `idle_session_timeout` on the leader session

`try_acquire` runs, on its dedicated connection, in this order:

1. connect (5 s timeout, `application_name = stroem-leader`, unchanged);
2. `SELECT set_config('idle_session_timeout', $1, false)` with
   `$1 = '{N in ms}ms'` (default `'30000ms'`) — session scope (`is_local = false`), outside any
   transaction, a bound parameter (no dynamic SQL);
3. `SELECT pg_try_advisory_lock(x'5354524D4C445201'::bigint),
   pg_backend_pid()` — sent with the **simple query protocol**
   (`sqlx::raw_sql`, a constant `&'static str`; a unit test checks that the
   literal is `LEADER_LOCK_KEY`).

The lease is set BEFORE the lock is taken, so there is no moment in which
the session holds the lock without the lease, and a failure of step 2 never
leaves a lock behind. A follower's short-lived attempt connection carries
the lease too; it is closed right after the attempt either way.

PostgreSQL semantics relied on (`idle_session_timeout`, PostgreSQL ≥ 14;
`PostgresMain` in `src/backend/tcop/postgres.c`): the backend arms the idle
timer when it is about to send ReadyForQuery outside a transaction block,
keeps it armed while it reads the next message (`ReadCommand`, including a
message that has arrived only in part), and disarms it once a whole message
has been read. When the timer fires during that read, the backend ends the
session with a `FATAL` (SQLSTATE `57P05`, "terminating connection due to
idle-session timeout"), exits and releases its session-level advisory
locks, whether or not the client reads the message (~100 bytes; the
client's socket buffer cannot be full, because the client only ever sends
tiny queries). An executing statement is outside the timer; every statement
of the leader session completes in milliseconds and none waits on a lock.

**Protocol rule: from the lock query on, the leader session sends only
simple-protocol `Query` messages** (`sqlx::raw_sql`): the lock query and
every ping (`SELECT 1`). The extended protocol (`sqlx::query`) sends one
statement as several messages (Bind, Execute, Sync); between Execute and
Sync the backend has sent no ReadyForQuery, so no idle timer is armed while
it waits — a session whose Sync never arrives (a stalled intermediary
keeping the connection open, a write cut short) would hold the lock with
no lease. A simple `Query` is one message: before it is complete the timer
is armed, after it the backend answers with ReadyForQuery and re-arms it.
So at every moment after the lock is granted, the backend is executing a
millisecond statement or waiting for a message with the idle timer armed.
`set_config` (step 2) and the follower's holder report (§ 4.6) keep the
extended protocol with bound parameters: neither runs while the session
holds the lock. The leader never opens a transaction block on this
connection. A frozen leader's session therefore ends at most N after the
backend last became idle (the bound from the freeze is § 5.1).

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
  timer for that query starts after it has answered, so after `sent_ms`
  (§ 5.2.1).
- On a ping error or timeout, a release, or shutdown: `i64::MIN`, and the
  connection is dropped.

Validity is soft while the session lives: if the leader task is late but
its next ping succeeds, the session was never ended, the lock is still ours,
and `is_leader()` becomes `true` again. Only a ping error, a ping timeout, a
release or shutdown drops the connection.

Every query on the leader connection gets a client-side bound: the ping
(`raw_sql("SELECT 1")`, § 4.2's protocol rule) `ping_timeout` (default
5 s), the whole acquire attempt `acquire_timeout` (default 10 s). A
timed-out query leaves the sqlx connection in an unknown protocol state, so
it is dropped; dropping it closes the socket. If the lock had been granted,
PostgreSQL releases it when the backend sees the close, or at the latest
when the lease ends the idle session.

`is_leader()` is read by the three gated loops, `/livez`, `/healthz/detail`
and the `stroem_leader_status` gauge; all now see validity. No caller
changes.

### 4.4 Self-release when a leader-gated loop is unfit

**Decision (user, Q1 and Q3): implement, for a loop that is `Stalled`, or
ran and then exited (`Stopped`), when that persists for `release_after`
(default 30 s; `leader.self_release_after_secs`, `0` = off).**

`LeaderElection` gets an optional fitness probe, installed once by `main.rs`
after `AppState` exists (the election is started before `AppState`, so a
setter, not a constructor argument):

```rust
pub enum LoopFault { Stalled, Stopped }
pub struct UnfitLoop { pub name: &'static str, pub fault: LoopFault }
pub type FitnessProbe = Arc<dyn Fn() -> Option<UnfitLoop> + Send + Sync>;
impl LeaderElection { pub fn set_fitness_probe(&self, probe: FitnessProbe); }
```

The probe (`health::leader_fitness_probe`, built on `health::unfit_loop`)
returns the first leader-gated loop, in the order scheduler, recovery,
event source, that is either:

- `Stalled` — `health::task_health(alive, beat_age, stall_after) ==
  Stalled`, with the same thresholds as `/livez` (scheduler 300 s, event
  source 600 s, recovery `max(1800, 5 × sweep_interval_secs)`); or
- `Stopped` after it STARTED — its `AliveGuard` is gone and its heartbeat
  has beaten at least once (`TaskReport::started`, the rule `/livez` uses,
  § 4.4.1). A loop that never beat (not spawned yet, router-only tests) is
  not unfit.

`health.rs` factors `background_report` into a function of
`(&BackgroundTasks, recovery_stall_secs)` so the probe can own clones of
the `Arc`s it reads and `/livez` keeps calling the same code.

The leader loop keeps `unfit: Option<(UnfitLoop, Instant)>`: set on the
first iteration the probe returns `Some`, cleared on the first that returns
`None`. If the named loop or fault changes within one unfit stretch, the
record takes the new `UnfitLoop` and keeps the first sighting's time (the
replica stayed unfit). Each iteration, before pinging or acquiring:

| Self-release | Holding the lock | Probe | Action |
|---|---|---|---|
| off (`release_after = None`) | any | not called | ping or acquire as in § 4.2–4.3; no fitness log line |
| on | yes | `Some(loop)`, unfit for ≥ `release_after` | **release**: validity `i64::MIN`, drop the connection, WARN `Releasing leader lock: the {loop} loop {is stalled \| has stopped} ({age}s); not taking the lock again while that lasts` |
| on | yes | `Some(loop)`, unfit for < `release_after` | ping (§ 4.3); WARN once when the stretch starts |
| on | yes | `None` | ping (§ 4.3) |
| on | no | `Some(loop)` (any duration) | **do not attempt** to acquire; WARN once when the stretch starts, INFO when it ends |
| on | no | `None` | try to acquire (§ 4.2) |

The decision is a pure function (`leader::next_action(holding, unfit_for:
Option<Duration>, release_after: Duration)`), unit-tested; "off" is decided
before it: `iterate` does not call the probe when `release_after` is
`None`, so `unfit` stays `None`.

Timing:

- `Stalled`: a stall that started at `t` is `Stalled` from `t +
  threshold`; the probe sees it on the next iteration (≤ `retry_interval +
  ping_timeout`, default 10 s, later); the release happens on the first
  iteration at least `release_after` after that — i.e. by `t + threshold +
  10 s + 30 s + 10 s` with the defaults (5 min 50 s for the scheduler).
- `Stopped`: no threshold; a loop that exits at `t` is released by `t +
  10 s + 30 s + 10 s` (50 s) with the defaults.

Slower than G1; it is a different failure (the process still talks to
PostgreSQL), and the thresholds are the ones operators already know from
`/livez`.

Reasoning:

- **Why it helps.** In this incident, and in the 2026-09-16 scheduler hang,
  the process could keep pinging while doing no leader work. The lease
  (§ 4.2) covers only a leader task that stops pinging; a leader task that
  keeps pinging for a process whose scheduler is stuck, or whose scheduler
  task panicked, holds the lock for as long as the process lives. `/livez`
  already reports that state, but only the kubelet acts on `/livez`, and in
  this incident the kubelet could not reach the node.
- **False positives — more sensitive than a restart, accepted.** The probe
  uses `/livez`'s rules, and the default `release_after` (30 s) matches
  Helm's default liveness policy (`periodSeconds: 10`, `failureThreshold:
  3`), so with default probes an unfit stretch long enough to release is,
  give or take one probe period, one long enough to restart the pod today.
  It is NOT equivalent: with liveness probes disabled or loosened, release
  happens where no restart would, and after a release the process's
  in-flight work continues (a restart would end it; § 5.2.2). We accept the
  more sensitive policy: a release costs a handover (and the fires of
  § 5.3), while an unfit leader costs every trigger until the fault ends.
  Operators who disagree set `self_release_after_secs: 0`.
- **Known false-positive source.** A single scheduler fire whose `ref:`
  target resolves cold for longer than 300 s (TODO.md, "A scheduler fire
  with a cold or slow ref resolves inline") — now hands the lock to a
  healthy replica shortly before the kubelet restarts the pod.
- **No flapping back.** An unfit replica does not attempt to acquire, so it
  cannot win the lock back from a healthy follower. Once its loops beat
  again it is eligible again, but the lock is held by the new leader, so it
  stays follower. Leadership moves at most once per unfit stretch. A
  `Stopped` loop never comes back within the process (nothing respawns a
  loop task), so that replica stays follower until it restarts — and
  § 4.4.1 makes its `/livez` fail, so the kubelet restarts it.
- **Single-replica deployments.** With no follower, a release leaves no
  leader until the fault ends or the process restarts; the healthy loops
  pause too. With liveness probes, today the same state ends in a restart
  of the whole process, which pauses them as well. Without liveness probes
  (Docker Compose, probes disabled), a `Stopped` loop now pauses ALL leader
  work until someone restarts the process, where before only that loop's
  work stopped. Accepted and documented in the HA guide, with
  `self_release_after_secs: 0` as the remedy for such a deployment.
- **Shutdown.** On `cancel` the three gated loops exit, which reads as
  `Stopped` after start. `run_loop` does not start an iteration once
  `cancel` has fired (`!cancel.is_cancelled()` next to the `paused`
  check), and its `select!` then returns on `cancel`; so no iteration
  that begins after `cancel` reads the probe. An iteration that began
  before `cancel` can still read it after the loops exited. If that is the
  start of an unfit stretch, its unfit time is about 0, below every
  allowed `release_after` (≥ 1 s), and it pings or acquires as usual. If
  an unfit stretch was already running (a loop stalled before the
  shutdown), the stretch keeps its first sighting's time, and the
  iteration can release. Both are harmless: the shutdown drops the
  connection and clears the validity right after, and a release only does
  the same a moment earlier. The cost is at most one WARN line, which
  names a real stall when it is the release message.
- **Never-beaten loops.** `task_health(alive, None, _)` is `Ok`, and a
  `Stopped` loop that never beat is not unfit: a loop that hangs or panics
  before its first beat is invisible to the probe. The recovery and
  event-source loops beat first thing; the scheduler runs `load_triggers`
  first (`scheduler.rs:88–92`). It now beats once before that call (change
  6), so a hang there becomes `Stalled` after 300 s like any other.

The fitness probe is optional: `LeaderElection::start` without a probe
behaves exactly like § 4.2–4.3 (every existing test), and so does a timing
with `release_after = None` with a probe installed. "Off" is a property of
the leader loop only: it disables the fitness-based release and the
acquire suppression, not the health rules of § 4.4.1, which read the loop
flags directly and do not depend on `leader:` settings.

#### 4.4.1 Health contract: a loop that ran and then exited

`health.rs` changes two rules (spec 2026-09-17's split of the probes stays):

| Endpoint | Before | After |
|---|---|---|
| `/livez` | `stopped` (503) for a `Stopped` loop that started, **on the leader only** | `stopped` (503) for a `Stopped` loop that started, **on every replica** |
| `/healthz/detail` per loop | `stopped` for every `Stopped` loop on the leader, `follower` for every `Stopped` loop elsewhere | `stopped` when the loop is `Stopped` and (this replica is leader, or the loop started); `follower` only for a never-started loop on a follower |

`stalled`, `ok`, the never-started rule and the status codes are
unchanged.

- **Why every replica.** Followers run the loops too. A follower whose loop
  panicked cannot do that loop's work if it becomes leader, and with § 4.4
  it never becomes leader again; only a restart repairs it. The same
  reasoning already applies to `Stalled` ("a hung standby is no standby").
  Without this change, a leader that released on `Stopped` would become a
  follower with a green `/livez`: never restarted, never eligible again.
- **Shutdown.** The loops exit on `cancel`, so `/livez` reads `stopped` on
  every replica for the rest of a shutdown (seconds). Before, it could read
  `stopped` on the leader in the same window, until the leader loop cleared
  the validity. The kubelet does not act on it: for a pod that has a
  deletion timestamp, the prober stops the liveness and startup probes and
  records success (`pkg/kubelet/prober/worker.go`). Outside Kubernetes,
  nothing acts on `/livez`.
- **Existing tests stay unchanged and green:**
  `healthz_follower_reports_follower_not_503` (never started → `follower`),
  `livez_ok_when_tasks_fresh_or_never_started`,
  `livez_fails_when_started_task_exited_on_leader`,
  `healthz_leader_reports_stopped_when_scheduler_guard_dropped` (leader,
  never started → `stopped`).

### 4.5 Gated loops: unchanged

The scheduler, recovery sweeper and event-source manager keep reading
`is_leader()` once per iteration. Re-reading per unit was considered and
dropped:

- **Scheduler.** Its due batch was computed while this replica was leader;
  the other replicas advanced those triggers "as if fired" when they came
  due (`advance_state_after_tick`), so in the common case nobody else fires
  them. Skipping the rest of the batch after a loss would turn those fires
  into misses.
- **Recovery.** Aborting phase 1 after `mark_stale_inactive` committed would
  strand the dead worker's running steps: the next sweep only rediscovers
  workers that are still `active` (`worker.rs:90–109`), and steps without
  `timeout_secs` escape phase 2. Other phases (4.5, 4.6, retention) are
  multi-unit loops; a sound per-unit design needs every phase to be
  restartable, which is recovery work outside this spec.
- **Event source.** One reconcile is the unit; duplicates are cancelled by
  the next reconcile (existing behaviour).

The residual is analysed in § 5.2.2.

### 4.6 Diagnostics

Leader side (`leader.rs`; durations are the configured values, shown here
with the defaults):

- Start: INFO `LeaderElection started (lease=30s, validity=20s,
  retry=5s, ping_timeout=5s, acquire_timeout=10s, self-release after 30s)`,
  or `… self-release off`.
- Acquire: `Acquired leader lock (key=0x5354524d4c445201, backend pid=…,
  lease=30s)` or `… lease not enforced`.
- Loss, by cause:
  - ping returned SQLSTATE `57P05`: WARN `Leader lease expired: PostgreSQL
    ended the leader session after 30s without a ping
    (idle_session_timeout). Now a follower.` sqlx surfaces a received
    `ErrorResponse` as a database error; when the socket is already closed
    it surfaces an I/O error instead, which takes the generic path;
  - other ping error: WARN `Leader connection lost: {err:#}` (as today);
  - ping timeout: WARN `Leader ping timed out after 5s; dropping the leader
    connection`;
  - release on an unfit loop: § 4.4.

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
uses one role (prod: `stroem`). Hidden fields are logged as `unknown`. The
query is a read of two catalog views; it runs inside the attempt's
`acquire_timeout`, its failures are logged at DEBUG and never change the
acquire result.

The holder's client address is the old pod IP; in this incident it would
have named 172.31.73.52 in every follower's log from the first minute.

Metrics: none new. `stroem_leader_status` now reflects validity. The HA
guide documents the alert `sum(stroem_leader_status) == 0` for 5 min (the
gauge is only exported when `metrics.public` is `false`).

### 4.7 Unchanged

- The lock key `0x5354524D4C445201`. Bumping its version byte would let an
  old-version and a new-version replica both be leader during the rolling
  upgrade; the new contract is compatible with the old one (§ 7).
- `application_name = stroem-leader` (operators' holder queries and
  `leader_connection_lost_triggers_reflag_to_follower` select on it).
- `/healthz` semantics; `/livez` and `/healthz/detail` change only as
  § 4.4.1 says.

## 5. Failure analysis

### 5.1 Bounds and scenarios

Assumptions: the database answers the leader's millisecond statements in
milliseconds; the follower's runtime runs its retry timer roughly on time;
the network between the follower and the database works.

Numbers in § 5 use the default timing (N = 30 s, V = 20 s, retry and ping
timeout 5 s, attempt 10 s). For another lease, put the § 4.1 values into the
same formulas: for example the worst-case takeover is `N + 2 ×
acquire_timeout + retry_interval`, and the starved-runtime rows below read
"V to N" and "> N".

Takeover after a frozen leader. Let `i` be the moment the old leader's
backend last became idle (just after answering its last ping).

- Session ends: at `i + N` (PostgreSQL's timer, its own clock).
- Takeover completes: the follower's next attempt after `i + N` succeeds.
  An attempt starts every `retry_interval` after the previous one ended, and
  an attempt lasts at most `acquire_timeout`. So the takeover completes by
  `i + N + acquire_timeout (an attempt that started just before and was
  refused) + retry_interval + acquire_timeout` — **≤ 55 s** worst case,
  **~35 s** typical (attempts of milliseconds).
- Measured from the freeze, add the delivery delay of the leader's message
  in flight at the freeze, if any. The leader task has at most one message
  outstanding (it waits for each answer before the next ping), and a frozen
  process writes nothing new. A message that arrives only in part does not
  re-arm the timer (§ 4.2). A whole message that arrives late does: it
  restarts the idle interval at its arrival, so the session ends N after
  that. The delay is milliseconds on a working network; on a degraded one
  it is at most the client kernel's retransmission limit for unacknowledged
  data (Linux `tcp_retries2 = 15`, about 15.5 min), after which the client
  kernel abandons the connection and the message never arrives. So: ≤ 55 s
  after the freeze on a working network, ≤ ~16.5 min in the worst case of a
  degraded one (today: unbounded — 16.5 h in the incident). A per-socket
  `TCP_USER_TIMEOUT` would shorten the worst case; sqlx's `PgConnectOptions`
  does not expose it — accepted. The scheduler stops firing from the freeze.

| Scenario | Before | After |
|---|---|---|
| Leader process frozen (SIGSTOP, cgroup freeze, CPU starvation, node hang), kernel answers TCP (this incident) | lock held until the node dies (16.5 h) | session ended ≤ 30 s after its last idle start; takeover ≤ 55 s after it (from the freeze: § above). On thaw the ex-leader reads `is_leader() == false` (validity lapsed ≤ 20 s after its last ping was sent); its next ping fails; it stays follower |
| Network to the DB silently dropped (half-open: the DB host or an intermediary keeps the session) | DB closes by keepalive after ~6 min (if probes are not ACKed); ex-leader's ping hangs ~15 min with `is_leader()` true → minutes of overlap | ex-leader: ping times out at 5 s, validity cleared; session: ended by the lease 30 s after its last idle start; no overlap |
| Leader crawling: leader task pings < 30 s apart but a gated loop does not come round | lock held while the process lives | loop `Stalled` (scheduler: 300 s), then held for `release_after` → release (§ 4.4 timing) → follower acquires one attempt later |
| Leader crawling: pings < 30 s apart, loops beat within their thresholds | lock held, work slow | unchanged: slow but making progress is not a failure this design detects |
| Session ended early (administrator `pg_terminate_backend`, connection reset) | ex-leader detects on its next ping (≤ 5 s + an unbounded ping) | detected on the next failing ping; overlap ≤ 15 s, never more than V = 20 s (§ 5.2.1) |
| DB restart / failover | sessions die, ping errors, followers race to reacquire | unchanged (with the § 5.2.1 bound) |
| Healthy leader, its runtime starved 20–30 s | n/a | `is_leader()` false for the starved stretch, lock kept; next successful ping restores it. Scheduler triggers that came due in the stretch are advanced as by a follower (missed) |
| Healthy leader, runtime starved > 30 s | n/a | session ended; failover (the frozen-leader outcome) |
| PostgreSQL < 14 | as row 1 "before" | no lease: a frozen leader keeps the lock (WARN at startup); validity, ping timeout and self-release still apply |
| Gated loop panicked (`Stopped`) on the leader | lock kept, `/livez` fails, kubelet restarts if it can reach the node | release `release_after` after the probe sees it (§ 4.4 timing) → follower acquires one attempt later; the ex-leader stays follower and its `/livez` fails, so the kubelet restarts it |
| Gated loop panicked on a follower | `/livez` green; the replica stays up but cannot do that loop's work as leader | the replica does not acquire; `/livez` fails → kubelet restart (§ 4.4.1) |
| Self-release off (`self_release_after_secs: 0`) | n/a | no fitness-based release and no acquire suppression: in the two rows above and in the crawling row, the lock stays as "before". The health rules of § 4.4.1 still apply: a stopped loop fails `/livez` on every replica. Lease and validity unchanged |
| Stalled scheduler on a single replica | `/livez` restart | release after `release_after` (recovery and event source pause too), then the `/livez` restart |

### 5.2 Split brain

#### 5.2.1 Two replicas reporting `is_leader() == true`

**Takeover by lease expiry: no overlap.** Let the old leader send its last
successful query at local monotonic time `s` (read before sending).
PostgreSQL receives it at real time `≥ s`, answers, and becomes idle at
`i ≥ s`. If no later query arrives, the session ends at `i + N`; another
replica can acquire only after that. The old leader reads `true` only while
its monotonic clock is below `s + V`. Monotonic clocks on two hosts advance
at the same rate within NTP's slew limit (500 ppm, 15 ms over 30 s), so in
real time the old leader's validity ends before `s + V + 0.015 s < s + N ≤
i + N`. The 10 s cushion is three orders of magnitude larger than the drift.

**Session ended early: bounded overlap.** The lease bounds the session's
lifetime from above; nothing stops it ending sooner — `pg_terminate_backend`
by an administrator, a connection reset, a database restart. A follower can
then acquire within one attempt while the old leader still holds a validity
deadline up to V in the future. The old leader learns of the loss only on
its next ping (sqlx delivers nothing between queries). Let `E` be the
moment the session ended. Overlap:

- **hard cap V (20 s), in every case.** A query sent after `E` cannot
  succeed, so the newest validity deadline comes from a query sent before
  `E`: `valid_until ≤ E + V`. This covers a late successful acquire answer
  too (its deadline is the lock query's send time + V).
- **leader task running: ≤ `2 × ping_timeout + retry_interval` =
  min(N/2, 15 s) (§ 4.1 caps; 15 s with the default lease).** The
  answer to a ping sent before `E` can still arrive, successful, up to
  `ping_timeout` after `E`; the next ping is sent `retry_interval` later and
  fails or times out within `ping_timeout`, clearing validity. With a prompt
  network it is ≈ `retry_interval` (5 s).

Today the same overlap exists, ≤ `retry_interval` plus an unbounded ping.
Closing it would need the leader to learn of the session's end without
sending a query (a pending read on the socket), which sqlx's `PgConnection`
does not offer; accepted. Tests pin both the prompt and the delayed-answer
bound (§ 6.3, tests 5 and 6).

Requirement on the clock: it must advance while the process is frozen.
Linux `CLOCK_MONOTONIC` (Rust's `Instant`) advances during SIGSTOP, cgroup
freezes, CPU starvation and hypervisor steal; it does not advance across a
system suspend (`CLOCK_BOOTTIME` does). Server nodes do not suspend;
accepted. After a suspend the TCP sessions are dead, so the first ping fails
within `ping_timeout` — the exposure is the gated loops that run in those
seconds.

#### 5.2.2 Work that started while the replica was leader

`is_leader()` is read, then the work happens. Work in flight when leadership
ends continues after it. Two kinds of handover:

- **Lease expiry (frozen leader).** A unit that read `true` at the last valid
  moment `s + V` can overlap a new leader only if it is still running at
  `s + N`: the 10 s cushion, in practice a freeze inside the unit longer than
  that.
- **Immediate release** — shutdown (`cancel` drops the connection while the
  loops still run, today), self-release (§ 4.4), early session end
  (§ 5.2.1). No cushion: a follower can acquire within one attempt while the
  old leader's in-flight unit runs on.

Per loop:

- **Scheduler.** The old leader's in-flight batch holds occurrences that
  came due while it was leader. A follower fires one of them only if it
  became leader BEFORE its own scheduler processed that due time — both
  schedulers sleep until the trigger's `next_run`, so normally within
  milliseconds of each other, and the follower's acquire attempt must land
  inside that gap. Then the occurrence fires twice (with `ConcurrencyPolicy::
  Allow`; `skip` and `cancel_previous` see the first job). The window is
  per handover and narrow; durable occurrence deduplication is out of scope
  (non-goals) and shares its design with missed-fire replay. Accepted, not
  tested: the race needs a scheduler-internal hook this change does not add,
  and the shutdown handover has had the same window since leader election
  exists.
- **Recovery.** One whole in-flight sweep can run alongside the new
  leader's. Phases 1 and 2 are guarded by the observed claim
  (`claimed_step_failed`); a duplicate failure in phase 2.5 or 4
  (`expected = []`) can consume one extra retry of one step; phase 3
  (cancel), 4.5/4.6 (`advance`) and retention (deletes) are idempotent or
  rediscovered. Accepted.
- **Event source.** One in-flight reconcile can create duplicate consumer
  jobs; the new leader's next reconcile cancels them (existing behaviour).

#### 5.2.3 Not done: write-level fencing

A fence makes every leader-gated write fail unless the writer is still the
leader at commit time. Mechanisms that are fences:

- run the gated writes on the lock-holding session itself — a commit on that
  session proves the session, and so the lock, existed at commit;
- an epoch row: each new leader increments it on acquire; every gated
  transaction reads it `FOR SHARE` and aborts on a mismatch — the increment
  blocks behind in-flight gated transactions, so no old-epoch write commits
  after a new leader's increment.

Checking in the write transaction that the old leader's pid still holds the
lock in `pg_locks` is NOT a fence (the session can end between the check and
the commit). Every gated path (job creation, settlement, retention) would
need the session or the epoch check. Out of scope; § 5.2.2's residual is
small per handover.

### 5.3 Missed fires during a handover

Followers advance triggers without firing. A trigger that comes due between
the moment the old leader stops firing and the moment the new leader
acquires is never fired by anyone: after a frozen leader, from the freeze to
the takeover (§ 5.1: ≤ 55 s on a working network); after a stalled or
stopped scheduler, the stall (or the exit) plus `release_after` plus one
attempt. Today: the whole outage (16.5 h). Replay
stays an open TODO item.

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

- `leader.rs`: default `LeaderTiming` satisfies the § 4.1 invariants, and
  the test timings below do too; `from_lease`: `check` passes for every
  whole second from 10 s to 3600 s, `from_lease(30 s, Some(30 s))` equals
  revision 5's values, the values at 10 s (1666 / 1666 / 3333 / 6666 ms)
  and at 3600 s (5 s / 5 s / 10 s / 2400 s, the caps), and validity
  7333 ms at 11 s (the floor of 2L/3); validity arithmetic
  (`is_leader` false at/after `valid_until`, `always()`, `never()`);
  `next_action` table of § 4.4 incl. the `release_after` boundary; the
  "off" rule: with `release_after = None` the probe is not called (a probe
  that counts its calls stays at 0) and the result is `None`;
  `track_fitness` keeps the first sighting's time when the named loop or
  fault changes; the `UnfitLoop` wording (`is stalled` / `has stopped`);
  `lease_setting(Duration)` → `"30000ms"`; SQLSTATE classification
  (`42704` → `Unsupported`, other codes → error; `57P05` → lease-expired
  wording).
- `config.rs`: no `leader:` section → defaults 30 / 30; YAML values are
  read; env overrides `STROEM__LEADER__LEASE_SECS` and
  `STROEM__LEADER__SELF_RELEASE_AFTER_SECS` (strings) are coerced; an
  unknown field is rejected; `validate` rejects `lease_secs` 9 and 3601 and
  `self_release_after_secs` 3601, accepts 10, 3600 and 0; `timing()` maps
  `self_release_after_secs: 0` to `release_after = None`.
- `health.rs`: `unfit_loop` — a backdated scheduler beat yields `scheduler`
  `Stalled`; a recovery loop that beat and then lost its guard yields
  `recovery` `Stopped`; fresh beats and a loop that never beat (guard not
  held) yield `None`.

### 6.3 Integration tests (`crates/stroem-server/tests/ha_test.rs`)

All `#[serial]` with the existing leader tests. Test timing through
`start_with`: `retry_interval` 200 ms, `ping_timeout` 500 ms,
`acquire_timeout` 1.5 s, `validity` 1.5 s, `lease` 2.5 s, `release_after`
600 ms (invariants: 2 × 0.5 + 0.2 = 1.2 < 1.5 < 2.5; 2.5 ≥ 0.6). Test-only
tools:

- `LeaderElection::pause_for_test(&self, paused: bool)`
  (`#[doc(hidden)] pub`): the leader loop skips its iterations — no ping, no
  acquire — which is what PostgreSQL sees from a frozen process with a
  working network.
- A **message-framing TCP proxy** in the test file (tokio, one task per
  direction) between one replica and PostgreSQL. The proxied replica
  connects with `sslmode=disable`, so the stream is plain protocol: the
  client's first message is the untyped StartupMessage (int32 length), every
  later message in both directions is typed (1-byte type + int32 length).
  The proxy reads whole messages (TCP read chunks carry no write
  boundaries) and acts per message:
  - **freeze** (both directions): stop forwarding and keep both sockets
    open, also when the client side closes — what the incident's node did;
  - **hold requests**: hold the next client `Q` message until the test
    releases it, and record when it was forwarded;
  - **delay answers by `d`**: hold the server's messages and release them
    together `d` after the `Z` (ReadyForQuery) that ends an answer arrives;
    an `E` + end of stream (a FATAL) is released `d` after it arrives;
  - **cut the next `Q` after `k` bytes**: forward the first `k` bytes of
    the next client `Q` message (`k` is asserted to be inside it:
    `0 < k < 1 + length`), then freeze the client→server direction.

  Barriers (`tokio::sync::Notify` / watch counters the test awaits):
  "a `Q` is held", "a held `Q` was forwarded (at time t)", "an answer
  ending in `Z` is held in the answer delay", "a held answer was released
  (at time t)", "a `Q` was cut after `k` of its `n` bytes". Tests act
  only after the barrier, so the case is exercised, not just likely.

Tests:

1. `frozen_leader_loses_lock_to_follower_after_lease` (pause hook): A leads,
   B follows; pause A; A reads `false` within V + 0.5 s; B acquires within
   N + 2 × `acquire_timeout` + `retry_interval` + 1 s; sampled every 20 ms,
   A and B are never both `true`. Then resume A: for 2 s A reads `false` at
   every sample and B `true` (thawed ex-leader).
2. `partitioned_leader_loses_lock_to_follower_after_lease` (proxy freeze,
   both directions): A through the proxy, B direct. A's ping times out and
   A reads `false` within `ping_timeout` + `retry_interval` + 0.5 s; A's
   backend stays alive (A's dropped socket does not pass the frozen proxy)
   until the lease ends it; B acquires within the test-1 bound; never both
   `true`.
3. `partial_ping_message_does_not_stop_the_lease` (proxy cut): A through
   the proxy, B direct; arm "cut the next `Q` after 5 bytes" and wait for
   the cut barrier (A's ping `Q` of 14 bytes was cut after 5). The backend
   waits mid-message with the idle timer armed: B acquires within N + 2 ×
   `acquire_timeout` + `retry_interval` + 1 s of the cut. This is the message-aware
   check of § 4.2's protocol rule; a variant with the extended protocol
   (Bind/Execute delivered, Sync withheld) is not part of the suite —
   `raw_sql` is the guard, and the variant would only demonstrate
   PostgreSQL's behaviour, which § 4.2 cites.
4. `healthy_leader_keeps_lock_past_lease`: A leads for 3 × N; the holder
   pid (from `lock_holder`) never changes; B never reads `true`.
5. `early_session_end_overlap_is_bounded`: A (validity 4 s, lease 5 s, so
   a lapse cannot pass the test) leads, B follows; `pg_terminate_backend`
   A's backend (pid from `lock_holder`); A reads `false` no later than
   `retry_interval + ping_timeout` + 0.5 s after the termination; then
   exactly one of A and B leads (A is healthy and may win the election).
6. `early_session_end_with_late_answers_is_bounded` (answer delay `d` =
   350 ms < `ping_timeout`; A with validity 4 s): A leads through the
   proxy, its pings still succeed; wait for the barrier "an answer ending
   in `Z` is held", then terminate A's backend; the barrier "a held answer
   was released" must come after the termination. A reads `false` no later
   than `2 × ping_timeout + retry_interval` + 0.5 s after the termination
   (before its validity could lapse); then exactly one of A and B leads.
7. `delayed_ping_extends_the_session_by_one_lease_only` (hold requests):
   wait for the barrier "a `Q` is held", pause A, keep the ping held
   300 ms more (below `ping_timeout`, so A's ping still succeeds), release
   it and read its forward time `f` (after the pause). B does not lead
   before `f + N − 0.15 s` — the late ping restarted the idle interval —
   and leads within N + 2 × `acquire_timeout` + `retry_interval` + 1 s of
   `f`: the § 5.1 bound from the freeze with a delivery delay.
8. `lease_setup_maps_unrecognized_parameter_to_unsupported`: `apply_lease`
   with a non-existent parameter name on PostgreSQL 15 returns
   `Unsupported` — the real server error code; with `idle_session_timeout`
   it returns `Enforced` and `SHOW idle_session_timeout` on that connection
   reads the value set.
9. `stalled_loop_releases_lock_after_persistence_and_blocks_reacquire`: A
   with a probe backed by an `AtomicBool`; set it → A keeps the lock for at
   least `release_after`, then releases; B acquires; while the flag is set A
   never reads `true`, even after B is cancelled; clear it → A acquires.
   A flag set for less than `release_after` and then cleared → no release.
10. `follower_reports_lock_holder`: `lock_holder` on another connection
    returns A's backend pid with `application_name = stroem-leader`.
11. Scheduler startup beat: not covered by a test. `load_triggers` reads
    the published in-memory snapshots (`WorkspaceManager::get_config`) and
    cannot be made to block without a test-only hook in the workspace
    manager; the change is one statement before that call, checked in
    review.

12. `stopped_loop_releases_lock_through_the_health_probe`: an `AppState`
    whose leader is A (`fast()` timing) and whose loops are marked alive
    and beaten; A gets `health::leader_fitness_probe(&state)` — the real
    probe, not a flag; B follows. Drop the scheduler's alive flag (the
    loop started, so it is `Stopped`). A releases within `release_after +
    2 × (retry_interval + ping_timeout)` + 1 s; B acquires; never both
    `true`. With B cancelled, A does not acquire for 1.5 s, and `/livez` on
    A's router answers 503 `stopped` while A is a follower.
13. `self_release_off_ignores_an_unfit_loop`: A with `fast()` timing but
    `release_after = None`, and a probe that always returns a `Stopped`
    loop; B follows. For 3 × `fast().release_after` after A leads, A stays
    leader at every sample and B never reads `true`.
14. `livez_fails_when_started_task_exited_on_follower` (router test, like
    `livez_fails_for_stalled_loop_on_follower`): a follower whose loops are
    alive and beaten, then the recovery flag dropped: `/livez` 503
    `{"status": "stopped"}`; `/healthz/detail` `recovery` = `stopped`,
    `scheduler` = `ok`. The same assertions hold for a state whose config
    has `leader.self_release_after_secs: 0` ("off" does not change the
    health rules).

Existing tests keep the default timing and must pass unchanged, except
that test 9's probe returns an `UnfitLoop`.

## 7. Rollout

- No migration, no API change. Lock key unchanged. One new optional
  configuration section, `leader:` (§ 4.1); without it, every value is
  revision 5's.
- Behaviour change: a replica whose gated loop panicked fails `/livez`
  even as a follower, so its liveness probe restarts it (§ 4.4.1).
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
  validity, PostgreSQL ≥ 14, self-release (stalled and stopped loops), the
  `leader:` settings with the derived values and what the lease trades,
  the single-replica note without liveness probes, the `/livez` change for
  followers, the frozen-node failure mode,
  holder logs, Karpenter `NodeRepair`, the alert, RDS keepalives,
  missed-fire replay and duplicate fires still open.
- `docs/src/content/docs/getting-started/configuration.md`: the `leader:`
  section and its env overrides.
- `CLAUDE.md` § High Availability: the lease, validity, self-release,
  the settings, diagnostics, test image; § Health Check: the `Stopped`
  rule of § 4.4.1.
- `CONTEXT.md`: **Leader lease**, **Leader validity**, **Self-release**
  (stalled or stopped).
- `docs/internal/TODO.md`: the incident under Bugs, `[x]` with this fix;
  missed-fire replay stays open; open items for write-level fencing and
  durable occurrence deduplication; the `Stopped` self-release item `[x]`.

## 9. Decisions (user, 2026-10-20)

- Q1. Self-release (§ 4.4): keep it.
- Q2. Timing: configurable, with two settings, `leader.lease_secs` and
  `leader.self_release_after_secs` (`0` = off); the other values are
  derived from the lease, with caps (§ 4.1; revision 7 removed the caps,
  revision 8 restored them).
- Q3. A loop that ran and then exited (`Stopped`): covered in this change,
  with the `/livez` change on every replica (§ 4.4, § 4.4.1).
