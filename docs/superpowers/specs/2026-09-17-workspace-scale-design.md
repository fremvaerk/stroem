# Workspace Scale (50+ Repos) — Design

Status: revision 6 — **§ 4 APPROVED WITH NITS** (Codex round 6, 2026-09-18;
nit applied, Appendix B), as scoped by § 4.10; §§ 3, 6, 7 are problem statements
and are **not** approved
Ships in: unscheduled
Coordinates with: `2026-09-16-task-step-lifecycle-hardening-design.md` — § 4.7's
availability transitions overlap that spec's "re-advance jobs after workspace
recovery" proposal (lines 219–220). Not independent.
Relates to: `docs/internal/TODO.md:1006` (the 2026-09-16 scheduler hang — root
cause still **unknown**; nothing in this spec claims to fix it)

Triggered by the question "will Strøm work fine with 50 repos?". Line numbers
cite `main` at `663b9d4`; library line numbers cite the pinned `git2` **0.21.0**
(`Cargo.lock`), its bundled `libgit2-sys` 0.18.5 (libgit2 1.9.4), and
`tokio` 1.52.3. Production measurements are reproduced verbatim in
**Appendix A** with the commands that produced them; every number derived from
them is labelled **measured** or **hypothesis**.

Revisions 2–6 respond to Codex design-gate reviews (2026-09-17/18, thread
`01a0afcd`). **Appendix B** records every finding's disposition.

## 1. Answer

Not on the current deployment envelope. The architecture scales acceptably;
the **operating envelope does not**, and two code paths that are merely
wasteful at 9 workspaces become load-bearing at 50.

| # | Failure | Kind | Confidence |
|---|---|---|---|
| 1 | Startup exceeds the deployed 300 s `startupProbe` budget | config | **measured** basis, **hypothesis** for 50 |
| 2 | Server OOMKilled | config | **measured** pressure, **hypothesis** for threshold |
| 3 | Runtime has one worker thread while git work blocks it | config + code | **measured** (`cpu.max`), causal link to the hang **unproven** |
| 4 | One remote failure fans out into N blocking full fetches | code | **measured** in code, N=8 observed in the incident |
| 5 | Worker revision cache never evicts on the normal path | code | **measured** (call-site analysis + prod `du`) |
| 6 | Tarball rebuilt + re-gzipped per request, no single-flight | code | **measured** in code |
| 7 | O(N) per-request workspace fan-outs | code | **measured** p99; not currently a bottleneck |

The ordering is a ranking of severity, not a schedule of when each bites.

## 2. Evidence

Measured 2026-09-17 on the `tools` cluster, `stroem-server` **v0.16.3**,
2 replicas, 9 git workspaces. Raw captures in Appendix A.

| Signal | Measured | Appendix |
|---|---|---|
| Workspace load at boot | **51.1 s** for 9 workspaces; listener binds 71 ms later | A.1 |
| Deployed `startupProbe` | `failureThreshold: 30` × `periodSeconds: 10` = 300 s | A.2 |
| Deployed liveness probe | `/healthz` — **not** `/livez` | A.2 |
| `memory.current` / `memory.max` | 509,923,328 / 536,870,912 B (**95 %**) | A.3 |
| `memory.stat` | anon 294 MiB, file 179 MiB, slab 11 MiB | A.3 |
| `memory.events` | `max 81`, `oom 0`, `oom_kill 0` | A.3 |
| `cpu.max` | `50000 100000` = 0.5 CPU | A.3 |
| Threads | 8 total, 7 named `tokio-rt-worker`; `nproc` reports 2 | A.3 |
| Server git clones | 455 MB; largest single repo 229 MB | A.4 |
| Worker revision cache | 2.3 GB / 6 workspaces; 1.6 GB is 3 revisions of one repo | A.5 |
| `/api/jobs/{id}` | 217 requests, p99 < 25 ms, max < 50 ms | A.6 |

Two caveats on what these support:

- **The deployed envelope is not the chart default.** `helm/stroem/values.yaml`
  ships `resources: {}` and `startupProbe: {}` (`:92`, `:156`); the numbers
  above come from the live deployment, and the liveness path shows prod is on a
  chart older than the `/livez` work. Sizing guidance must be written against
  the chart, and the chart must grow non-empty defaults.
- **51.1 s is elapsed time, not CPU time.** It includes network fetch and
  subprocess waits, so it does **not** by itself establish CPU-bound scaling.

**Hypothesis (to verify, not assume):** at 50 workspaces, startup ≈ 51 s ×
50/9 ≈ 4.7 min; server clones ≈ 2.5 GB; `/api/jobs/{id}` p99 ≈ 100–125 ms.
Step 1 of § 10 is a *measurement*; every later step is gated on its result.

## 3. Startup blocks the listener — NOT YET DESIGNED

`main.rs:96-104` awaits `WorkspaceManager::new` before the axum listener binds.
Inside, sources load on `tokio::spawn`ed tasks through a `JoinSet`, bounded by
`MAX_CONCURRENT_WORKSPACE_LOADS = 8` (`workspace/mod.rs:25`, `:264-279`). The
`JoinSet` is deliberate and correct for today's `block_in_place`-based
`GitSource::load` (`git.rs:233`; `workspace/mod.rs:252-263` explains why
`join_all` would serialize it). § 4.5 (0) replaces `block_in_place` with
`spawn_blocking` but keeps this orchestration as it is (§ 4.5 (9)).

Per-workspace cost is not only git: `load_workspace`
(`stroem-common/src/workspace_loader.rs:154-178`) scans the repo — decrypting
every `*.sops.yaml` through a **synchronous `sops -d` subprocess**
(`sops.rs:14`, via `workspace_loader.rs:72`) — then `render_secrets()` and
`render_connections()` render through Tera, where the `vals` filter
(`template.rs:30-51`) spawns **another synchronous subprocess** per `ref+`
secret. Neither has a timeout.

### Options

1. **Raise the CPU limit** (`limits.cpu: 2`, `requests.cpu: 1`). One line. How
   much it helps is exactly what § 10 step 1 measures.
