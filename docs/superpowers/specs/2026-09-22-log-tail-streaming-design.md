# Log reads: tail by default, streamed full log

Status: revision 1, proposed (2026-09-22)

Companion of `docs/internal/TODO.md` § Performance ("Opening the job page of a
job with a large log OOM-kills every server replica") and of the HA log
mirroring record in TODO.md § "Review: HA Log Mirroring (2026-05-21)".

## Revision history

**Revision 1 (2026-09-22).** Initial design, decided section by section with
the user (API contract, server internals, UI, testing/rollout). Awaiting the
first Codex review.

## 1. Problem

On 2026-09-22 05:56–06:00 UTC both production `stroem-server` replicas
(v0.16.5, 512 Mi limit, ~400 MiB resident baseline) were OOM-killed three
times each while a user had the job page of `jobs/tractor-doohclick` job
`cafa14ba` open. Each freshly restarted replica died 10–25 s after becoming
Ready: the browser's next poll landed on it.

The mechanism is entirely in the read path:

- The task writes ~475 k log lines per run: 83.3 MiB of JSONL (measured on
  the 2026-09-21 archive object, 6.5 MiB gzipped). Because the HA mirror
  appends every peer chunk to the local file
  (`crates/stroem-server/src/events.rs:415-449`), EVERY replica holds the
  full 83 MiB, not a share of it.
- `ui/src/components/step-detail.tsx:85-92` polls
  `GET /api/jobs/{id}/steps/{step}/logs` every 2 s while the step runs.
- `LogStorage::get_step_log` (`crates/stroem-server/src/log_storage.rs:421`)
  → `filter_step_from_file_if_present` (`:475`) reads the file line by line
  but accumulates every matching line into ONE `String`. The handler
  (`crates/stroem-server/src/web/api/jobs.rs:561`) wraps that string in
  `json!({"logs": ..})` (a second copy) and axum serialises it (a third):
  roughly 200 MB per poll against ~100 MiB of headroom.
- `get_job_logs` (`jobs.rs:597`), the WebSocket backfill
  (`crates/stroem-server/src/web/api/ws.rs:160-167`, the whole log as one
  text frame), the MCP tool `get_job_logs`
  (`crates/stroem-server/src/mcp/tools.rs:604`, no truncation) and the CLI
  `stroem-api logs` (`crates/stroem-cli/src/remote/logs.rs:7`) all have the
  same shape. Any of them, from any client, reproduces the outage.
- For a terminal job `get_log`/`get_step_log` additionally gunzip the whole
  archive object into memory (`log_storage.rs:302`, `:523`) and run
  `merge_jsonl_logs` (`:31`): a `HashSet` of every line plus a sort — the
  most memory-hungry read of all, reachable from the same endpoints.

No read path today bounds memory or streams to the client. There is no
`tail`, `offset`, `limit` or `since` parameter anywhere in the read API.

Two facts that shape the fix:

- **The UI never calls the whole-job endpoint or the WebSocket.** It calls
  only the per-step endpoint, for the ONE expanded step, plus the `_server`
  pseudo-step every 3 s (`ui/src/components/server-events.tsx`). `getJobLogs`
  in `ui/src/lib/api.ts:341` is a dead export; the WS hook was deleted
  (TODO.md § HA Log Mirroring). The whole-job endpoint and the WS exist for
  the CLI, the MCP tool, `websocat`-style consumers and scripts.
- **Byte offsets are not stable across replicas.** Each replica's local file
  interleaves direct worker pushes and mirrored peer chunks in its own arrival
  order, so an offset obtained from one replica means nothing on the next
  request behind the load balancer. Any design that pages or delta-polls by
  offset is wrong in HA.

## 2. Goals and non-goals

Goals:

1. No log read, from any client, can allocate more than a small fixed amount
   of memory on the server, whatever the log size. The bound is stated per
   read mode in § 3.4.
2. The UI keeps a live, auto-following view of a running step, and can still
   show the whole log of a long run on demand.
3. Every existing invariant of the read path survives unchanged: `NotFound`
   → empty (retention TOCTOU), archive errors degrade to local, archive only
   for terminal jobs, exact-match step filter, `_server` as a pseudo-step,
   "keep last non-empty body" in the UI.
4. Existing callers keep working: the `logs` field stays; the JSON envelope
   stays for every request that does not opt into streaming.

Non-goals (tracked separately in TODO.md, not touched here):

- The recovery sweep's missing startup grace period (an outage longer than
  `heartbeat_timeout_secs` + one sweep interval fails every running step of
  healthy workers).
