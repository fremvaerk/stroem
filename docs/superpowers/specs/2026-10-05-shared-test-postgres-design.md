# Shared test Postgres container

Status: revision 13, proposed (2026-10-05)

Companion of `docs/internal/TODO.md` § "Integration tests start one Postgres
container per test" (2026-10-05).

## Revision history

**Revision 13 (2026-10-05, Codex design review, trivial fix).** Both
revision 12 fixes confirmed complete. One miss found: § 3.1's manual
`docker run` example was described as labelled `stroem.test=true` but the
actual command text didn't include `--label stroem.test=true` — a
container started by copying it verbatim would be invisible to
`scripts/test-clean.sh`. Added the flag to the command itself. §3.1
changed.

**Revision 12 (2026-10-05, Codex design review, two precise fixes).** Codex
found the advisory lock in § 3.1 didn't actually cover the full race: it
was acquired *after* `CREATE TABLE IF NOT EXISTS stroem_test_marker`, which
Postgres does not guarantee is safe under concurrent execution — two
binaries against a fresh manually-shared server could still race on table
creation itself. Fixed by acquiring the lock first, before anything else,
and holding it across the entire sequence (table creation through the
marker write). Also: § 4's capacity fix claimed both paths get a raised
`max_connections`, but the manual `TEST_DATABASE_URL` path's example
`docker run` command didn't actually set it — and can't be set by
`ensure_template_migrated` on a server it didn't start. Added the flag to
the example, stated plainly that it's the human's own responsibility for
that path, and added a sizing rule for a high-core host or several test
binaries sharing one manual server at once. Also corrected an overstated
claim of my own: "411 tests" is a total, not a concurrency figure — Rust's
default test-harness parallelism is bounded by core count, not test count.
§3.1, §4 changed.

**Revision 11 (2026-10-05, Codex design review of the simplified spec).**
Codex confirmed the per-binary-only design holds up with no leftover
wrapper-script dependency, then found two real gaps: (1) § 3.1 contradicted
itself — the manual `TEST_DATABASE_URL` path was described as requiring a
pre-existing `stroem_test_marker` row and erroring if absent, while the
shared `ensure_template_migrated` function's own description said *both*
paths create/migrate the template. Resolved by making both paths
genuinely identical: `test_db()` always calls `ensure_template_migrated`
on whatever admin URL it has (OnceCell-managed or manually supplied), which
migrates on first use and no-ops afterward — removing the asymmetry rather
than papering over it. This introduces a new problem the per-binary case
never had: a manually-shared container can have *multiple test binaries*
calling `ensure_template_migrated` concurrently, which could race on
creating/migrating the template; closed with a Postgres advisory lock
around that function's check-and-maybe-migrate section. Also specified
where the migration fingerprint actually comes from (`stroem_db` gains a
public `migration_fingerprint()` alongside `run_migrations`, both reading
the same compile-time `sqlx::migrate!` data, so they can't drift — Codex
had asked how the helper would get the same fingerprint as the currently
private `Migrator` in `pool.rs`). (2) Connection capacity was hand-waved as
"needs a stress test"; Codex did the actual math — the largest integration
binary has 411 tests, and reusing `stroem_db::create_pool`'s production
defaults (5–20 connections *per test*) could put ~20 concurrent tests
around 100 baseline connections, at or past Postgres 11's stock
`max_connections`. Fixed with a concrete design, not a deferred stress
test: `test_db()` uses its own small, fixed pool size (not the production
default), and every container is started with a raised server-side
`max_connections`. Minor: "one per test binary" is more precise than "one
per file" (in-module tests share their crate's own test binary);
`scripts/test-clean.sh`'s routine-use guidance now says to run it after
active test runs have finished. §3.1, §3.2, §4 changed.