2. **Bind the listener first, load in the background.** Requires an explicit
   readiness change, **not** just the existing probe split: `/healthz`
   (`web/health.rs:116`) checks database reachability only, so binding early
   with today's handlers would report ready while workspaces are still empty.
   Needs a "first load pass complete" readiness condition; a cold-start answer
   distinguishable from the transient-outage case (`classify_execute_error`
   maps `"is not available"` to 500, and the scheduler's "unavailable ≠
   removed" rule would read a cold start as an outage); rules preventing a
   newly elected, not-yet-loaded leader from cancelling live event-source
   consumers (`event_source.rs:234`); and — from round 4 — a policy for initial
   loads that are saturated, pending or hung (§ 4.10 (4)).
3. **Raise the `startupProbe` budget.** Treats the symptom; the replica is out
   of service for the whole load on every rollout, under `maxUnavailable: 0`.

Option 1 is § 10 step 1. Option 2 needs its own design round.

## 4. Peek failure amplifies into a fetch stampede — PROPOSED FOR APPROVAL

### 4.1 The defect

`start_watchers` (`workspace/mod.rs:767-784`) spawns one task per workspace,
each creating `tokio::time::interval(poll_secs)` at the same moment. Ticks are
**initially clustered, with no deliberate jitter**; they drift apart over time
and the period is configurable per workspace.

`GitSource::peek_revision` (`git.rs:251-266`) is built entirely from `.ok()?`:
network error, auth rejection, missing clone, corrupt local repository, missing
`origin` and missing ref are indistinguishable, and all return `None`. The
watcher reads:

```rust
// workspace/mod.rs:835
if current_revision == last_revision && current_revision.is_some() {
    continue;
}
```

`None` fails both conjuncts, so **a peek failure falls through to a full
`source.load()`** — blocking libgit2 fetch + `reset --hard` + YAML re-parse +
`sops` and `vals` subprocesses. One remote blip converts N cheap `ls-remote`s
into N simultaneous blocking loads on a runtime with one worker thread. Eight
such loads overlapped during the 2026-09-16 incident.

### 4.2 Decision

**A peek failure means "I don't know", not "it changed": skip the tick and keep
the last successfully loaded config in memory.**

**This is a new policy, not an application of an existing one.** The
"unavailable ≠ removed" rule governs what the *scheduler* retains after a
**reload failure** (`scheduler.rs:175`; `:368` still logs `MISSED` and declines
to fire), and CLAUDE.md:174 records that last-good *config/files serving* after
a failed reload was designed and deliberately **reverted**. This decision does
not revisit that. It rests on narrower ground, which differs by source:

- **Git.** A peek is **non-mutating and non-authoritative**: it reads only the
  remote's ref advertisement and writes nothing. Its failure is evidence about
  the *network*, not the workspace. Declining to act on it changes no committed
  state, because the config in memory is one we successfully loaded and have
  served continuously.
- **Folder.** A traversal or read error is local-file evidence, not network
  evidence, so the git rationale does not carry over. The folder policy rests
  on a weaker, separate argument: the most common cause is a concurrent write
  (a deploy `rsync`, an editor's atomic rename) observed mid-flight, which is
  also genuinely "I don't know". Staleness is bounded by the same K escalation
  (§ 4.4), after which a real load decides.

### 4.3 The `Peek` contract

```rust
enum Peek {
    /// Remote/tree answered authoritatively. Compare against the PUBLISHED revision.
    Revision(String),
    /// This source cannot peek. Caller must do a full load (today's behaviour).
    Unsupported,
    /// Could not determine the current state. Skip; keep last loaded config.
    Failed(anyhow::Error),
    /// Local state is unusable. A full load is required; skipping cannot fix it.
    LocalInvalid(anyhow::Error),
}
fn peek_revision(&self, budget: &LoadBudget) -> Peek; // synchronous; run via spawn_blocking
```

Every case that returns `None` today:

| Source | Condition | Today | `Peek` |
|---|---|---|---|
| Git | clone dir missing (`git.rs:252`) | `None` → full load | `LocalInvalid` |
| Git | `Repository::open` fails — corrupt checkout (`:255`) | `None` → full load | `LocalInvalid` |
| Git | `find_remote("origin")` fails (`:256`) | `None` → full load | `LocalInvalid` |
| Git | `connect_auth` fails — DNS/TCP/TLS/SSH/auth (`:258-260`) | `None` → full load | **`Failed`** |
| Git | `connection.list()` fails mid-advertisement (`:261`) | `None` → full load | **`Failed`** |
| Git | ref absent from advertisement (`:263-265`) | `None` → full load | **`Failed`** — may be mid force-push; escalates via K |
| Git | read timeout fires (§ 4.5) | — (new) | **`Failed`** |
| any | peek worker panics (`mod.rs:831`) | logged, `None` | **`Failed`** |
| any | peek exceeds budget P (§ 4.5 (7)) | — (new) | `PeekTimedOut` event, treated as **`Failed`** |
| Folder | root missing (`folder.rs:30`) | `None` → full load | `LocalInvalid` |
| Folder | traversal error (`folder.rs:41`) | silently skipped; partial hash returned | **`Failed`** |
| Folder | file read error (`folder.rs:54-57`) | error string hashed in | **`Failed`** |

`LocalInvalid` and `Unsupported` preserve today's immediate full load (which
fails and enters backoff if the local state really is broken). Only `Failed`
skips. The classification is a pure function and is unit-tested row by row.

### 4.4 State and transitions

Per-entry state is split three ways, by how long each piece is held:

| State | Type | Held | Purpose |
|---|---|---|---|
| **Execution mutex** | existing `reload_state` (`tokio::sync::Mutex`), acquired as `OwnedMutexGuard` | for a whole load; a watcher load holds it **until real completion** (§ 4.5 (1)) | serializes loads on one checkout. **Nobody waits on it** — every acquirer uses `try_lock` (§§ 4.5 (2), (8)). |
| **Availability** | `std::sync::Mutex<Availability>` | microseconds; never across `.await` or I/O | watcher bookkeeping: freshness, backoff, in-flight records |
| **Published snapshot** | `std::sync::RwLock<Arc<Published>>` | only to clone or swap the `Arc` | what readers serve: `{ config, revision, warnings, error: Option<String> }` |

`get_config`, `get_path`, `get_revision` and `is_healthy` read **only** the
published snapshot. They never touch the execution mutex or `Availability`, so
a load that holds the mutex for an hour does not delay a single read.
`load_error` is replaced by `Published.error`.

```rust
struct Availability {
    freshness: Freshness,
    load_in_flight: Option<InFlight>,
    peek_in_flight: Option<InFlight>,
}
enum Freshness {
    Fresh { consecutive_peek_failures: u32 },   // saturates at K
    Errored { backoff: Duration, next_attempt: Instant },
}
struct InFlight { op_id: u64, started_at: Instant, deadline: Instant }
```

**One writer.** Every write to `Availability` — freshness *and* in-flight
records — goes through the pure function
`transition(&mut Availability, Event) -> Effects`:

```rust
enum Event {
    Tick { now: Instant },
    PeekStarted   { op_id: u64, deadline: Instant },           // observer, on admission
    PeekCompleted { peek: Peek },                               // observer, within P
    PeekTimedOut,                                               // observer, P expired
    PeekFinished  { op_id: u64 },                               // finalizer, always — incl. panic
    LoadStarted   { op_id: u64, deadline: Instant },           // observer, on admission
    LoadCompleted { op_id: u64, caller: Caller, ok: bool, completed_at: Instant }, // finalizer
    StartupFailed { now: Instant },
}
```

Responsibilities are disjoint: the **observer** applies peek *outcomes*
(`PeekCompleted`/`PeekTimedOut`) and records starts; the **finalizer** (§ 4.5
(1)) clears in-flight records (`PeekFinished`, `LoadCompleted`) and applies load
outcomes. `PeekFinished` and `LoadCompleted` clear their record **only if the
`op_id` matches**, so a stale completion can never clear a newer operation.

`apply_load_result` is the load-completion path: it applies `LoadCompleted`
and, on success, swaps the published snapshot (on failure, sets
`Published.error`), inside one short critical section. **Every** load path
reaches it — watcher, `reload`, `reload_for_api`, peer
`stroem_workspace_reloaded`, scheduler and webhook `force_refresh`, and startup.
External callers bypass the *retry policy*; they never bypass the *state
update*. This is the round-2 recovery fix: an external failure while `Fresh`
lands in `Errored`, where the watcher's backoff retries it.

**Tick transitions (watcher only):**

| Freshness | Observation | Next | Effect |
|---|---|---|---|
| `Fresh{n}` | `Revision(r)`, `r ==` published | `Fresh{0}` | none — also clears a pending forced probe (the remote recovered) |
| `Fresh{n}` | `Revision(r)`, `r !=` published | — | attempt admission (§ 4.5 (2)) |
| `Fresh{n}` | `Unsupported` / `LocalInvalid` | — | attempt admission |
| `Fresh{n}` | `Failed` / `PeekTimedOut`, `n + 1 < K` | `Fresh{n+1}` | skip; `warn!` only on 0 → 1 |
| `Fresh{n}` | `Failed` / `PeekTimedOut`, `n + 1 >= K` | `Fresh{K}` | attempt admission — the **forced probe**. The counter saturates at K; it is **not** reset here |
| `Fresh{n}` | `peek_in_flight` still set at tick (its observer timed out) | as for `PeekTimedOut` | no new peek starts — a peek hung forever still escalates after K ticks |
| `Errored{..}` | now `<` `next_attempt` | unchanged | skip; no peek, no load |
| `Errored{..}` | now `>=` `next_attempt` | — | attempt admission, **without** peeking — an errored workspace serves no config, so peek protects nothing |

**Admission outcomes:**

| Admission | Transition |
|---|---|
| admitted | `LoadStarted`: records `load_in_flight`; if `Fresh`, counter → 0. The reset happens **only when a load is actually admitted**, so the load's outcome alone decides what follows |
| `Busy` / `Saturated` | **none**. At `Fresh{K}` the forced probe therefore stays pending: the next failing tick re-attempts admission; a matching peek clears it |

Revision 4 reset the counter *before* admission while also saying a rejected
admission makes no transition — contradictory at `n = K−1`, and it lost the
probe. Saturating at K and resetting on `LoadStarted` removes the contradiction.

**Load-completion transitions** (`LoadCompleted`, from the finalizer). Keyed on
freshness *at completion*; exactly one row matches; delays run from
`completed_at`:

| Freshness at completion | Outcome | Caller | Next |
|---|---|---|---|
| any | `Ok` | any | `Fresh{0}`; publish config + revision + warnings; clear `error` |
| `Fresh{..}` | `Err` | any **except startup** | `Errored{ b = poll_interval, next = completed_at + b }` |
| — | `Err` | **startup** (`StartupFailed`) | `Errored{ b = poll_interval, next = now }` — the first watcher tick retries immediately, preserving today's "retry failed load at start" |
| `Errored{b, ..}` | `Err` | watcher | `Errored{ b' = min(2b, 15 min), next = completed_at + b' }` |
| `Errored{b, t}` | `Err` | external | `Errored{b, t}` — unchanged; external calls neither advance nor reset the ladder |

A panicking load is `Err` (§ 4.5 (1)).

**Parameters.** K = 5. At the 60 s git default that is ~5 minutes of skipping
before a forced probe — **longer** than the ~60 s outage of 2026-09-16, which is
what lets an outage of that length pass with no *watcher-initiated* loads at
all. External paths are unaffected and can still load during it. The cost is
that a real push made during a peek outage is picked up up to ~5 minutes late;
accepted.

- **Missed ticks:** `MissedTickBehavior::Delay` — no catch-up burst.
- **Jitter:** initial offset uniform in `[0, poll_secs)` per workspace.
- "Published revision" is `Published.revision` (§ 4.6), not today's
  watcher-local `last_revision` initialised from `source.revision()` (`:797`).

### 4.5 Deadline and cancellation contract

An async `timeout` cannot interrupt non-yielding blocking work, and
`spawn_blocking` does not make work cancellable. What the pinned libraries
actually offer, what Strøm wires today, and what this change guarantees:

| Phase | Available | Wired today | Guarantee after this change |
|---|---|---|---|
| TCP connect | `opts::set_server_connect_timeout_in_milliseconds` (`opts.rs:421`) — process-wide, `unsafe`, set before any libgit2 thread; applied at `streams/socket.c:224` | no | bounded per connect attempt |
| DNS resolution | nothing | — | **not bounded** |
| TLS/SSH handshake, ref advertisement, every socket read | `opts::set_server_timeout_in_milliseconds` (`opts.rs:460`); SSH session timeout set at `transports/ssh_libssh2.c:545` | no | each *read* bounded; total time **not** — a slow-drip remote resets it per read |
| ref-advertisement callback | none — the smart-protocol ref reader never invokes `sideband_progress` | — | covered only by the read timeout |
| object transfer | `transfer_progress` → `false` aborts; cooperative, fires only *between* reads | no | total deadline, cooperative; a blocked read is bounded by the read timeout |
| checkout **planning** | `CheckoutBuilder::notify` → `false` cancels; fires only inside `checkout_get_actions` (`checkout.c:2636`), before any file is touched. **Requires `notify_on(..)`** — notification types default to none (`build.rs:478`). `Repository::reset` accepts the builder (`repo.rs:782`) | no — `reset(.., None)` (`git.rs:88`) | cancellable **before** the first write |
| checkout **write phase** | none — removals, blob writes and submodule updates (`checkout.c:2651` ff., `:1886`) report progress through a callback that cannot cancel | no | **not interruptible**: once writing starts, the **entire remaining write phase** runs to completion |
| YAML scan | filesystem I/O; deadline check per file | no | cooperative between files; one blocked read is **not** interruptible |
| `sops -d` (`sops.rs:14`) | spawn + timed wait + `kill` + reap | no — `.output()` | **bounded** |
| `vals eval` (`template.rs:30-51`) | spawn + timed wait + `kill` + reap | no — `wait_with_output()` | **bounded** |

The two global timeouts also bound `peek_revision`. They are set once in
`main.rs`, before `WorkspaceManager::new` and before library resolution.

**These cooperative bounds apply to every load path** — watcher, external and
startup all pass a `LoadBudget` with budget L. Only the *watchdog* (below) is
watcher-specific.

**Deadline expiry aborts the whole load.** `scan_and_merge_yaml_files` today
turns a per-file error into a *warning* and continues; a deadline surfacing that
way would publish an incomplete config. Expiry is a distinct error type
(`DeadlineExceeded`) that the per-file handler re-raises, never downgrades.

**Stated residual risk:** DNS; a slow-drip remote; a single blocked filesystem
read; the entire checkout write phase once it has begun. These remain
uninterruptible. The watchdog exists precisely because they do.

#### Watchdog lifecycle (watcher-initiated operations)

**(0) Blocking isolation — all load paths.** `WorkspaceSource::load` becomes a
synchronous `fn load(&self, budget: &LoadBudget) -> Result<LoadOutcome>`, always
run on the blocking pool via `spawn_blocking`. `block_in_place` is removed from
`GitSource`. Today the fetch runs under `block_in_place` and the subsequent
YAML, `sops` and `vals` work runs *directly on a runtime worker*
(`git.rs:240` → `folder.rs:93-94`, an `async fn` wrapping synchronous work) —
with one worker thread, one blocked read there stops every timer in the
process, including the watchdog's. After this change **no load phase runs on a
runtime worker thread**. Peek already runs on `spawn_blocking` (`mod.rs:829`).

**(1) Three layers per operation.**

| Layer | What it is | Owns | May |
|---|---|---|---|
| **worker** | `spawn_blocking(move \|\| source.load(&budget))` | nothing | block, hang, panic |
| **finalizer** | a `tokio::spawn`ed async task | the execution-mutex guard and the permit | only `.await` — never blocks a runtime thread |
| **observer** | the watcher loop | nothing | give up waiting |

The finalizer, in this order:

1. awaits the worker's `JoinHandle` **with no timeout**, converting
   `Err(JoinError)` — a panic — into a failed outcome;
2. calls `apply_load_result` (→ `LoadCompleted`, clearing `load_in_flight` by
   `op_id`, publishing or recording the error);
3. stamps `reload_state.last_completed` — the completion-based API cooldown
   that `reload` and `reload_for_api` set today after completion, success or
   failure (`mod.rs:653`, `:691`) — through the guard it still holds, so the
   cooldown holds even when the caller has gone;
4. **drops the guard and the permit**;
5. only then, if today's rule applies (a successful watcher load that changed
   the revision, `mod.rs:890-895`), spawns the `stroem_workspace_reloaded`
   notification as its own detached, best-effort task.