- Worker-side retry of a failed log push (`stroem-worker/src/poller.rs`
  drains the buffer before the push and drops the batch on error).
- Delta polling / paging by offset (impossible in HA, see § 1).
- Changing the NOTIFY mirroring, the 3 500-byte segment cap or the archive
  upload path.
- Raising the production memory limit (an operations change; the helmfile
  already says 1 Gi/2 Gi, the deployed release still runs 256 Mi/512 Mi).
- Virtualising any other list in the UI.

## 3. Design

### 3.1 API contract

Both REST endpoints keep their URL, auth and ACL (`View`; `Deny` → 404):

- `GET /api/jobs/{id}/logs`
- `GET /api/jobs/{id}/steps/{step}/logs` (`_server` remains a pseudo-step)

Two new query parameters, mutually exclusive:

| Parameter | Type | Meaning |
|---|---|---|
| `tail_bytes` | integer | Size of the tail to return. Default `log_storage.tail_default_bytes` (256 KiB). `0`, a value above `log_storage.tail_max_bytes` (4 MiB), or a non-integer → 400 `BadRequest`. |
| `full` | `true` | Stream the whole log. Combined with `tail_bytes` → 400. |

**Tail mode** (no parameters, or `tail_bytes`): the response is the existing
JSON envelope plus three fields. Nothing that reads only `logs` breaks.

```json
{
  "logs": "<JSONL, whole lines only>",
  "truncated": true,
  "total_bytes": 87325871,
  "returned_bytes": 262011
}
```

- `logs` contains only whole lines: the tail is cut at the first newline
  after the seek point. A single line longer than the tail window yields an
  empty `logs` with `truncated: true` (pathological; raise `tail_bytes`).
- `truncated` is `true` when the source was NOT read completely — i.e. there
  MAY be earlier lines. For a step tail it can be `true` even when no earlier
  line of that step exists (§ 3.2 explains why); `false` is exact.
- `total_bytes` is the size of the source that was consulted (the local file
  length; or the decompressed length of the archive object when the archive
  served the tail). It is the size of the JOB log, not of the step's share.
- `returned_bytes` is `logs.len()`.

**Full mode** (`full=true`): a streamed body, `Content-Type:
application/x-ndjson; charset=utf-8`, no JSON envelope, raw JSONL exactly as
stored (legacy `.log` plain-text files stream as-is under the same content
type; consumers already tolerate non-JSON lines). An empty log is a 200 with
an empty body. The step endpoint applies its filter inside the stream.

**Every response** of both endpoints, both modes, carries
`X-Stroem-Log-Source: local | archive | merged | none`, naming which path
served it (§ 3.3). This exists so that a "lines are missing" report can be
diagnosed without guessing which replica or source answered.

**WebSocket** `GET /api/jobs/{id}/logs/stream`: the backfill frame becomes
the default tail (same rules as tail mode, whole lines, same source rules).
No control frame is added — external consumers read raw JSONL frames today
and stay compatible. `skip_backfill` is unchanged. Documented as a behaviour
change: a fresh connection no longer receives the whole history.

**MCP `get_job_logs`**: gains optional `tail_bytes` (same validation, an
invalid value is `invalid_params`). No full mode: an agent context never
wants 83 MiB. When the read was truncated the formatted text ends with one
extra line, `[truncated: showing the last 256.0 KiB of 83.3 MiB — earlier
lines omitted]`, produced after `format_logs` so the existing formatter and
its tests are untouched.

**CLI `stroem-api logs <job_id>`**: prints the tail by default and, when
truncated, one line on stderr: `note: showing the last 256.0 KiB of 83.3 MiB;
use --full for the whole log`. New flags `--full` (streams the NDJSON body to
stdout as it arrives) and `--tail-bytes <N>`. `--full` against an OLD server
(which ignores the parameter and answers with the JSON envelope) is detected
by `Content-Type` and printed from the envelope, so a new CLI works against
both. An OLD CLI against a NEW server prints a tail without a notice; release
note.

### 3.2 Server read modes

`LogStorage` gains one enum and two entry points; the old
`get_log`/`get_step_log` (String-returning) are removed together with their
production callers so no unbounded read path is left to regress to. Their
unit tests migrate to the new entry points.