**Revision 10 (2026-10-05, scope cut #2, user decision).** Revisions 6–9
simplified `scripts/test.sh`'s teardown but kept finding one more genuine
bash signal-handling bug each round (trap re-entrancy, an unbound variable,
a launch-to-PID-capture race) — nine rounds total since revision 2, nearly
all of it spent on the wrapper script's bash correctness, not the database
design. Asked whether to keep going or step back further, the user asked
directly: would reverting to one container *per test binary* — no wrapper
script at all — actually help? It does, substantially: that mechanism is
exactly the existing § 3.1 per-binary `OnceCell` path, which needs no
process-group tracking, no traps, no signal races, because there is no
cross-process container to hand off — each test binary just owns its own
container for its own lifetime, the same simple Rust pattern already
reviewed and stable since revision 3. The user confirmed: `scripts/test.sh`
and all of old § 3.2's signal handling are deleted outright. What remains:
the per-binary path is now the *only* path (not a fallback), cutting
container count from hundreds (one per test) to roughly one per test
*file* (~27-30 for a full workspace run) — a smaller win than "exactly
one," but one with no wrapper-script complexity left to get wrong, and the
cleanup story for those containers is the same human-run
`scripts/test-clean.sh` already designed and reviewed for the crash case in
revision 6, now serving as the *primary* cleanup path rather than a rare
backstop. `TEST_DATABASE_URL` is kept as an optional, manually-managed
override (§ 3.1) for anyone who wants to hand-start one shared container
for faster local iteration — entirely optional, never orchestrated by any
script this design provides. §2, §3.1, §3.2, §3.3, §4, §5 rewritten.

**Revision 9 (2026-10-05, Codex design review, empirically reproduced
races).** Superseded by the scope cut above. Fixed three more real,
empirically-reproduced bugs in the now-deleted wrapper script: a signal
arriving during an already-in-progress teardown could abandon it before
`docker rm`; a grace-period variable was unbound under `set -u`; a window
between backgrounding `cargo test` and capturing its PID could lose track
of the process entirely. The underlying lesson (each fix closing one gap
while the mechanism kept finding another) is what prompted asking the user
whether to simplify further, which they did.

**Revision 8 (2026-10-05, Codex design review, bash correctness pass).**
Superseded. Fixed four bugs in the wrapper's teardown: an inaccurate claim
about when the process group is empty, unspecified trap-then-return
semantics that let execution resume mid-script, `set -e` aborting before
`docker rm` on an expected `kill` failure, and a classic `A && B || C`
false-warning gotcha.

**Revision 7 (2026-10-05, Codex design review on the cut-scope revision).**
Superseded. Fixed trap-installation ordering and `wait`-vs-group-liveness
conflation in the wrapper's teardown; qualified the "one container per
run" goal to "only via the wrapper"; corrected a GitHub Actions
cancellation-signal claim; reframed `scripts/test-clean.sh`'s age check
honestly as a human's explicit choice, not an inherent safety property.

**Revision 6 (2026-10-05, scope cut #1, user decision after Codex review
round 5).** Revisions 2–5 spent five rounds hardening an *automatic*,
cross-invocation reaper (could a later process safely tell whether an
earlier, possibly-crashed run's container was still in use) and kept
finding genuine but increasingly narrow cross-platform bash/POSIX
subtleties doing it. The user cut that reaper entirely in favor of a
same-invocation-only teardown plus a human-run `scripts/test-clean.sh` for
the crash case — later itself superseded by revision 10's deeper cut, but
`scripts/test-clean.sh` as designed here is what revision 10 keeps and
promotes to the primary cleanup path.

**Revision 5 (2026-10-05, Codex design review round 4, empirical).**
Superseded. Confirmed a backgrounded subshell under `set -m` gets its own
PGID surviving `SIGKILL` of just the wrapper's PID, but found `$$` inside a
`( … )` subshell is the *wrapper's* PID on every bash tested (not the
subshell's own), and `pgrep -g` can exit with an ambiguous internal-error
status rather than a clean "no match."

**Revision 4 (2026-10-05, Codex design review round 3).** Superseded.
Replaced PID-based ownership with process-group-based ownership after a
`SIGKILL`'d wrapper was shown able to leave its `cargo test` group running
while a dead-PID label fooled a reaper into removing the container anyway.

**Revision 3 (2026-10-05, Codex design review round 2).** Moved the
`TEST_DATABASE_URL` marker check out of `stroem_template` into an
admin-database table queried over the same `postgres` connection already
used for `CREATE DATABASE`, after Codex showed the original check
reintroduced the exact connection-collision problem this design exists to
avoid. Also fixed call-site coverage (missed an in-module unit test in
`crates/stroem-server/src/settlement/hooks.rs`). Still current in § 3.1 and
§ 3.4 — this revision's database-design fixes are the stable core that
every later revision (including the scope cuts) has left untouched.

**Revision 2 (2026-10-05, Codex design review, thread
01a10bed-b645-76c1-9362-0c0a7f7147f7).** Corrected the Ryuk explanation
(the pinned testcontainers `0.27.3` has no Ryuk code at all); fixed the
reaper's label (was the global `org.testcontainers.managed-by=testcontainers`,
matching every testcontainers-managed container on the host including
unrelated projects'); fixed the fixed container name (let two overlapping
invocations remove each other's live container); added the explicit
template-pool close before cloning; added `test_db() -> TestDb { pool, url
}` to cover call sites needing the raw connection URL, not just a pool. Its
`stroem_test_support` crate design (§ 3.1) is still current.

## 1. Problem

Every DB-backed `#[tokio::test]` across ~27 integration test files (in
`stroem-db`, `stroem-server`, `stroem-e2e`) — plus at least one in-module
unit test (`crates/stroem-server/src/settlement/hooks.rs`, § 3.4) — calls
`Postgres::default().start()` (testcontainers, `postgres:11-alpine`) and
runs every migration, individually. A full `cargo test --workspace` run
starts hundreds of containers.

Cleanup is unreliable, independent of count, for three *actual* reasons
(`TESTCONTAINERS_RYUK_DISABLED=true` — used in some documented
worktree/spec test-run commands in this repo, but not universally;
`AGENTS.md`'s own Build & Test section runs plain `cargo test --workspace`
without it — is a no-op regardless: the pinned testcontainers `0.27.3` has
no Ryuk code at all, zero matches for "ryuk" in the installed source):

- **`Drop` in Rust cannot be `async`.** `ContainerAsync`'s `Drop` impl
  (`testcontainers-0.27.3/src/core/containers/async_container.rs:244`)
  schedules the container's removal as a detached background task — it
  cannot block the owning scope on that call finishing. Every call site that
  does *not* leak its container still only gets this best-effort cleanup,
  not a guaranteed one.
- **One helper leaks its container outright.**
  `crates/stroem-db/tests/common/mod.rs::setup_db()` does
  `Box::leak(Box::new(container))` ("CI runners are ephemeral" — true in CI,
  false locally), guaranteeing that container is never stopped by Rust code
  at all, ever.
- **The `watchdog` feature (a separate, unrelated mechanism — local
  signal-handling cleanup, not Ryuk) is deliberately left disabled** in both
  `crates/stroem-db/Cargo.toml` and `crates/stroem-server/Cargo.toml`,
  because enabling it panics under parallel testcontainer startup
  (`conquer_once::Lazy` race, noted in both files' comments).

Observed effect: 118 orphaned `postgres:11-alpine` containers, aged 32
minutes to 6 hours, accumulated over one day of local test runs.

A secondary finding: `.github/workflows/ci.yml`'s `test` job declares a
`services: postgres:17` container and a `DATABASE_URL` env var. No Rust code
reads `DATABASE_URL` — this CI container is unused dead weight; CI hits the
identical per-test testcontainers problem as local runs, just on an
ephemeral runner where the disk-pressure symptom doesn't surface.

## 2. Goals / non-goals

Goals:
- Cut container count from hundreds (one per test) to roughly one per test
  *binary* — not one per test function, and not quite one per test file
  either, since an in-module unit test (§ 3.4) shares its crate's own
  existing test binary rather than getting a dedicated one — for a full
  `cargo test --workspace` run, locally and in CI. (Revisions 2–9 pursued
  literally one container for the whole run via a wrapper script; the user
  judged the resulting bash complexity not worth it relative to this
  simpler win — see revision 10's history entry.)
- Per-test database isolation preserved: recovery sweeps, the fixed
  leader-election advisory-lock key, and `LISTEN/NOTIFY` channels all operate
  database-wide, so two tests sharing one database would collide.
- A single shared helper, not dozens of copies of setup logic.
- No code path ever opens a second connection to the template database after
  its one-time migration — that connection would collide with a concurrent
  `CREATE DATABASE ... TEMPLATE`.
- Ad hoc single-test runs (IDE "run test", `cargo test -p stroem-db`,
  `cargo test --workspace`) work identically with no manual setup step —
  there is no separate "wrapper" mode to opt into or bypass.
- An orphaned container from a crash, or simply the normal accumulation of
  one-per-binary containers over many local runs, is recoverable with one
  on-demand human command (`scripts/test-clean.sh`), not by hunting for
  stray containers by hand.

Non-goals:
- One container for an entire `cargo test --workspace` run. Pursued through
  revision 9 via a wrapper script managing a single container across every
  test binary in the run; cut because the mechanism needed to bridge that
  many independent OS processes (traps, process groups, signal races) kept
  surfacing one more genuine bash/POSIX bug every review round, for a
  benefit (hundreds → 1 vs. hundreds → ~30) that didn't justify the ongoing
  correctness burden. An interested user can still get this manually: start
  one container by hand and export `TEST_DATABASE_URL` (§ 3.1) — nothing in
  this design orchestrates that for you, which is exactly why it carries
  none of the cut mechanism's risk.
- An *automatic*, cross-invocation reaper that can safely tell whether a
  container from a previous (possibly crashed) run is still in use. Cut in
  revision 6 for the same reason as above, one layer earlier.
- Fixing the `watchdog` feature's parallel-startup panic, or any other
  in-process signal-handling cleanup mechanism.
- Changing per-test isolation semantics (still one logical database per
  test).
- Cleanup of test databases created against a manually-exported
  `TEST_DATABASE_URL` pointing at a server nothing in this design manages —
  that usage is explicitly a manual, best-effort convenience (§ 3.1), not a
  supported, cleaned-up mode.

## 3. Design

### 3.1 `stroem-test-support` crate

New workspace member, dev-dependency only (`stroem-db`, `stroem-server`,
`stroem-e2e`), replacing the setup helpers copied across the call sites in §
3.4 (including `crates/stroem-db/tests/common/mod.rs::setup_db()`, which
loses its `Box::leak`).

```rust
pub struct TestDb {
    pub pool: sqlx::PgPool,
    pub url: String,
}

/// Returns a fresh, migrated, isolated Postgres database for one test —
/// both a ready-to-use pool and its raw connection URL, since several
/// existing call sites (crates/stroem-server/tests/integration_test.rs's
/// `setup()`, crates/stroem-e2e/tests/harness.rs's `TestEnv::setup()`) need
/// the URL to build a real server's `DbConfig`, not just a pool.
pub async fn test_db() -> TestDb;

/// Convenience wrapper for call sites that only need the pool.
pub async fn test_pool() -> sqlx::PgPool {
    test_db().await.pool
}
```

**One mechanism, not a primary-plus-fallback pair — and no asymmetry
between the two ways of getting a base container.** `test_db()` always
calls the same shared function, on whatever admin URL it has; neither path
has special "requires a pre-existing marker" behavior the other lacks:

```rust
async fn ensure_template_migrated(admin_url: &str) -> anyhow::Result<()>;
```

Resolving the base URL:

1. If `TEST_DATABASE_URL` is set, use it directly. This is an **optional,
   entirely manual convenience** — someone who wants fewer containers can
   hand-start one (`docker run --name my-test-pg --label stroem.test=true
   -e POSTGRES_USER=postgres -e POSTGRES_PASSWORD=postgres -P
   postgres:11-alpine -c max_connections=200` — the label is what lets
   `scripts/test-clean.sh` find it later, so it isn't optional flavor
   either) and export the variable
   themselves. Nothing in this crate or in CI starts or stops that
   container on their behalf — but `test_db()` still calls
   `ensure_template_migrated` against it exactly as it would its own
   container, so a freshly hand-started, never-migrated container just
   works on the first test that touches it, with no separate manual
   bootstrap step to remember. **The raised `max_connections` is the human's
   responsibility here, not something `ensure_template_migrated` can set on
   a server it didn't start** — the `-c max_connections=200` above isn't
   optional flavor text, it's required for this path to be safe under any
   real concurrency, and doubly so if several test binaries are pointed at
   the same manually-shared server at once (§ 4's sizing rule).
2. Otherwise — the default, and the only path CI or an unmodified local
   `cargo test` ever takes — a per-*binary* `tokio::sync::OnceCell`-guarded
   container: start one labelled `Postgres::default()` (via
   `ImageExt::with_label`, confirmed present in the pinned `0.27.3`), raised
   server-side `max_connections` (§ 4), then use its URL the same way.
   - Not expected to be removed by `Drop` at process exit — `static` values
     are never dropped when a Rust binary exits, for the same reason the
     original `Box::leak` bug existed in the first place. Cleanup is
     `scripts/test-clean.sh` (§ 3.2), run by a human. This is the accepted
     cost of the whole design: roughly one container per test binary
     (~27-30 for a full workspace run, not hundreds) that needs periodic
     manual sweeping, in exchange for zero wrapper-script complexity.

`stroem_test_marker` lives in `postgres`, never in `stroem_template` itself
— querying it never requires a second connection into the template, which
would collide with a concurrent `CREATE DATABASE ... TEMPLATE`:

```sql
CREATE TABLE IF NOT EXISTS stroem_test_marker (
    template_name      text PRIMARY KEY,
    migration_fingerprint text NOT NULL,
    created_at          timestamptz NOT NULL DEFAULT now()
);
```

`migration_fingerprint` comes from a new `pub fn migration_fingerprint() ->
String` in `stroem_db`, alongside the existing `run_migrations` — both read
the same compile-time `sqlx::migrate!("./migrations")` data (hashing every
embedded migration's checksum together), so the two can never drift apart;
`stroem-test-support` never constructs its own `Migrator` or guesses at a
version number.

`ensure_template_migrated`, over a connection to `postgres`: takes a
**Postgres advisory lock** (a fixed key, e.g. `pg_advisory_lock(<constant>)`)
**first, before anything else** — including before `CREATE TABLE IF NOT
EXISTS stroem_test_marker`. Creating the table itself is not safe to leave
outside the lock: Postgres does not guarantee `CREATE TABLE IF NOT EXISTS`
is race-free under concurrent execution (two sessions can both see "not
yet created" and both attempt the `CREATE`, one of them erroring). The lock
is held across the *entire* sequence — table creation, the marker check,
template creation/migration, and the marker write — and released only at
the end. This is the one new piece revision 11 adds, and it's needed only
because of path 1 above: the default per-binary container is only ever
touched by one process, so nothing can race on migrating it, but a
manually shared container (`TEST_DATABASE_URL`) can have several test
binaries calling `ensure_template_migrated` against it concurrently —
without serializing the *whole* sequence (not just the marker check), two
could race on table creation or both see "no marker row" and both try to
create/migrate `stroem_template` at once. Inside the lock: create the
table if absent; if the row already matches the current
`migration_fingerprint`, return; otherwise creates/recreates
`stroem_template`, connects to it **on a separate, short-lived pool**, runs
`stroem_db::run_migrations`, **explicitly closes that pool**
(`pool.close().await` — load-bearing: Postgres refuses `CREATE DATABASE ...
TEMPLATE` while any session holds the source open), then writes/updates the
`stroem_test_marker` row, then releases the lock. No connection to
`stroem_template` is ever opened again after this function returns.

Per-test isolation, both paths, over a direct (non-pooled, non-transactional)
admin connection to `postgres` — Postgres rejects `CREATE DATABASE` inside a
transaction block:

```sql
CREATE DATABASE "t_<uuid>" TEMPLATE stroem_template;
```

No explicit `DROP DATABASE` after each test: every database this scheme
creates lives inside a container whose entire writable layer is discarded
when that container is removed. See § 4 for the capacity implications —
including why `test_db()`'s own pool is deliberately *not*
`stroem_db::create_pool`'s production-sized default.

### 3.2 `scripts/test-clean.sh`

The only script this design adds. Run by a human, on demand, never
automatically — not by `cargo test`, not by CI:

```
scripts/test-clean.sh [--max-age <duration>]
```

Lists every `stroem.test=true` container with its age (`docker ps -a
--filter label=stroem.test=true`, age from `docker inspect`'s
`.State.StartedAt`), removes those older than `--max-age` (default 5
minutes — short enough to catch a crash from a few minutes ago, long enough
not to catch a run that's merely slow to start), and prints what it did.

This is an **explicit, potentially destructive selection a human chooses to
make** — a plain age threshold, with no liveness signal behind it at all.
It is not a safety guarantee and isn't described as one: an age check
cannot know whether an older container is still legitimately in use, only
how old it is. Running it in good faith (after a `docker ps` glance if in
doubt) is what makes it a reasonable tool for this problem, not any
property of the check itself. With the per-binary design as the only
mechanism (§ 3.1), this is also the **normal, periodic cleanup path**, not
just a rare crash backstop — running `cargo test --workspace` repeatedly
over a work session will accumulate roughly one container per distinct
test binary run, and this is how you sweep them up. Run it after whatever
test run you have in flight has actually finished, not while one is still
running — the default `--max-age` doesn't know the difference (§ 4).

### 3.3 CI

`.github/workflows/ci.yml`'s `test` job:
- Removes the unused `services: postgres:17` block and `DATABASE_URL` env var
  (§ 1, dead config) — nothing reads it, and nothing in this design needs a
  pre-started service container.
- Otherwise unchanged: still just `cargo test --workspace`. There is no
  wrapper script to call instead — every test binary manages its own
  container exactly as it would locally. `scripts/test-clean.sh` is never
  invoked in CI: every run is on a fresh, ephemeral GitHub-hosted runner
  with nothing left over from a previous run, and the runner's own teardown
  discards anything left behind regardless.

### 3.4 Call-site migration

Every DB-backed test that starts its own Postgres container today is
migrated to `stroem_test_support` — not just the ~27 files under `tests/`
directories named in earlier revisions, but every call site a repo-wide
sweep finds, confirmed to include at least one in-module unit test:
`crates/stroem-server/src/settlement/hooks.rs`'s
`hook_chain_depth_counts_hook_links_across_intermediate_task_levels` (and
any siblings in that `#[cfg(test)] mod tests` block) calls
`Postgres::default().start()` directly, inside `src/`. The implementation
step for this section is a fresh `rg 'Postgres::default\(\)|testcontainers::runners'`
across the whole workspace (`src/` and `tests/` alike) to produce the
definitive list, rather than reusing an earlier hand count.

- Call sites that only need a pool (the majority) use `test_pool().await`.
- Call sites that build a real server/app and need a `DATABASE_URL`-shaped
  config value — confirmed at `crates/stroem-server/tests/integration_test.rs`'s
  `setup()` and `crates/stroem-e2e/tests/harness.rs`'s `TestEnv::setup()` —
  use `test_db().await` and take both `.pool` and `.url`.

`crates/stroem-db/tests/common/mod.rs` keeps its `create_job` convenience
helper but delegates `setup_db()` to the shared crate (or is removed in
favor of direct calls — decided during planning).

## 4. Edge cases

- **Port conflicts**: each container lets Docker assign the published port
  (`-P`) and reads it back — no fixed port assumption.
- **Template quiescence, with no re-opened connection to check it**:
  `ensure_template_migrated` explicitly closes its migration pool before
  returning — load-bearing. The marker lives in `postgres`, not
  `stroem_template`, so checking it never requires reconnecting to the
  template.
- **Concurrent `CREATE DATABASE … TEMPLATE …` within one binary, or across
  several sharing a manual `TEST_DATABASE_URL`**: multiple `#[tokio::test]`s
  — in the same binary, or (for the optional manual path) in several
  binaries pointed at the same hand-started container — can issue `CREATE
  DATABASE … TEMPLATE …` concurrently. Safe for the same reason as always:
  every connection other than the one-time migration targets `postgres`,
  never the template itself. The migration step specifically (not the
  per-test clones) is additionally guarded by the advisory lock in § 3.1,
  needed only for the manual multi-binary case — the default per-binary
  container is never touched by more than one process, so nothing can race
  on *its* migration. Still an inference from the connection pattern for
  the clone step, not a load-tested guarantee; worth a quick concurrent
  test during implementation.
- **Connection capacity**: the largest existing integration test binary has
  411 tests *total* — not 411 concurrent; Rust's default test-harness
  concurrency is bounded by available parallelism (roughly core count), not
  the test count, so actual simultaneous connections are far lower.
  Reusing `stroem_db::create_pool`'s production defaults (5–20 connections
  *per test*) would still be wrong, though — a test needs at most a couple
  of connections briefly, not a production-sized pool. `test_db()` uses its
  own small, fixed pool (e.g. `max_connections(2)`, no separate
  `min_connections`) rather than `stroem_db::create_pool`'s defaults, and
  the default per-binary container (§ 3.1, path 2) is started with a raised
  server-side `max_connections` (e.g. `200`) for headroom beyond Postgres
  11's stock default of 100 — comfortable for a GitHub-hosted runner's core
  count at 2 connections/test. **This raised limit only covers the default
  path.** For the optional manual `TEST_DATABASE_URL` path, raising
  `max_connections` is the human's own responsibility (§ 3.1's `docker run`
  example includes it) — `ensure_template_migrated` has no way to configure
  a server it didn't start. Sizing rule for either path on an unusually
  high-core host, or for several test binaries deliberately pointed at one
  shared manual server at once: budget roughly `2 × (concurrent test
  threads across every binary touching that server)` connections, plus a
  handful for admin/migration use, and raise `max_connections` accordingly.
  A representative stress run of the largest binary is still worth doing
  during implementation to confirm these numbers on real hardware, but the
  design no longer depends on that test to decide what to build.
- **Database count per container**: one test file's worth of databases
  accumulate inside its own container over that binary's run, discarded
  with the container whenever it's eventually removed — not a leak, flagged
  so it isn't mistaken for one.
- **`TEST_DATABASE_URL` set but template missing/stale**: no longer
  possible to hit as an error — `ensure_template_migrated` (§ 3.1) now
  migrates a fresh or stale template the same way for both paths, instead
  of requiring a separate manual bootstrap step.
- **Crash recovery and routine accumulation**: no automatic recovery for
  either case — by design (§ 2 non-goals). Both are handled the same way,
  with `scripts/test-clean.sh` (§ 3.2), a deliberate human decision.
- **A `scripts/test-clean.sh` run while a test is legitimately still
  starting up** (e.g. run from habit in another terminal moments after
  starting a slow `cargo test --workspace`): possible with the default
  5-minute `--max-age`, since the heuristic has no liveness signal at all —
  the accepted trade-off of a human-gated, deliberately simple tool. A
  `docker ps` check before running it, or a larger `--max-age`, avoids this.

## 5. Out of scope / deferred

- One container for an entire `cargo test --workspace` run via an automatic
  wrapper script, and the automatic cross-invocation reaper that would have
  supported it (§ 2 non-goals; cut across revisions 6 and 10 after nine
  rounds of genuine but increasingly narrow cross-platform bash/POSIX
  hardening — see the revision history for what was tried and why it was
  judged not worth the ongoing correctness burden). A manually-managed
  shared container via `TEST_DATABASE_URL` (§ 3.1) remains available for
  anyone who wants it, without this design orchestrating it.
- Fixing the `watchdog` feature's parallel-startup panic, or any other
  in-process signal-handling cleanup mechanism.
- Cleanup of test databases created against a manually-exported
  `TEST_DATABASE_URL` pointing at a server this design doesn't manage.