The order of 4 and 5 matters. `publish_workspace_reloaded` awaits
`pg_notify` through the pool with no local deadline (`events.rs:141-147`,
`:276`). Revision 5 notified *before* releasing, so eight stalled notifications
could hold every watcher permit while every overdue gauge read 0 —
`load_in_flight` had already been cleared — making the § 4.8 alert blind to
the stall. Released first, a stalled notification holds nothing. Notifications
are already best-effort by design (CLAUDE.md § High Availability: "DB is
source of truth").

Because the finalizer sits **outside** the closure that can panic, panic
finalization is guaranteed, and a late success is still announced to peers.
Because it is a detached task, **cancelling whoever awaits it does not cancel
it** — the guard stays owned until the worker really returns (see (8) for why
this matters to external callers). The observer awaits the *finalizer's*
`JoinHandle` with `timeout_at(deadline)`.

*Decided:* the guard and permit are released only when the work actually
finishes. The concurrency bound stays honest, and two loads can never mutate
one checkout concurrently.

**(2) Admission — watcher only, ordered, every step non-blocking.**

1. `try_lock_owned()` the execution mutex. Contended ⇒ `Busy`.
2. `try_acquire_owned()` a watcher permit (`MAX_CONCURRENT_WORKSPACE_LOADS`).
   None free ⇒ release the mutex, `Saturated`.
3. `transition(LoadStarted { op_id, deadline: now + L })`.
4. Spawn the finalizer, moving the guard and permit into it.
5. Observer: `timeout_at(deadline, finalizer_handle)`.

On `Busy` or `Saturated` the watcher skips the tick and increments
`stroem_workspace_load_admission_skipped_total{reason}`. Neither is a load
outcome; neither changes `Availability` (§ 4.4 admission table).

**(3) Budget.** P = 30 s (peek), L = 5 min (load), configurable. L runs from
`LoadStarted` to completion, **including** any queueing in tokio's blocking
pool — which queues once its thread limit is reached
(`task/blocking.rs:91-94`), and the load semaphore reserves no blocking threads.
Revision 4's claim that queueing was "impossible" was false. The worker
therefore checks `budget.expired()` **before its first mutation** and returns
`DeadlineExceeded` without touching the checkout if it started too late.

**(4) Overdue is derived, not written.** On timeout the observer writes
**nothing**. `stroem_workspace_load_overdue{workspace}` is computed at scrape
time in `gather_gauges` as `load_in_flight.is_some_and(|f| now > f.deadline)` —
the existing scrape-time gauge pattern (CLAUDE.md § Prometheus Metrics). Only
the finalizer clears `load_in_flight`, and only by matching `op_id`. With a
single writer and a derived reading, a completion racing the timeout cannot
leave the gauge stuck.

**(5) Late completion is never discarded.** The finalizer applies the result
whenever the worker returns — success publishes, failure (including panic)
enters `Errored`.

**(6) Accepted consequence.** If every watcher permit on a replica is held by a
load stuck in an uninterruptible phase, that replica's watchers refresh **no**
workspace until one returns. Surfaced by
`stroem_workspace_load_permits_available` and the overdue gauge (§ 4.8). **Not
wired into `/livez`.** Recovery from a replica-wide stall is an operator action.

**(7) Peek.** Same three layers with budget P. Admitted only when
`peek_in_flight` is empty; the observer records `PeekStarted`. The finalizer
awaits the worker and **always** emits `PeekFinished` (clearing the record by
`op_id`), including after a panic. The observer applies the outcome: within P,
`PeekCompleted{peek}` (a panic reaching it within P is `Failed`); after P,
`PeekTimedOut`. A late result is **discarded** simply because only the observer
applies peek outcomes, and it has stopped waiting. A peek never touches the
published snapshot, so the § 4.4 recovery rule is preserved.

**(8) External callers — only what § 4 changes for them.** API reload, peer
`stroem_workspace_reloaded` and scheduler/webhook `force_refresh` still await
their own load, unbounded by any watchdog and taking no permit (§ 4.10 (3)).
§ 4 changes exactly three things:

- they acquire the execution mutex with **`try_lock`** instead of
  `.lock().await` (today's `reload`, `mod.rs:645-655`). Without this, § 4 would
  introduce a regression: a watcher load now holds that mutex until real
  completion, and a peer reload queued behind it would stall the serial event
  listener (`events.rs:360` → `:394`) for as long;
- **they run their load through the same detached finalizer as the watcher**
  (§ 4.5 (1)), minus the permit and the timeout, and await its `JoinHandle`.
  Today `reload` and `reload_for_api` hold the guard inside the caller's own
  future (`mod.rs:651`, `:671`). That is safe today only because the load is
  one blocking poll with no yield point inside a mutation. § 4.5 (0) introduces
  one — the caller now awaits a `spawn_blocking` handle — so if the guard stayed
  in the caller's future, cancelling the caller (an HTTP client disconnecting
  from a refresh request, say) would drop the guard and skip result application
  while the worker, which cannot be aborted (`task/blocking.rs:106-110`), kept
  writing to the checkout; a second load could then acquire the mutex and write
  to the same checkout concurrently. With the finalizer owning both the guard
  and result application, cancelling the caller only stops the caller waiting;
- they report their outcome through `apply_load_result` — which the finalizer
  now does on their behalf, so it happens even if the caller has gone.

On `Busy`:

| Caller | On `Busy` |
|---|---|
| `reload_for_api` | `ReloadApiError::Cooldown` — its existing try-lock-busy response (`mod.rs:671-678`) |
| peer notification | skip, log at debug. Convergence then relies on this replica's subsequent watcher ticks **when admission and source access succeed** — not guaranteed within one poll interval, because the mutex may still be held by an uninterruptible load and later admissions may be `Busy` or `Saturated`. Peer-triggered reloads never rebroadcast — unchanged (`events.rs:388-405`) — so no echo loop can form |
| scheduler `force_refresh` | **new policy**: if the published snapshot is healthy, fire from it; if `Errored`, today's behaviour — the workspace is unavailable and the fire is logged `MISSED` (`scheduler.rs:368-371`) |
| webhook `force_refresh` | **new policy**: healthy snapshot ⇒ proceed with it; `Errored` ⇒ today's error response (`web/hooks.rs:85`) |

Revision 4 described the `force_refresh` fallback as "today's behaviour"; it is
not — today a failed forced reload sets the workspace unavailable. Continuing
from a *healthy* snapshot is new, and applies only to `Busy`.

**Accepted additional exposure.** Today competing external reloads serialize
*before* the `force_refresh` caller creates its job (`mod.rs:651`,
`scheduler.rs:350`, `web/hooks.rs:67`), so the job is created from the
post-reload state. Under the Busy policy, a job can be created from healthy
snapshot A **while another load is mutating the checkout toward B**, and
tarball construction reads the live checkout (`web/worker_api/workspace.rs:97`).
That widens the § 4.6 byte-identity window for exactly those jobs. It is
accepted: the alternative — treating `Busy` as `MISSED` — reintroduces missed
fires, the failure this whole investigation began from. The window closes
with § 7 (2).

**(9) Startup — orchestration unchanged.** `WorkspaceManager::new` keeps its
`JoinSet` and blocking `acquire_owned().await` on the semaphore
(`mod.rs:264-279`). What changes: each load runs on `spawn_blocking` with a
cooperative `LoadBudget`, and each result goes through `apply_load_result`
(caller: startup). There is no watchdog at startup; an uninterruptible hang
still blocks startup exactly as today (§ 4.10 (4), § 3).

**(10) Shutdown — no promise.** § 4 makes no shutdown guarantee beyond today's.
A hung load delays process exit today (a runtime worker blocked in
`block_in_place`) and after § 4 (a blocking-pool thread that tokio waits for at
runtime drop) alike; Kubernetes' `SIGKILL` after `terminationGracePeriodSeconds`
is the backstop in both (§ 4.10 (5)).

#### Deadline propagation without changing other callers

The loader and the `vals` filter are shared with the CLI
(`stroem-cli/src/local/mod.rs:42`) and with claim-time rendering.

- `LoadBudget` — a deadline plus P/L — with `LoadBudget::unbounded()`.
- `load_workspace_with(path, &LoadBudget)`; the existing `load_workspace(path)`
  delegates with `unbounded()`. CLI and all other callers are unchanged.
- `sops::read_yaml_file_with(path, &LoadBudget)`; the existing function
  delegates unbounded.
- `vals`: Tera filters are `Filter` trait objects and may carry state, so
  `render_template_with(.., &LoadBudget)` registers a `ValsFilter { budget }`;
  plain `render_template` registers `ValsFilter::unbounded()`. **Claim-time
  rendering stays unbounded** — out of scope, recorded in TODO.md.

### 4.6 Serialization and publication

**Publication boundary.** Today `GitSource::load` writes `self.revision`
**before** YAML and secret loading (`git.rs:236-238`), and `get_revision` reads
the source directly (`mod.rs:502`), so a load that fetches B and then fails
secret rendering publishes revision B alongside config A. Fix:
`WorkspaceSource::load` returns `LoadOutcome { config, warnings, revision }` and
mutates no published state; `apply_load_result` swaps the published snapshot as
one `Arc`. **A failed load publishes nothing new.**

**Byte-identity window.** This fixes the metadata, not the files. Once a failed
load publishes `Errored`, `get_path` and `get_revision` return `None` and the
download handler rejects the request before any cache lookup or archive read
(`web/worker_api/workspace.rs:53-58`) — so a *new* download does not then
receive B under A. The hazard exists **while a load is mutating the checkout**
(including an overdue one, during which the workspace is still `Fresh` and
serving), and for a download that passed the availability check before the
mutation began. That is the inherited hazard of § 7 (2) and CLAUDE.md:174 — not
fixed, and not made worse **except** for `force_refresh` jobs created under the
`Busy` policy, which § 4.5 (8) explicitly accepts.

**Serialization.** Today the watcher calls `source.load()` directly
(`mod.rs:843`), bypassing the `reload_state` mutex that `reload` and
`reload_for_api` hold (`:645-693`) — the race recorded at TODO.md:1007. After
this change every load path holds the execution mutex, acquired with
`try_lock` (§ 4.5 (2), (8)); startup is the only acquirer that may wait, and it
runs before any watcher exists.

### 4.7 Availability transitions

A failed load is not a watcher-local event. It also makes the scheduler log
`MISSED` and decline to fire (`scheduler.rs:368`); makes event-source
reconciliation cancel live consumers (`event_source.rs:234`); and makes
`Settlement::advance` return early for non-terminal jobs
(`settlement/mod.rs:205`), which can strand a job until recovery.

The stranded-job case is **accepted as pre-existing and not fixed here**, and is
coordinated with the lifecycle spec (lines 219–220). Net effect of § 4: these
transitions become **strictly rarer**, because a transient peek failure no
longer produces a load that can fail.

### 4.8 Observability

`health::background_report` covers three background loops, not watcher
freshness, so operators cannot today distinguish "fresh" from "quietly skipping
for an hour". All metrics carry the global `replica_id` label.

| Metric | Type | Meaning |
|---|---|---|
| `stroem_workspace_last_successful_load_age_seconds{workspace}` | gauge | staleness |
| `stroem_workspace_peek_failures_total{workspace}` | counter | skip rate |
| `stroem_workspace_load_overdue{workspace}` | gauge, derived at scrape (§ 4.5 (4)) | a watcher load exceeded L and is still running |
| `stroem_workspace_load_permits_available` | gauge | **replica-local** watcher saturation — 0 also occurs with 8 healthy loads in progress |
| `stroem_workspace_load_admission_skipped_total{reason}` | counter | `busy` / `saturated` skips |

**Alert on** sustained `permits_available == 0` **together with**
`load_overdue > 0`, per `replica_id` — not on either alone.
`WorkspaceInfo` gains `last_successful_load` and `availability` for the UI.

**None of these fail `/livez` or `/healthz`.** Stale-but-serving is a healthy
state for this process; promoting a replica-wide stall to a restart is a
separate, deliberate decision — not made here.

### 4.9 Tests

Pure-function (unit):
- every row of the § 4.3 classification table
- every row of the § 4.4 tick, admission and completion tables, including the
  startup row and the precedence rule
- **K boundary (round 4):** `Fresh{K−1}` + `Failed` → `Fresh{K}`, admission
  `Saturated` → still `Fresh{K}`; next `Failed` re-attempts admission;
  `Revision == published` instead → `Fresh{0}`; `LoadStarted` → counter 0
- `PeekFinished` / `LoadCompleted` with a stale `op_id` leave a newer in-flight
  record intact

State and concurrency (integration, `#[tokio::test(flavor = "multi_thread")]`,
local bare repos over `file://`):
- **regression, round 2:** an external reload failure while `Fresh` enters
  `Errored`, and the watcher retries on backoff
- **regression, round 3:** `get_path`, `get_revision`, `get_config` return the
  published snapshot without blocking while a load holds the execution mutex
- **regression, round 3:** on a runtime built with `worker_threads(1)`, the
  observer's timeout fires while a load blocks in a fake `sops` that sleeps
- **panic, round 4:** a load worker that panics *after* the observer timed out
  → finalizer applies `Err`, workspace enters `Errored`, `load_in_flight`
  cleared, guard and permit released; a peek worker that panics after P →
  `PeekFinished` clears the record and the counter is unaffected by the late panic
- **late start, round 4:** a worker that begins after its deadline (saturated
  blocking pool) returns `DeadlineExceeded` without touching the checkout
- **no regression for external callers:** a peer reload arriving while a watcher
  load holds the execution mutex returns immediately; the event listener then
  dispatches a following cancellation notification without delay
- **cancellation, round 5:** cancel an external caller (drop the
  `reload_for_api` future) while its blocking load is still running — the
  finalizer keeps the execution mutex until the worker returns, a second load's
  `try_lock` gets `Busy` throughout, and the result is still applied; an API
  retry gets `Cooldown` while the original runs, is still subject to the
  completion-based cooldown after it finishes, and may refresh once that window
  expires (it does not recover the cancelled request's response)
- **stalled notification, round 5:** with `pg_notify` blocked, a successful
  watcher load still releases its guard and permit — permits gauge recovers,
  other workspaces admit normally
- external success while `Errored` → `Fresh{0}`; external failure while
  `Errored` leaves the ladder unchanged
- `force_refresh` on `Busy`: healthy snapshot ⇒ fires; `Errored` ⇒ `MISSED`
- a failed load publishes neither a new config nor a new revision
- watcher and `reload_for_api` cannot interleave config and revision
- watcher admission: busy mutex ⇒ `Busy`; exhausted semaphore ⇒ `Saturated` and
  the mutex is released; neither changes `Availability`
- watcher timeout while the load continues: guard and permit held until real
  completion; a late success publishes **and notifies peers**; a late failure
  enters `Errored`
- completion exactly at the timeout boundary: the derived overdue gauge reads 0
  afterwards regardless of ordering
- all watcher permits held by hung loads: permits gauge reads 0, other
  workspaces skip as `Saturated` without deadlock, refresh resumes when one returns
- deadline expiry mid-scan aborts the whole load (no partial config published)
- `sops` and `vals` are killed and reaped at the deadline (fake binaries on
  `PATH` that sleep)
- watcher start offsets differ across workspaces

### 4.10 Out of scope — recorded exposures

Cut after round 4, when findings moved from the watcher into the event bus,
startup orchestration and server lifecycle. Each is pre-existing — § 4 does not
make it worse — and each has a TODO.md entry.

1. **An admitted peer reload stalls the event listener.** Once a peer reload
   acquires the mutex it is awaited serially (`events.rs:360` → `:394`) and can
   delay cancellation and log notifications for its full duration. Today it can
   do the same. Fix shape: peer dispatch returns after admission and observes
   completion separately.
2. **Peer-notification semantics.** Payloads carry only sender and workspace
   (`events.rs:65`) — no revision for deduplication. If peer-triggered reloads
   are ever made to rebroadcast, origin tagging is required first, or A's
   notification reloads B, whose notification reloads A, indefinitely.
3. **External loads are unbounded.** API, peer and `force_refresh` loads take no
   permit and have no watchdog; only § 4.5's cooperative deadlines bound them.
4. **Startup saturation and hung initial loads.** Startup still waits on permits
   and on any uninterruptible initial load. Belongs with § 3 option 2.
5. **No signal-to-exit bound.** Axum's graceful drain is unbounded
   (`main.rs:305-311`), and tokio waits for running blocking tasks at runtime
   drop (`task/blocking.rs:27`). Fix shape: explicit runtime `Builder`,
   `Runtime::shutdown_timeout` — which tokio documents as leaving only
   `spawn_blocking` work behind (`runtime/runtime.rs:420-449`) — and a bounded
   HTTP drain.

## 5. Runtime thread count — MITIGATION, NOT A FIX

`#[tokio::main]` (`main.rs:26`) sizes from `available_parallelism()`, which
honours the cgroup quota: `500m` → **1 worker thread**. `nproc` reports 2 (it
reads online CPUs, not the quota), which is why this was not visible from inside
the container.

**The causal link to the 2026-09-16 hang is unproven.** TODO.md:1006 states the
root cause remains unknown and proposes a reproduction (`worker_threads = 1`,
several git sources failing concurrently, a task sleeping across the window).
These are **plausible mitigations for a measured condition**, nothing more:

1. Raise `limits.cpu` to ≥ 2.
2. Floor the runtime in `main.rs` — `worker_threads(max(4, n))` via
   `Builder::new_multi_thread`.

§ 4.5 (0) separately moves every load phase to the blocking pool and removes
`block_in_place` from `GitSource`, so after § 4 no workspace load runs on a
runtime worker thread. That removes the suspected ingredient of the hang; it is
**still not a proven fix**. Run the TODO.md:1006 reproduction alongside, so
neither change is mistaken for a diagnosis.

## 6. Worker revision cache never evicts on the normal path — NEEDS DESIGN

### 6.1 The defect

`cleanup_old_revisions` has exactly one call site,
`stroem-worker/src/workspace_cache.rs:222`, inside `ensure_up_to_date`. The
poller chooses on whether the step carries a revision (`poller.rs:252-258`):

```rust
let ws_result = if let Some(ref rev) = step.revision {
    ws_cache.ensure_revision(&client, &step.workspace, rev).await
} else {
    ws_cache.ensure_up_to_date(&client, &step.workspace).await
};
```

Every server creation path stamps a revision, so `ensure_revision` is the
production path and **cleanup never runs**. `max_retained_revisions` (default 2)
is dead config. The same call site also prunes `revision_refs` and reaps
orphaned `.tmp` staging dirs, so both are equally unreachable. Prod: 2.3 GB
across 6 workspaces, 1.6 GB of it three revisions of one repo (A.5).

### 6.2 Why "just add the call site" is wrong

1. **Check-then-acquire race.** `cleanup_old_revisions`
   (`workspace_cache.rs:451`, esp. `:478-529`) tests refcounts and deletes
   without holding the per-workspace extraction mutex; the download path
   releases that mutex *before* acquiring its `WorkspaceGuard` (`:260-324`). A
   revision can be deleted in that window.
2. **Staging-directory ownership.** Cleanup cannot distinguish a live
   extraction's `.tmp` dir from an abandoned one.
3. **Historical-revision availability.** Guards protect *executing* steps, not
   *future* ones. After eviction the worker must re-download; the server 404s
   when the requested revision is neither cached nor current
   (`web/worker_api/workspace.rs:76`), and the worker turns that into a step
   failure (`poller.rs:249`).

### 6.3 The historical-revision prerequisite — necessary, not sufficient

The server's tarball keep-set (`recovery.rs:441-470`) is built from

```sql
SELECT DISTINCT workspace, revision FROM job
 WHERE status IN ('pending','running') AND revision IS NOT NULL
```

plus each workspace's current revision. It does **not** include
`job_step.action_workspace` / `action_revision`, which a cross-workspace step
claims against (`web/worker_api/jobs.rs:876`). Such a tarball can be evicted
server-side today — masked only because the worker never evicts its copy.

Extending the keep-set is **necessary** before worker eviction. It is **not
sufficient**, because it only protects an archive that already exists on the
replica doing the sweep:

- **Replica-local caches** (§ 7 (1)): each replica's cache is private to its
  container layer, so a peer or a replacement replica may never have had it.
- **Best-effort cache write**: the archive is written fire-and-forget
  (`web/worker_api/workspace.rs:111-116`, `:170-185`) and may never exist.
- **Unavailable workspace**: the endpoint 404s via `get_path` before any cache
  lookup (`workspace.rs:53-58`), so a cached historical revision is unreachable
  while its workspace is `Errored`.
- **Task-retry handoff window**: settlement observes the failed job as terminal
  before `create_retry_job` creates a retry that inherits
  `failed_job.revision` (`settlement/retry.rs:149`). A retention snapshot in
  between finds no non-terminal owner. Independent of this spec; tracked in
  TODO.md.

Worker eviction therefore also depends on a **historical-serving guarantee**
(§ 7 (1)). § 10 orders both before it. Mixed-version rollout: a new worker that
evicts, against a server lacking either, turns cross-workspace and retried steps
into failures — same shape as the fail-or-retry / cascade ordering constraint in
CLAUDE.md § Step Cascade.

### 6.4 Open questions before this is implementable

- What is "current" for a pinned download, given that pinned downloads
  deliberately do not update `.current` (`workspace_cache.rs:449`)?
- What happens to a revision whose guard is released *during* a cleanup pass?
- Does eviction need a grace period, given a downstream step of a still-running
  job may need a revision no guard currently holds?
- Nothing removes a `{workspace}/` dir for a workspace the worker no longer
  serves — separate, simpler, same pass.

### 6.5 Tests

- deterministic concurrency test: guard acquisition racing eviction, asserting
  no acquired revision is ever deleted
- extraction racing `.tmp` cleanup
- cross-workspace step with `action_revision ≠ job.revision`, executed after
  eviction, against a server with both prerequisites — must succeed; against a
  server lacking either — must fail, pinning the rollout order

## 7. Tarball build: no cache on the live path, no single-flight — NEEDS DESIGN

`web/worker_api/workspace.rs:137-198` — the un-pinned path checks
`If-None-Match`, then unconditionally calls `build_tarball`, which walks the
workspace plus every library and gzips into an in-memory `Vec<u8>`, then
`tarball.clone()`s it to write back to the cache it never read. No `Semaphore`,
no in-flight dedup: after a change, every worker can trigger a simultaneous full
build. Peak memory ≈ concurrent misses × ~2 × tarball size, against a limit
already at 95 %. Cache hits are whole-file `std::fs::read` into memory
(`tarball_cache.rs:63-87`); the cache has no size or count bound, and eviction
is a leader-only sweep at `retention.interval_secs` (default 3600 s).

Open questions any design must answer:

1. **Replica ownership.** The cache path is a sibling of the log directory
   (`state.rs:174-179`), and Helm mounts only the log directory
   (`templates/server-deployment.yaml:99`). Each replica's cache is local to its
   container layer and swept only while that replica is leader; a follower's
   grows unswept, and a historical-revision request can land on a replica that
   never cached it. Candidates: shared volume, object storage, rebuild-from-git
   on demand. **A prerequisite for § 6.**
2. **Byte identity.** Keying on `(workspace, revision)` does not guarantee the
   bytes: `build_tarball` reads a live checkout that `reset --hard`
   (`git.rs:74-89`) can mutate mid-build (§ 4.6 bounds when). Needs an explicit
   snapshot or a lock spanning the archive read.
3. Streaming instead of a buffered `Vec<u8>`, on both build and hit paths.

## 8. O(N) per-request fan-outs — real, low priority

Measured p99 for `/api/jobs/{id}` is under 25 ms at 9 workspaces (A.6); listed
so they are not rediscovered as a surprise.

- `web/api/jobs.rs:492-498` — `WorkspaceSet::load` + `collect_redaction_values`
  over every workspace's secrets and connections. The UI poll is adaptive: 3 s
  with running/suspended steps, 8 s otherwise, and a completed job's tab
  generally stops (`ui/src/pages/job-detail.tsx:141`).
- `web/api/jobs.rs:1194-1216` `resolve_acl_scope` — flattens every task of every
  workspace, then glob-evaluates; the admin short-circuit happens *after* the
  collect. From `/api/stats` and `/api/jobs`, polled every 5 s by the dashboard.
- `mcp/auth.rs:159-179` — same flatten, only for `list_workspaces`, `list_tasks`,
  `list_jobs` (`mcp/tools.rs:326`, `:361`, `:688`), and it exits early without
  authentication or configured ACL.
- `web/worker_api/jobs.rs:588` — `WorkspaceSet::load` on every `claim_job`.
- `job_creator.rs:809-837` — `WorkspaceSet::load` per `type: task` flow step,
  conditionally: skipped for `when`-guarded steps and steps with no literal
  connection inputs.
- `scheduler.rs:155` — rebuilds the whole trigger map, re-parsing every cron
  expression, on every wake-up.
- `web/hooks.rs:474` — unindexed scan of every trigger per inbound webhook.

No lock-contention risk: each fan-out takes a short per-entry read lock and
releases it after an `Arc::clone` (`workspace/mod.rs:52-72`).

## 9. Also true, lower stakes

- **Clones are full, not shallow** (`git.rs:96-106`), with a working tree, in
  `std::env::temp_dir()` (`git.rs:29-32`) — the container writable layer, so
  every restart re-clones all of them.
- **`FolderSource::peek_revision`** (`folder.rs:29-61`) reads and blake2-hashes
  every file every 30 s.
- **Removing a workspace from config leaves its clone on disk** forever.
- **Library resolution at startup** still uses `block_in_place`
  (`workspace/mod.rs:196`); § 4.5 (0) does not cover it.

## 10. What this gate approves, and the order

**This revision seeks approval for § 4 only**, as scoped by § 4.10. Nothing else
in the table below is part of this gate.

| Step | Scope | Gate |
|---|---|---|
| 1 | Envelope: `limits.cpu: 2`, `limits.memory: 2Gi`, non-empty chart defaults; **re-measure** startup, `memory.current`, `/api/jobs/{id}` p99 | none — config |
| 2 | § 5 (2) explicit runtime `Builder` with `worker_threads` floor | none — small change, PR review |
| 3 | **§ 4** | **this gate** |
| 4 | § 4.10 (1), (2), (5) — peer dispatch, notification origin, shutdown bound | own gate |
| 5 | § 6.3 keep-set extension | own gate |
| 6 | § 7 (1) historical serving across replicas | own gate |
| 7 | § 6 worker eviction — only after 5 **and** 6 are deployed fleet-wide | own gate |
| 8 | § 3 option 2 (with § 4.10 (4)) — only if step 1 shows an unacceptable rollout window | own gate |
| 9 | § 7 (2), (3) | own gate |

Steps 4–9 are conditional on what step 1 measures. Nothing below step 3 should be
implemented on the strength of this document.

## 11. Verification and rollout (for § 4)

Per CLAUDE.md §§ Mandatory Test Coverage / Mandatory Documentation Updates:

- **Unit / integration:** § 4.9 in full.
- **HA:** covered by § 4.9's no-regression test for peer reloads — a peer reload
  arriving during a watcher load returns immediately, and the listener stays
  responsive.
- **E2E:** `tests/e2e.sh` — a job executing against a workspace whose peek is
  failing still runs from the last loaded config.
- **Docs:** sizing guidance under `docs/src/content/docs/operations/`; each new
  metric gets a `pub const` in `metrics.rs`, a recording site, a
  `metrics_test.rs` case and an entry in `operations/metrics.md` (CLAUDE.md
  § Prometheus Metrics); CLAUDE.md § Multi-Workspace gains the peek policy, the
  three-way state split, the single-writer `transition` rule and the
  worker/finalizer/observer layering; TODO.md entries marked `[x]`.
- **Config:** K, P, L, the backoff cap and the two libgit2 timeouts need
  defaults, validation and docs. Per-workspace fields hit the
  `WorkspaceSourceDef` tagged-enum limitation (TODO.md:1577) — env overrides
  arrive as strings and need lenient deserializers. The libgit2 timeouts are
  process-wide and `unsafe`; they are server-level config, set in `main.rs`
  before any git thread starts.
- **Trait change:** `WorkspaceSource::load` and `peek_revision` become
  synchronous with a `&LoadBudget` parameter. Both implementors and the test
  doubles behind `mark_unavailable_for_test` / `replace_config_for_test` change
  with them. Internal to `stroem-server`; no public API or wire change.
- **Rollout:** server-local; no data-model change; no ordering constraint
  against workers or other replicas.

---

## Appendix A — raw captures

All taken 2026-09-17 against namespace `tools`, pod
`stroem-server-75d4b54d98-kfm6b` (and `stroem-worker-b9f5cf56-lzxrc` for A.5).

### A.1 Startup timing
`kubectl logs -n tools stroem-server-75d4b54d98-kfm6b --since-time=...`
```
2026-09-17T07:49:43.162712Z  INFO stroem_server: Starting Strøm server v0.16.3
2026-09-17T07:49:43.304344Z  INFO stroem_server: Loading 9 workspace(s)...
2026-09-17T07:50:34.411111Z  INFO stroem_server::workspace: Loaded 9 workspace(s) in 51.106725134s
2026-09-17T07:50:34.482223Z  INFO stroem_server: Server listening on 0.0.0.0:8080
```

### A.2 Deployed probes and resources
`kubectl get deploy stroem-server -n tools -o jsonpath='{...probes}'`
```
liveness:  {"failureThreshold":3,"httpGet":{"path":"/healthz","port":"http"},
            "initialDelaySeconds":10,"periodSeconds":10,"timeoutSeconds":5}
readiness: {"failureThreshold":3,"httpGet":{"path":"/healthz","port":"http"},
            "initialDelaySeconds":5,"periodSeconds":5,"timeoutSeconds":5}
startup:   {"failureThreshold":30,"httpGet":{"path":"/healthz","port":"http"},
            "periodSeconds":10,"timeoutSeconds":1}
```
`kubectl get deploy -n tools -o custom-columns=...`
```
NAME             CPU_REQ  CPU_LIM  MEM_LIM  REPLICAS
stroem-server    100m     500m     512Mi    2
stroem-worker    1        2        8Gi      2
```
Volumes: `config` (ConfigMap, `/etc/stroem`), `logs` (emptyDir,
`/var/stroem/logs`). **`/tmp` is not a volume.**

### A.3 cgroup and threads
```
/sys/fs/cgroup/memory.max      536870912
/sys/fs/cgroup/memory.current  509923328
/sys/fs/cgroup/memory.stat     anon 308531200
                               file 188260352
                               slab 11745088
                               kernel_stack 147456
                               pgmajfault 11
/sys/fs/cgroup/memory.events   low 0 / high 0 / max 81
                               oom 0 / oom_kill 0 / oom_group_kill 0
/sys/fs/cgroup/cpu.max         50000 100000
ls /proc/1/task | wc -l        8
comm histogram                 7 tokio-rt-worker, 1 stroem-server
nproc                          2
```

### A.4 Server git clones — `du -sh /tmp/stroem/git/*`
```
455M total
229M demographic_prediction     25M jobs_beta        600K dwelltime_isolated
133M jobs_stage                 25M jobs            216K playground
 44M ai_traffic_model          948K ai_indoor_validation    0 ai_validation
```
`df -h /` → `overlay 100G, 8.4G used, 92G avail, 9%`

YAML volume per workspace (excluding `.git`):
```
ai_traffic_model 111 files / 3,102,024 B      jobs        50 / 169,371
jobs_beta         48 /   150,859              jobs_stage  48 / 150,859
demographic_prediction 9 / 38,907             playground   9 /  29,885
dwelltime_isolated 1 /    5,970               (2 workspaces: 0 files)
```

### A.5 Worker cache — `du -sh /var/stroem/workspace-cache/*`
```
2.3G total
ai_traffic_model        3 revision dirs   1.6G
demographic_prediction  2                 451M
jobs                    4                  97M
jobs_stage              1                 133M
jobs_beta               1                  25M
dwelltime_isolated      1                 732K
*.tmp staging leftovers: 0
```
`df -h /` on the worker → `overlay 100G, 47G used, 54G avail, 47%`

### A.6 `/api/jobs/{id}` latency — `GET /metrics`
```
stroem_http_requests_total{method="GET",route="/api/jobs/{id}",status="200"} 217
duration_bucket le=0.005   58
duration_bucket le=0.01   197
duration_bucket le=0.025  216
duration_bucket le=0.05   217   (and all higher buckets)
```
Comparators: `/api/jobs` 4.343 s / 99 = 43.9 ms mean; `/api/stats` 1.994 s /
48 = 41.5 ms mean; `/api/workers` 2.100 s / 417 = 5.0 ms mean.

---

## Appendix B — gate review history

### Round 1 → revision 2

Codex, 2026-09-17, thread `01a0afcd`: *needs revision before implementation*.
Final dispositions, as re-assessed in round 2:

| Finding | Disposition |
|---|---|
| 2.1 cache guard claim | Corrected; safe eviction deferred (§ 6.2) |
| 2.2 "unavailable ≠ removed" mis-cited | **Resolved** (§ 4.2) |
| 2.3 probe split ≠ readiness | **Resolved** (§ 3) |
| 2.4 runtime change as proven fix | **Resolved** (§ 5) |
| 2.5 fan-out frequencies | **Resolved** (§ 8) — contested only as to priority |
| 2.6 unverifiable capacity numbers | **Resolved** as presentation (§ 2, Appendix A); not re-collected |
| 3.1 cancellation contract | → revs 3–5 (§ 4.5) |
| 3.2 watcher algorithm | → revs 3–5 (§ 4.4) |
| 3.3 serialization / HA callers | → revs 3–5 (§§ 4.5 (8), 4.6, 4.10) |
| 3.4 historical availability | Acknowledged → rev 3 marks it insufficient (§ 6.3) |
| 3.5 availability transitions | **Resolved** as scope/risk; lifecycle recovery deferred (§ 4.7) |
| 3.6 tests / docs / rollout | Improved → revs 3–5 (§§ 4.9, 11) |
| 4.1 / 4.2 preservation / gate scope | → rev 3 (§§ 4.4, 10) |
| 5.1 `Peek` classification | → rev 3 (§ 4.3) |
| 5.2–5.4 retention / replicas / identity | **Deferred** (§§ 6.4, 7) |
| 5.5 observability | → revs 3–4 (§ 4.8) |
| 6.1 / 6.2 numbering / tick wording | **Resolved**; rev 2's two dangling refs fixed in rev 3 |

### Round 2 → revision 3

Codex, 2026-09-18, same thread: *§ 4 still needs revision before approval*.
Dispositions as re-assessed in round 3:

| Finding | Round-3 assessment |
|---|---|
| 2.1 wrong `git2` version; mechanisms misclassified | Mostly resolved; checkout timing → rev 4 |
| 2.2 YAML not pure CPU; SOPS omitted | **Resolved** at contract level |
| 2.3 classification not exhaustive; git-only rationale | **Resolved** |
| 3.1 **blocker** — watcher-local recovery regression | **Resolved** in the design |
| 3.2 **blocker** — watchdog lifecycle undefined | Substantially resolved; hold-until-completion endorsed as sound |
| 3.3 backoff timing / precedence | **Resolved**, apart from the startup-row exception → rev 4 |
| 3.4 keep-set insufficient; retry handoff race | Acknowledged and deferred; no longer blocks § 4 |
| 4.1–4.3 promises / scope table / comparison typo | **Resolved** |
| 5.1 publication boundary | Pair publication specified; readable serving state → rev 4 |
| 5.2 deadline propagation through shared APIs | **Resolved** at contract level |
| 6 dangling references | **Resolved** |
| Author's `/livez` self-correction | Confirmed correct |

### Round 3 → revision 4

Codex, 2026-09-18, same thread: *§ 4 NEEDS REVISION*. Dispositions as
re-assessed in round 4:

| Finding | Round-4 assessment |
|---|---|
| 2.1 checkout cancellation during writes; `notify_on` | **Resolved** |
| 2.2 byte-identity timing | **Resolved** |
| 3.1 readers need a lock distinct from the execution mutex | **Resolved** |
| 3.2 watchdog schedulability → `spawn_blocking` for the whole load | **Resolved**; choice assessed as sound |
| 3.3 admission / budget start | Partially — "queueing impossible" false; external/startup callers → rev 5 |
| 3.4 overdue gauge race | **Resolved** |
| 4 startup row; replica-local alerting | **Resolved** |
| 4 single writer vs peek | Partially — starts not expressible as events → rev 5 |
| 5 late or panicking peek | Partially — panic after timeout → rev 5 |
| 5 shutdown supervisor | Runtime teardown correct, but signal-to-exit unbounded → cut to § 4.10 (5) |

### Round 4 → revision 5 — scope cut

Codex, 2026-09-18, same thread: *§ 4 NEEDS REVISION*. Every round-3 finding
resolved or narrowed; the new findings split into five inside § 4 and four in
neighbouring subsystems — each of the four caused by revision 4 widening § 4's
admission contract to every load path. Following the author's standing rule for
this loop (cut scope when findings drift to a neighbouring subsystem), the user
chose to cut those four out and re-gate.

| Finding | Disposition |
|---|---|
| 2.1 admitted peer reload still stalls the listener | **Cut** → § 4.10 (1). § 4 now only guarantees *no regression*: externals `try_lock`, so none queues behind a watcher load (§ 4.5 (8)); test in § 4.9 |
| 2.2 "queueing is impossible" false | **Fixed** — § 4.5 (3): L includes blocking-pool queueing; expiry check before first mutation; test |
| 2.3 `force_refresh` fallback mislabelled as today's | **Fixed** — § 4.5 (8): labelled *new policy*, healthy snapshot only; `Errored` keeps today's `MISSED` / error |
| 3.1 startup saturation / unfinished initial load | **Cut** → § 4.10 (4) with § 3. Startup orchestration unchanged (§ 4.5 (9)); no non-blocking admission at startup |
| 3.2 signal-to-exit not bounded | **Cut** → § 4.10 (5). § 4 makes no shutdown promise (§ 4.5 (10)); the dependency on § 10 step 2 is removed |
| 3.3 notification ownership; echo loop | **Fixed** for § 4's own path — the finalizer publishes under today's rule, peer reloads still never rebroadcast (so no loop); the "in-flight load notifies peers" claim for dropped notifications is withdrawn, convergence is by poll. Wider semantics **cut** → § 4.10 (2) |
| 3.4 panic finalization | **Fixed** — § 4.5 (1): finalizer outside the panicking closure owns guards and cleanup; panic ⇒ `Err`; tests for late panic of both load and peek |
| 4.1 K-th failure: reset vs no-transition | **Fixed** — § 4.4: counter saturates at K, resets only on `LoadStarted`; forced probe stays pending across rejected admissions; boundary test |
| 4.2 single-writer events cannot express starts | **Fixed** — § 4.4: `PeekStarted`, `LoadStarted`, `PeekFinished` events; disjoint observer/finalizer responsibilities |

### Round 5 → revision 6

Codex, 2026-09-18, same thread: *§ 4 NEEDS REVISION* — all five in-scope
round-4 findings resolved; scope cut assessed as coherent; deferred items not
reopened. Two new lifecycle gaps, both introduced by revisions 4–5.

| Finding | Disposition |
|---|---|
| 3.1 external loads lose guard ownership on caller cancellation — a regression introduced by § 4.5 (0) adding a yield point inside the load | **Fixed** — § 4.5 (8): external loads run through the same detached finalizer (no permit, no timeout), which owns the guard and result application; cancellation test in § 4.9 |
| 3.2 finalizer holds permits across an untimed `pg_notify`, after `load_in_flight` is cleared | **Fixed** — § 4.5 (1): release guard and permit **before** notifying; notification spawned as a detached best-effort task; stalled-notification test in § 4.9 |
| 2.1 peer convergence "within one poll interval" overstated | **Fixed** — § 4.5 (8): relies on subsequent watcher ticks when admission and source access succeed |
| 4.1 Busy `force_refresh` widens the byte-identity window § 4.6 called unchanged | **Accepted explicitly** — § 4.5 (8) and § 4.6; the alternative (`Busy` ⇒ `MISSED`) would reintroduce missed fires |
| 6 older TODO.md entries contradict the cut (semaphore around every load; bounded shutdown) | **Fixed** in TODO.md |

### Round 6 — verdict

Codex, 2026-09-18, same thread: **§ 4 APPROVE WITH NITS.** Both round-5 majors
confirmed resolved; no new correctness issue from the finalizer reordering, the
external-caller routing, or notification overtaking (payloads carry no revision
and receivers reload from source, so reordering can only cause redundant
reloads). No blocking inconsistency.

| Nit | Disposition |
|---|---|
| 3.1 preserve completion-based API cooldown bookkeeping (`reload_state.last_completed`) in the finalizer | **Applied after approval** — § 4.5 (1) step 3, before guard release, even with the caller gone; § 4.9 cancellation test extended to retry-after-completion. Not re-reviewed; it transfers existing behaviour (`mod.rs:653`, `:691`) and adds no design |