```rust
pub enum StepFilter<'a> { All, Step(&'a str) }

pub struct Tail {
    pub logs: String,        // whole lines, <= tail_bytes (+ one line for the step scan, see below)
    pub truncated: bool,
    pub total_bytes: u64,
    pub source: LogSource,   // Local | Archive | None
}

pub async fn read_tail(&self, job_id, meta, is_terminal, filter, tail_bytes) -> Result<Tail>;
pub async fn stream_full(&self, job_id, meta, is_terminal, filter)
    -> Result<(LogSource, BoxStream<'static, io::Result<Bytes>>)>;
```

**Unfiltered tail (`StepFilter::All`).** `metadata().len()` → seek to
`max(0, len - tail_bytes)` → read to EOF into a buffer of at most
`tail_bytes` → if the seek point was `> 0`, drop everything up to and
including the first `\n`. One buffer, `<= tail_bytes`.

**Step tail (`StepFilter::Step`).** A quiet step next to a chatty one would
return nothing from the last 256 KiB, so the step tail scans BACKWARDS in
windows of `tail_bytes`: read the last window, split it at newlines (the
partial first line is carried into the next window), test each whole line
with the existing `line_matches_step` (`log_storage.rs:508` — the `contains`
fast guard, then a `serde_json` parse and exact `step` equality, so `build`
never matches `build-docs`), collect matching lines newest-first, and stop
when the collected bytes reach `tail_bytes`, or the scan reaches the start of
the file, or the scan has covered `log_storage.tail_scan_max_bytes` (64 MiB).
Output is reversed back into file order. `truncated` is `true` unless the scan
reached the start of the file — so a truncated step tail may in fact be
complete; the UI wording ("showing the last …") is chosen for that. Bound:
one window + the result (`<= tail_bytes` plus the line that crossed the
threshold) + one carried partial line (a carried line longer than 1 MiB is
dropped with a `warn!`, matching the full-mode cap below). The scan cap keeps
a 3 s `_server` poll on a gigabyte log from re-reading the whole file every
tick; 64 MiB covers this incident's log in one scan.

**Full, unfiltered.** `tokio::fs::File` → `tokio_util::io::ReaderStream`
(8 KiB chunks) → `axum::body::Body::from_stream`. Nothing is buffered beyond
the chunk.

**Full, filtered.** `FramedRead` with
`LinesCodec::new_with_max_length(1 MiB)`; each decoded line goes through
`line_matches_step`; matches are re-emitted with their `\n`. A line over
1 MiB is skipped (the codec's `MaxLineLengthExceeded` is logged at `warn!`
and the stream continues with the next line). Unfiltered full mode has no
such cap because it never splits lines. `tokio-util` gains the `codec` and
`io` features (workspace `Cargo.toml`).

**Missing file.** Both entry points map `NotFound` on the local file to an
empty result with `source: None` (tail) or an empty stream (full), never to a
500 — retention can delete the file mid-request (`log_storage.rs:380-385`
keeps its comment and its behaviour). Every other I/O error propagates as it
does today. Legacy `.log` fallback order is unchanged: `.jsonl` first, then
`.log`.

### 3.3 Terminal jobs and the archive

Today a terminal job's read is a line-level union of the local file and the
archive (`merge_jsonl_logs`): each replica's local file may hold only the
chunks that reached it, the archive holds what the orchestrating replica had
at terminal time, and the union is the most complete view. That invariant
is kept for every normal-sized log and bounded for the rest:

- **Tail, local file present:** served from the local file only. The
  archive is not touched. A partial replica may therefore show a tail that
  lacks mirrored-away lines; the full read (below) still unions them, and
  `X-Stroem-Log-Source: local` says what happened.
- **Tail, local file missing:** the archive object is streamed through a
  gunzip decoder into a ring buffer of `tail_bytes`, counting the total
  decompressed length for `total_bytes`, then cut at the first newline. For a
  step tail the ring buffer holds matching lines only (same
  `line_matches_step`, newest-first eviction). Bound: `2 × tail_bytes` plus
  the decoder window. `source: archive`.
- **Full:** if BOTH local and archive exist and
  `local_len + 16 × archive_object_len <= log_storage.merge_max_bytes`
  (32 MiB), the existing in-memory union merge runs, code unchanged, and the
  result is streamed from that one string (`source: merged`). 16 is a
  conservative gzip ratio for JSONL (measured 12.8 on the incident log; a
  lower estimate would under-count and merge too much). Above the cap the
  server streams ONE source: the archive first, because it is the
  orchestrating replica's complete file; on an archive error it falls back
  to the local stream exactly as the current code degrades archive errors to
  local (`log_storage.rs:358-366`). `source: archive` or `local`.
- Non-terminal jobs never touch the archive (unchanged; the upload happens
  at terminal time, so a pre-terminal archive read is a guaranteed miss).

**`BlobArchive` additions** (`crates/stroem-server/src/blob_storage.rs:24`):

- `async fn size(&self, key) -> Result<Option<u64>>` — `S3BlobArchive`
  (`:279`) via `head_object`, `LocalBlobArchive` (`:108`) via `metadata`,
  the in-memory/test archives via their maps. Needed for the merge decision
  without fetching the object.
- Real streaming `get_stream` overrides: the trait's default (`:50`) buffers
  the whole object through `get`, which would defeat the archive tail and
  the archive full stream. `S3BlobArchive` returns the SDK `ByteStream` as a
  `BoxStream`; `LocalBlobArchive` a `ReaderStream` over the file.
  `put_stream` is out of scope.

The gunzip decoder is `async_compression::tokio::bufread::GzipDecoder`
(new dependency `async-compression`, features `tokio`, `gzip`), wrapped over
`StreamReader` of the archive stream.

### 3.4 Memory bounds

| Read | Bound per request |
|---|---|
| Tail, unfiltered, local | `tail_bytes` |
| Tail, step, local | window `tail_bytes` + result `<= tail_bytes` + one line (`<= 1 MiB`) |
| Tail from archive | `2 × tail_bytes` + gunzip window (32 KiB) |
| Full, unfiltered | one 8 KiB chunk |
| Full, filtered | codec buffer `<= 1 MiB` |
| Full, merged (terminal, under cap) | `merge_max_bytes` (32 MiB) + the merge's `HashSet`, as today but capped |

With defaults, the worst request costs about 33 MiB (the capped merge) and a
typical UI poll costs 256 KiB, against ~100 MiB of headroom in production
today. The merge cap is the only bound above 4 MiB and is reachable only
through `full=true` on a terminal job.

### 3.5 Configuration

Under `log_storage:` (`crates/stroem-server/src/config.rs:51`), all
optional with defaults; `ServerConfig::validate` rejects `tail_default_bytes
> tail_max_bytes`, any zero, and `tail_max_bytes > tail_scan_max_bytes`:

| Key | Default | Meaning |
|---|---|---|
| `tail_default_bytes` | 262144 (256 KiB) | tail size when the request names none |
| `tail_max_bytes` | 4194304 (4 MiB) | largest `tail_bytes` a request may ask for |
| `tail_scan_max_bytes` | 67108864 (64 MiB) | how far back a step tail scans before giving up |
| `merge_max_bytes` | 33554432 (32 MiB) | estimated local + archive size under which a terminal full read still merges in memory |

Env overrides follow the existing convention
(`STROEM__LOG_STORAGE__TAIL_DEFAULT_BYTES`).

### 3.6 UI

**Viewer.** `ui/src/components/log-viewer.tsx` keeps its props, the dark
monospace look, `role="log"` / `aria-live`, the "Waiting for logs…" empty
state and its scroll contract, but the `lines.map` at `:57-60`/`:79`
becomes a `@tanstack/react-virtual` list (new dependency, `^3.14`). Lines
are parsed once per body change into an array of `{ts, stream, step, line}`
or a legacy plain-text marker, instead of being JSON-parsed on every render.
Soft wrapping stays; the row-height estimate is
`ceil(line.length / charsPerRow) × lineHeight`, with `charsPerRow` derived
from one measured character width and the container width, and the
virtualiser's `measureElement` as the correction for the rare row that
differs (tabs, wide glyphs). Auto-follow keeps the existing contract —
follow while within 40 px of the bottom, pause when the user scrolls up —
expressed as `scrollToIndex(last)` instead of setting `scrollTop`.

Pretext (`pretextjs.dev`) was considered for row heights and rejected: it is
a text measurement engine, pre-1.0 (`@chenglou/pretext` 0.0.9), and for a
monospace log the arithmetic above gives the same number.

**Tail banner.** When the response has `truncated: true`, a thin bar at the
top of the viewer reads "Showing the last ~2,100 lines (256 KiB of
83.3 MiB)" — the line count is what the viewer parsed, the sizes come from
`returned_bytes`/`total_bytes` — with two actions:

- **Load full log** — fetches `?full=true` with a progress indicator fed by
  the response's `ReadableStream`, then swaps the viewer to full mode. If
  `total_bytes` exceeds 64 MiB it first asks for confirmation and suggests
  download instead; the browser will hold the whole string.
- **Download** — the authenticated `fetch` → `Blob` → `URL.createObjectURL`
  pattern from `ui/src/components/artifact-list.tsx:79`, saving
  `<job>-<step>.jsonl`; a plain link cannot carry the bearer token.

**Polling.** The 2 s poll (`step-detail.tsx`) and the 3 s `_server` poll
(`server-events.tsx`) keep running, unchanged in cadence, and now receive at
most `tail_default_bytes`. The "keep last non-empty body" guard
(`step-detail.tsx:70-79`, `server-events.tsx:23-31`) is untouched: an empty
body from a cold replica still never clears the view.

- In **tail mode** each poll replaces the body, as today: the tail IS the
  latest view.
- In **full mode** the poll result is stitched on by a pure function
  `appendTail(displayed: string[], tail: string[]): {lines, gap}`:
  1. Find the largest `k` such that the last `k` lines of `displayed` equal
     the first `k` lines of `tail`, comparing whole raw lines (sequence
     equality handles repeated identical lines). Append `tail[k..]`.
  2. If `k == 0` and both are non-empty, fall back to timestamps: append the
     lines of `tail` whose `ts` is later than the last displayed `ts`. If the
     FIRST tail line is already later than the last displayed line, the step
     outran a 256 KiB tail within one poll: also insert a visible "gap"
     divider row so nobody mistakes the view for complete.
  Step 2 exists because a partial replica's tail can lack the last mirrored
  lines the full read had, which would otherwise look like a gap.
  `appendTail` lives in `ui/src/lib/log-tail.ts` with Vitest coverage
  (overlap, repeated lines, gap, partial-replica fallback, empty inputs).

**API client.** `getStepLogs` returns the new envelope type; new
`getStepLogsFull(jobId, step, onProgress)` streams; `getJobLogs` stays
exported and typed but remains unused by the UI.

### 3.7 Compatibility and rollout

- UI and server ship in one binary; no ordering concern.
- Scripts reading `logs` from the envelope keep working and now get a tail;
  `truncated` is the signal. Release note under "Behaviour changes".
- Old CLI vs new server: tail without notice (documented). New CLI vs old
  server: works through the envelope path.
- WS consumers: backfill is a tail; documented in `operations/log-storage.md`
  and `reference/api.md`.
- No migration, no data-format change: the on-disk JSONL, the archive key
  and object format, the NOTIFY protocol are all untouched.

## 4. Decisions

1. **Bytes, not lines.** A line count has no memory bound (one 50 MB line
   defeats it); a byte count is a seek. The UI translates to a line count in
   its wording.
2. **256 KiB default, 4 MiB max.** Two thousand-odd lines is a live view;
   4 MiB is the most a single tail request may ever allocate.
3. **Tail by default for every caller,** with `truncated`/`total_bytes` in
   every envelope, rather than tail only where the UI asks. The other
   callers are exactly the ones that would reproduce the outage, and an
   MCP agent is better served by a tail anyway.
4. **No offsets, no paging.** Offsets are replica-local (§ 1). Every request
   is stateless: tail or full.
5. **Streamed NDJSON for full, not a streamed JSON string.** Escaping a JSON
   string while streaming is possible but pointless; `full=true` is an
   opt-in, so it may have its own content type.
6. **Single source above the merge cap.** A streaming union is impossible
   without sorted inputs (the local file is in arrival order, not `ts`
   order), and an external sort is disproportionate. The archive is the more
   complete source when mirroring worked; the header says which was served.
7. **Keep the in-memory merge under the cap.** Removing it would silently
   drop the HA-recovery behaviour for the common case where it costs nothing.
8. **Virtualised viewer that can load the whole log** (the user's choice
   over "page backwards" and "download only"), TanStack Virtual, no Pretext.
9. **Remove the String-returning read functions** so the unbounded path
   cannot be reintroduced by a future caller.

## 5. Testing

Unit (`log_storage.rs`):
- tail cut at a line boundary; tail larger than the file returns the whole
  file with `truncated: false`; empty file; a single line longer than the
  window → empty + truncated.
- step tail: matching line straddling a window edge; quiet step behind a
  chatty one found beyond the first window; scan cap reached → truncated;
  scan reaching the file start → not truncated; `build` vs `build-docs`.
- missing file → empty, `source: None`, no error (retention TOCTOU).
- terminal + local present: archive never called (mock archive counts
  calls); terminal + local missing: ring-buffer tail from the archive with
  correct `total_bytes`; step tail from archive.
- full filtered: line over 1 MiB skipped, following line delivered.
- `choose_full_source(local_len, archive_len, cap)` as a pure function.
- `LinesCodec`/`ReaderStream` streams deliver more than one chunk for a body
  larger than the chunk size (guards against an accidental buffering
  adapter).

Integration (`crates/stroem-server/tests/integration_test.rs`):
- default REST response carries `truncated`, `total_bytes`, `returned_bytes`
  and `X-Stroem-Log-Source`; `?full=true` answers `application/x-ndjson`
  with bytes equal to the file; `tail_bytes=0`, above max, non-numeric, and
  `tail_bytes` + `full` → 400; step endpoint in both modes; `_server`.
- WS backfill is a tail (`test_ws_backfill_existing_logs` updated; a new
  test with a body larger than the default proves the cut).
- MCP: trailer present when truncated, absent otherwise; `tail_bytes`
  validation.
- Terminal job under the cap → `merged` and the union content; above the
  cap → `archive` with the archive's content; archive error above the cap →
  `local`.
- `ha_test.rs` mirror tests and `s3_integration_test.rs` pass unchanged;
  one new S3 test streams a real gzipped object into a tail.
- `size()` on the local and S3 backends.

Memory regression (the test this incident needs):
- `#[ignore]`d, Linux-only integration test that writes a 100 MiB synthetic
  JSONL log, reads `/proc/self/statm` before and after a `full=true` read
  and a step tail, and asserts RSS grew by less than 16 MiB. Run on demand
  and in the release checklist.

CLI: `--full` handles NDJSON and the JSON envelope; the truncation notice
goes to stderr; `--tail-bytes` is forwarded.

UI (Vitest): `appendTail` cases listed in § 3.6; banner rendered when
`truncated`, absent otherwise; confirmation above 64 MiB; `step-detail.test.tsx`
updated for the envelope; a `log-viewer` test that a 10 000-line body renders
fewer than 200 rows (virtualisation is in effect; jsdom needs the size mock
the TanStack docs describe). Playwright (`ui/e2e/log-streaming.spec.ts`):
banner appears for a log above the default tail and "Load full log" renders
the first line.

## 6. Documentation

- `docs/src/content/docs/reference/api.md`: query parameters, the three new
  fields, NDJSON mode, the source header, WS backfill note.
- `docs/src/content/docs/operations/log-storage.md`: the four keys, the new
  read order and source rules, the header, the `curl … | jq -r .logs` recipe
  gains `?full=true`.
- CLI help text; `docs/src/content/docs/guides/mcp.md` for `tail_bytes`.
- `CLAUDE.md` § Log Storage / § WebSocket Log Streaming: read modes and
  bounds; correct two stale facts found on the way — the NOTIFY segment cap
  is 3 500 bytes (`events.rs:43-57`), not 7 000, and
  `ui/src/hooks/use-job-logs.ts` no longer exists.
- `CONTEXT.md`: **Tail read**, **Full read**, **Log source**.
- `docs/internal/TODO.md`: mark the incident entry done; keep the non-goals
  listed as open items.
- Release notes: behaviour change for scripts, old CLIs and WS consumers.

## 7. Risks and open points

- **Browser memory in full mode.** An 83 MiB log is ~83 MiB of string plus
  the parsed array; the 64 MiB confirmation is the only guard. Acceptable
  by the user's choice of a virtualised full view; download is one click
  away.
- **Step tails on a huge, quiet-step log** cost a sequential read of up to
  64 MiB every 2 s per open viewer (page cache, `contains` fast path). CPU,
  not memory; the cap bounds it. If it proves expensive, lower
  `tail_scan_max_bytes` — the trade is a `_server` card that may go blank on
  a very large job until "Load full log".
- **`truncated` is conservative for step tails** (§ 3.2). The UI wording
  and the docs say "may have earlier lines".
- **The archive-only tail streams the whole object** (no random access into
  gzip). Bounded memory, but a full S3 download per request for a job whose
  local file is gone. Rare: it needs retention to have deleted the local
  file, or a cold replica, for a terminal job someone is viewing.
- **`total_bytes` is the job log size, not the step's**, even on the step
  endpoint. Computing the step's own size would need a full scan; the
  banner wording uses the job size explicitly.
