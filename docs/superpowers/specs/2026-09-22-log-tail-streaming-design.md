# Log reads: tail by default, streamed full log

Status: revision 3, proposed (2026-09-22)

Companion of `docs/internal/TODO.md` § Performance ("Opening the job page of a
job with a large log OOM-kills every server replica") and of the HA log
mirroring record in TODO.md § "Review: HA Log Mirroring (2026-05-21)".

## Revision history

**Revision 3 (2026-09-22).** After the second Codex review (7 of 19 earlier
findings partial, 9 new). Every memory figure is now derived from a limit the
code enforces; estimates are gone. Decisions changed with the user:
`appendTail` is a union by exact line — the rule `merge_jsonl_logs` already
uses — instead of a timestamp cut, so arrival-ordered tails cannot duplicate
and re-applying a tail is a no-op (N4); merges are additionally capped by a
LINE count, `merge_max_lines`, because the merge's per-line overhead does not
depend on line length (N3). Mechanical tightenings: the local input of a
merge is also read through `take`, and the archive input's `take` is sized
to what remains of the budget (N1); the archive is streamed through
fixed-size range reads the server sizes, replacing the SDK's unbounded chunk
(N3); the line splitter runs on our own `fill_buf`/`consume` loop with one
preallocated accumulator instead of `FramedRead`, whose `BytesMut` doubles
(N2); the step matcher deserialises only the `step` field, borrowed, so no
per-line `Value` (N3); the tail reads exactly the snapshot range and drops a
torn trailing line (N5); `total_bytes` is defined as an upper bound and is
`local + archive` for a merged read (N6); an over-budget match that is left
out sets `truncated` (N7); the archive cost is stated per fetch (N8);
`size` + `get_range` with defined short-object semantics replace
`get_suffix` (N9); `ISIZE` wrap wording made consistent; citations fixed
(server-event lines ARE mirrored; tokio file buffer; `ws.rs:164`/`:167`;
"oldest-first eviction").

**Revision 2 (2026-09-22).** After the first Codex review (19 findings).
Terminal tails union bounded local and archive tails; exact `ISIZE` merge
gate guarded by `take`; local-first single source above the cap; a line
splitter that never yields a codec error; `take` after the seek; migrated
integration-test call sites; WS handoff gap recorded as pre-existing.

**Revision 1 (2026-09-22).** Initial design, decided section by section with
the user (API contract, server internals, UI, testing/rollout).

## 1. Problem

On 2026-09-22 05:56–06:00 UTC both production `stroem-server` replicas
(v0.16.5, 512 Mi limit, ~400 MiB resident baseline) were OOM-killed three
times each while a user had the job page of `jobs/tractor-doohclick` job
`cafa14ba` open. Each freshly restarted replica died 10–25 s after becoming
Ready: the browser's next poll landed on it.

The mechanism is entirely in the read path:

- The task writes ~475 k log lines per run: 83.3 MiB of JSONL (measured on
  the 2026-09-21 archive object, 6.5 MiB gzipped). The HA mirror appends
  every peer chunk that fits a NOTIFY payload to the local file
  (`crates/stroem-server/src/events.rs:415-449`); oversize segments are
  signal-only and stay on the publishing replica (`ha_test.rs:527-537`,
  `:770-780`), and a mirror disk write can fail. So a replica's file is
  typically most of the log, never guaranteed all of it — and for this job
  it was large enough on both replicas to kill both.
- `ui/src/components/step-detail.tsx:85-92` polls
  `GET /api/jobs/{id}/steps/{step}/logs` every 2 s while the step runs.
- `LogStorage::get_step_log` (`crates/stroem-server/src/log_storage.rs:421`)
  → `filter_step_from_file_if_present` (`:475`) reads the file line by line
  but accumulates every matching line into ONE `String`. The handler
  (`crates/stroem-server/src/web/api/jobs.rs:561`) wraps that string in
  `json!({"logs": ..})` (a second copy) and axum serialises it (a third):
  roughly 200 MB per poll against ~100 MiB of headroom.
- `get_job_logs` (`jobs.rs:597`), the WebSocket backfill
  (`crates/stroem-server/src/web/api/ws.rs:164` reads the whole log, `:167`
  sends it as one text frame), the MCP tool `get_job_logs`
  (`crates/stroem-server/src/mcp/tools.rs:604`, no truncation) and the CLI
  `stroem-api logs` (`crates/stroem-cli/src/remote/logs.rs:7`) all have the
  same shape. Any of them, from any client, reproduces the outage.
- For a terminal job the whole-log read additionally gunzips the entire
  archive object into a `String` (`get_log_from_archive`,
  `log_storage.rs:302-317`) and runs `merge_jsonl_logs` (`:31-55`): both
  inputs retained, a `HashSet` and a `Vec` of every line, an output sized
  to both inputs, and a sort. The per-step variant
  (`get_step_log_from_archive`, `:523-547`) buffers the compressed object
  and decompresses line by line, accumulating only matches — smaller, but
  still unbounded in the matches.

No read path today bounds memory or streams to the client. There is no
`tail`, `offset`, `limit` or `since` parameter anywhere in the read API.

Two facts that shape the fix:

- **The UI never calls the whole-job endpoint or the WebSocket.** It calls
  only the per-step endpoint, for the ONE expanded step, plus the `_server`
  pseudo-step every 3 s (`ui/src/components/server-events.tsx`). `getJobLogs`
  in `ui/src/lib/api.ts:341` is a dead export; the WS hook was deleted
  (TODO.md § HA Log Mirroring). The whole-job endpoint and the WS exist for
  the CLI, the MCP tool, `websocat`-style consumers and scripts.
- **Byte offsets are not stable across replicas, and files are not in
  timestamp order.** Each replica's local file interleaves direct worker
  pushes and mirrored peer chunks in its own arrival order
  (`events.rs:424-431`), so an offset from one replica means nothing on the
  next request behind the load balancer, and a window of the file is not
  monotonic in `ts`. Any design that pages by offset, or that treats a
  window as a time range, is wrong in HA.

## 2. Goals and non-goals

Goals:

1. No log read, from any client, can allocate more than a stated amount of
   memory on the server, whatever the log's size or content. Every term of
   every bound in § 3.4 is a constant the code enforces (a `take`, a
   preallocated buffer, a configured cap) — never an assumption about line
   length, compression ratio or upstream chunking.
2. The UI keeps a live, auto-following view of a running step, and can still
   show the whole log of a long run on demand.
3. Every existing invariant of the read path survives: `.jsonl` → legacy
   `.log` → (terminal) archive fallback with `NotFound` never a 500
   (retention TOCTOU), archive errors degrade to local whenever a fallback
   is still possible, archive only for terminal jobs, union recovery of
   mirrored-away lines for terminal jobs whenever it fits the caps,
   exact-match step filter, `_server` as a pseudo-step, "keep last
   non-empty body" in the UI.
4. Existing callers keep working: the `logs` field stays; the JSON envelope
   stays for every request that does not opt into streaming.

Non-goals (tracked in TODO.md, not touched here):

- The recovery sweep's missing startup grace period (an outage longer than
  `heartbeat_timeout_secs` + one sweep interval fails every running step of
  healthy workers).
- Worker-side retry of a failed log push (`stroem-worker/src/poller.rs`
  drains the buffer before the push and drops the batch on error).
- Delta polling / paging by offset (impossible in HA, see § 1).
- Changing the NOTIFY mirroring, the 3 500-byte segment cap or the archive
  upload path.
- The WebSocket backfill-to-live gap: `ws.rs` reads and sends the backfill
  (`:164`, `:167`) BEFORE subscribing to the broadcast (`:177`), so chunks
  pushed in between never reach that connection. Pre-existing; a tail
  backfill neither widens nor closes it. Recorded as an open TODO item.
- Lines appended to a job AFTER its archive upload — e.g. failed-hook
  events written to the original job by `settlement/terminal.rs:215-230`
  while the upload is spawned at `:250`. `server_log`
  (`settlement/mod.rs:119-129`) appends them locally, broadcasts and
  publishes them for mirroring, so peers' files get them too; the archive
  never does. Today and after this change.
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
JSON envelope plus three fields. Nothing that reads only `logs` breaks. The
handler serialises a typed struct (`Json(TailResponse { .. })`), not
`json!({..})`, so the body exists twice (the tail string and the serialised
buffer), not three times.

```json
{
  "logs": "<JSONL, whole lines only>",
  "truncated": true,
  "total_bytes": 87325871,
  "returned_bytes": 262011
}
```

- `logs` contains only whole lines and is at most `tail_bytes` long. A
  single line longer than the tail window yields an empty `logs` with
  `truncated: true` (pathological; raise `tail_bytes`).
- `truncated` is `true` when anything that exists was left out: a consulted
  source was not read from its start, a step-tail scan stopped at its cap,
  a match was left out for budget (§ 3.2), or the merged tail was cut
  (§ 3.3). For a step tail it can be `true` even when no earlier line of
  that step exists; `false` is exact.
- `total_bytes` is an UPPER BOUND on the size of the complete job log, from
  the sources consulted: the local file's length snapshot; the archive's
  decompressed length; their SUM when both were consulted (a union of two
  sources is at most their sum; `max` would be wrong for disjoint sources).
  `returned_bytes <= total_bytes` always holds. It is the size of the JOB
  log, not of the step's share.
- `returned_bytes` is `logs.len()`.

**Full mode** (`full=true`): a streamed body, `Content-Type:
application/x-ndjson; charset=utf-8`, no JSON envelope, raw JSONL exactly as
stored (legacy `.log` plain-text files stream as-is under the same content
type; consumers already tolerate non-JSON lines). An empty log is a 200 with
an empty body. The step endpoint applies its filter inside the stream. A
source failure AFTER the first byte ends the body early (chunked transfer
aborts); there is no mid-stream fallback (§ 3.3).

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
extra line, `[truncated: showing the last 256.0 KiB of up to 83.3 MiB —
earlier lines omitted]`, produced after `format_logs` so the existing
formatter and its tests are untouched.

**CLI `stroem-api logs <job_id>`**: prints the tail by default and, when
truncated, one line on stderr: `note: showing the last 256.0 KiB of up to
83.3 MiB; use --full for the whole log`. New flags `--full` (streams the
NDJSON body to stdout as it arrives) and `--tail-bytes <N>`. `--full`
against an OLD server (which ignores the parameter and answers with the JSON
envelope) is detected by `Content-Type` and printed from the envelope, so a
new CLI works against both. An OLD CLI against a NEW server prints a tail
without a notice; release note.

### 3.2 Server read modes

`LogStorage` gains one enum and two entry points; the old
`get_log`/`get_step_log` (String-returning) are removed together with their
production callers so no unbounded read path is left to regress to. Their
unit tests and the nine integration-test call sites (`s3_integration_test.rs`
`:143`, `:188`, `:256`, `:259`; `ha_test.rs` `:318`, `:383`, `:534`;
`integration_test.rs` `:29180`, `:29498`) migrate to the new entry points
with their assertions unchanged.

```rust
pub enum StepFilter<'a> { All, Step(&'a str) }

pub struct Tail {
    pub logs: String,        // whole lines, len() <= tail_bytes
    pub truncated: bool,
    pub total_bytes: u64,    // upper bound, see § 3.1
    pub source: LogSource,   // Local | Archive | Merged | None
}

pub async fn read_tail(&self, job_id, meta, is_terminal, filter, tail_bytes) -> Result<Tail>;
pub async fn stream_full(&self, job_id, meta, is_terminal, filter)
    -> Result<(LogSource, BoxStream<'static, io::Result<Bytes>>)>;
```

Constants used below (code constants unless listed in § 3.5): `T =
tail_bytes`; `L = max_line_bytes`; `K = 64 KiB`, the output chunk; `R =
1 MiB`, the archive range-read size; `C = merge_max_bytes`; `N =
merge_max_lines`.

**Local file resolution** (both entry points): `.jsonl` first, then legacy
`.log`, exactly the order of today's `read_local_log` (`log_storage.rs:386`).
`NotFound` on both is `Ok(None)` — retention can delete the file mid-request
(`:380-385` keeps its comment and its behaviour) — and the caller falls
through to the archive when the job is terminal (§ 3.3), else to an empty
result with `source: None`. Every other I/O error propagates as today
(`:393-409`), including one that occurs mid-read.

**The step matcher.** `line_matches_step` (`:508-516`) keeps its contract —
the `contains` fast guard, then the line must parse as JSON whose `step` is
a string exactly equal to the name, so `build` never matches `build-docs` —
but its body changes from `serde_json::from_str::<Value>` to a borrowed
`#[derive(Deserialize)] struct StepField<'a> { #[serde(borrow)] step:
Option<Cow<'a, str>> }`. serde skips every other field without allocating,
and `step` borrows unless it contains escapes, so matching a line of `n`
bytes allocates at most `n` bytes. That is what lets the bounds below carry
`L` instead of a parse multiplier.

**Unfiltered tail (`StepFilter::All`), local.** `len = metadata().len()` is
the snapshot and `total_bytes`. `start = max(0, len - T)`; `seek(start)`;
`take(len - start).read_to_end(&mut buf)` with `buf` preallocated to `len -
start <= T` — exactly the snapshot range `[start, len)`, never a byte past
it. `append_log` (`:192-199`) writes and flushes under its own handle lock
that readers do not share, so the file can grow after the snapshot; those
bytes are left for the next poll, which keeps `returned_bytes <=
total_bytes`. Then: if `start > 0`, drop everything up to and including the
first `\n`; if the buffer does not end in `\n`, drop the trailing partial
line (a chunk write in progress at snapshot time can leave a torn last
line; the writer terminates every line, so a complete tail always ends in
`\n`, and the dropped bytes return whole on the next poll). `truncated =
start > 0`. Peak: `buf` (`<= T`) + tokio's file buffer (`<= min(T, 2 MiB)`,
sized to the read request, `tokio-1.52.3/src/fs/file.rs:290`, `:620-628`)
= **2 T**.

**Step tail (`StepFilter::Step`), local.** A quiet step next to a chatty one
would return nothing from the last 256 KiB, so the step tail scans BACKWARDS
over the snapshot range in windows of `T`, each read with `seek` + `take`
into one reused window buffer of `T`. Each window is split at newlines; the
partial first line is carried into the next window in a carry buffer capped
at `L` (a longer carried line is dropped with a `warn!` and counts as
truncation). Each whole line is tested with `line_matches_step`; matches are
collected newest-first into a result buffer preallocated to `T`. The scan
stops when the next match would push the result OVER `T` (that match is NOT
taken and sets `truncated`), when it reaches the start of the file, or when
it has covered `tail_scan_max_bytes` (64 MiB, sets `truncated`). Output is
reversed back into file order. `truncated = !reached_start ||
excluded_match || scan_cap_hit || carried_line_dropped`; so a `false` is
exact and a `true` may be conservative (an earlier line of that step may
not exist). The cap keeps a 3 s `_server` poll on a gigabyte log from
re-reading the whole file every tick; 64 MiB covers this incident's log in
one scan. Peak: window `T` + file buffer `T` + result `T` + carry `L` +
matcher `L` = **3 T + 2 L**.

**Full, unfiltered, local.** `tokio::fs::File` →
`ReaderStream::with_capacity(_, K)` → `axum::body::Body::from_stream`
(polls one item per frame, `axum-core` `body.rs:209-218`). Tokio's file
buffer is sized to the request, so `<= K`. Peak **2 K**. It never
materialises the log.

**Full, filtered, local.** Not `FramedRead`: its `BytesMut` grows by
doubling (`tokio-util-0.7.18/src/codec/framed_impl.rs:215`,
`bytes-1.12.0/src/bytes_mut.rs:700-760`) and it ends the stream after any
decoder error (`framed_impl.rs:161-168`, `:199-204`). Instead, a
`bounded_lines(reader: impl AsyncBufRead, max_line: usize) -> impl
Stream<Item = io::Result<LineFrame>>`, `LineFrame = Line(Bytes) | Skipped {
bytes: u64 }`, written on `fill_buf`/`consume` over a
`BufReader::with_capacity(K, file)` with ONE accumulator
`Vec::with_capacity(max_line)` allocated when the stream is created and
reused for every line: bytes are copied from the reader's buffer into the
accumulator up to the next `\n`; when the accumulator would exceed
`max_line` the splitter switches to discard mode, consumes bytes without
copying until the newline, and yields `Skipped` (logged at `warn!` and
dropped by the caller); the stream never ends early on an oversize line. A
final line without a newline is yielded as a `Line`. Each `Line` goes
through `line_matches_step`; matches are re-emitted with their `\n` in
`K`-sized output chunks. Unfiltered full mode has no line cap because it
never splits lines. Peak: reader `K` + file buffer `K` + accumulator `L` +
matcher `L` + out chunk `K` = **2 L + 3 K**.

### 3.3 Terminal jobs and the archive

Today a terminal job's read is a line-level union of the local file and the
archive (`merge_jsonl_logs`): each replica's local file holds only the
chunks that reached it (`log_storage.rs:20-30`), the archive holds what the
orchestrating replica had at upload time, and the union is the most complete
view (`test at :1665-1690` requires disjoint lines from both to survive).
Neither source is complete on its own (§ 2 lists what each can miss). The
union is kept in every form that fits the caps.

**Archive access primitives** (`crates/stroem-server/src/blob_storage.rs:24`).
The trait's default `get_stream` (`:50-58`) buffers the whole object through
`get`, and an SDK body stream yields chunks the server does not size; so
this feature does not use `get_stream`. It adds two primitives with
identical semantics on every backend (`S3BlobArchive` `:279`,
`LocalBlobArchive` `:108`, the in-memory and test archives):

- `async fn size(&self, key) -> Result<Option<u64>>` — object length;
  `None` when the object does not exist. S3 via `head_object`, local via
  `metadata`.
- `async fn get_range(&self, key, offset: u64, len: u64) -> Result<Bytes>`
  — the bytes in `[offset, min(offset + len, size))`; EMPTY when `offset
  >= size` (S3 answers 416 for an unsatisfiable range, mapped to empty;
  local seeks and reads). Never more than `len` bytes.

On top of them, `ArchiveRangeReader { key, size, pos }` implements
`AsyncBufRead` by fetching `get_range(pos, R)` sequentially and holding
exactly one range buffer (`<= R`); `GzipDecoder` from `async-compression`
(features `tokio`, `gzip`, version pinned in `Cargo.lock` at implementation
time) decodes it, holding its own state of `D = 64 KiB` (the 32 KiB inflate
window plus tables). An archive object modified mid-read produces a gzip
error, handled like any other archive error at the point it occurs.

`ISIZE`: the last 4 bytes of a gzip member are the uncompressed length mod
2^32. `upload_to_archive` writes ONE member (`GzEncoder::new` … `finish()`,
`log_storage.rs:280-288`), so `get_range(size - 4, 4)` on an object of
`size >= 18` (the smallest valid gzip) gives the decompressed length exactly
for objects under 4 GiB; above that it wraps, which is why every
decompression below runs through `take` and never trusts `ISIZE` alone. An
object shorter than 18 bytes is treated as corrupt (an archive error).

**Tail (both filters), terminal.** Two bounded tails are read, then merged:

- The local tail as in § 3.2.
- The archive tail: the object streamed through `ArchiveRangeReader` →
  `GzipDecoder` → the same `bounded_lines` splitter (so the carry is
  bounded by `L`) into a ring of whole lines capped at `T` bytes with
  OLDEST-first eviction (the ring keeps the newest `T` bytes of lines; for
  a step tail only matching lines enter it), counting decompressed bytes
  for `total_bytes`. Computed COMPLETELY before the response starts, so an
  archive error at any point (download, gzip, footer) degrades to the local
  tail exactly as today (`:358-366`; `test at :1419-1425`).
- If both exist and `lines(local) + lines(archive) <= N`, the two tails
  (each `<= T`) go through the existing `merge_jsonl_logs` unchanged and the
  result is cut from the front at a line boundary to `<= T`. `source:
  merged`. If the line cap is exceeded the local tail is served alone
  (`source: local`) — the union's per-line overhead does not depend on
  line length, and two tails of one-byte lines would otherwise cost `48 × T`
  bytes of set and index.
- `truncated = local.truncated || archive.truncated || merged_was_cut ||
  union_skipped_for_lines`. `total_bytes = local_len + archive_isize` when
  both were consulted (an upper bound; `returned <= total` holds).
- Cost: one archive stream per FETCH on a terminal job. The job page
  issues one fetch for the expanded step (`step-detail.tsx:69`) and one for
  `_server` (`server-events.tsx:21`), each once, since a terminal job is not
  polled — two archive streams per page view, plus one per further step the
  user expands.
- Peak (the merge phase dominates; the scan buffers are released before
  it): local result `T` + archive ring `T` + range buffer `R` + decoder `D`
  + carry `L` + matcher `L` + merge inputs (the same two `T` buffers) + set
  and index `48 × N'` where `N' = min(N, lines)` + merge output `2 T` +
  cut result `T` = **5 T + 2 L + R + D + 48 N'**.

**Full, terminal, under the caps.** Gate: `local_len + isize <= C`. If it
holds, local is read through `take(C)` into a buffer, then the archive is
decompressed through `take(C - local_read + 1)`; if either `take` fills
(the local file grew past its snapshot, the trailer lied or wrapped, the
object changed), the merge is ABANDONED before any output and the
single-source rule below applies. Lines are counted while reading; if
`lines > N` the merge is likewise abandoned. Otherwise `merge_jsonl_logs`
runs unchanged and the result is streamed from that one string in `K`
chunks. `source: merged`. This preserves today's behaviour for every
normal-sized log. Peak: inputs `<= C` + set and index `<= 48 N` + output
`<= C` + range buffer `R` + decoder `D` = **2 C + 48 N + R + D**.

**Full, terminal, above a cap or archive size unknown.** ONE source is
streamed: the LOCAL file when it exists (§ 3.2 full modes), else the
archive through `ArchiveRangeReader` → `GzipDecoder` (→ `bounded_lines`
when filtered) in `K` chunks. Local first because a plain-file read fails
far less often mid-stream than a multi-request download and because it is
the only place the post-upload lines (§ 2) can be; either source can still
error mid-stream (`:393-409` propagates a mid-read I/O error), and when
that happens the body simply ends early — once headers are sent there is
no fallback and the header cannot be rewritten. An archive error BEFORE the
first byte (missing object, `size` failure) falls back to local when
present, else answers an empty 200 with `source: none`. `source: local` or
`archive`; the docs state that neither is guaranteed complete. Peak: local
as in § 3.2; archive **R + D + K** unfiltered, **R + D + 2 L + 2 K**
filtered.

Non-terminal jobs never touch the archive (unchanged; the upload happens at
terminal time, so a pre-terminal archive read is a guaranteed miss).

### 3.4 Memory bounds

Per request, counting every buffer alive at the peak. Symbols and their
enforcement: `T` (`take` + preallocated buffers, `<= tail_max_bytes`); `L`
(accumulator preallocated once, carry capped); `K = 64 KiB` (stream
capacities); `R = 1 MiB` (range read size); `D = 64 KiB` (decoder state,
from the inflate window and tables); `C` (`take`); `N` (line counter); the
envelope `E` (§ 3.1) is the serialised JSON of a `T`-byte string, at most
`6 T` because serde_json's worst-case escape is `\u00XX` for one byte; and
`48 N'` is the merge's `HashSet<&str>` entry (24 B) plus `Vec<&str>` entry
(16 B) plus hasher slack per line, `N' = min(N, lines in the inputs)`.

| Read | Peak | Default (`T` 256 KiB) | Max (`T` 4 MiB, `N'` = N) |
|---|---|---|---|
| Tail, unfiltered, local | `2 T + E` = `8 T` | 2 MiB | 32 MiB |
| Tail, step, local | `3 T + 2 L + E` = `9 T + 2 L` | 4.25 MiB | 38 MiB |
| Tail, terminal, merged | `5 T + 2 L + R + D + 48 N' + E` = `11 T + 2 L + R + D + 48 N'` | 5.8 MiB + `48 N'` (≤ 12 MiB adversarial, ~100 KiB typical) | 47 MiB + 12 MiB |
| Full, unfiltered, local | `2 K` | 128 KiB | 128 KiB |
| Full, filtered, local | `2 L + 3 K` | 2.2 MiB | 2.2 MiB |
| Full, archive, single source | `R + D + K` (`+ 2 L + K` filtered) | 1.1 MiB (3.2 MiB) | same |
| Full, terminal, merged | `2 C + 48 N + R + D` | 45 MiB | 45 MiB |

With defaults, a UI poll costs at most 4.25 MiB and the most expensive read
any client can trigger costs about 59 MiB (a 4 MiB terminal tail over
adversarial one-byte lines), against ~100 MiB of headroom in production
today and against the 512 Mi limit the helmfile bump raises to 2 Gi. Two
concurrent worst-case reads do not fit today's headroom, which is one more
reason every cap is configurable and the bump is due. The "typical" figures
are not bounds and are not relied on.

### 3.5 Configuration

Under `log_storage:` (`crates/stroem-server/src/config.rs:51`), all
optional with defaults; `ServerConfig::validate` rejects any zero,
`tail_default_bytes > tail_max_bytes`, `tail_max_bytes >
tail_scan_max_bytes`, and `max_line_bytes > merge_max_bytes`:

| Key | Default | Meaning |
|---|---|---|
| `tail_default_bytes` | 262144 (256 KiB) | tail size when the request names none |
| `tail_max_bytes` | 4194304 (4 MiB) | largest `tail_bytes` a request may ask for |
| `tail_scan_max_bytes` | 67108864 (64 MiB) | how far back a step tail scans before giving up |
| `max_line_bytes` | 1048576 (1 MiB) | longest single line a filtered read will carry; longer lines are skipped with a warning |
| `merge_max_bytes` | 16777216 (16 MiB) | sum of local length + archive decompressed length under which a terminal full read still merges in memory |
| `merge_max_lines` | 262144 | most lines a union merge (tail or full) will hold; above it a single source is served |

Env overrides follow the existing convention
(`STROEM__LOG_STORAGE__TAIL_DEFAULT_BYTES`).

### 3.6 UI

**Viewer.** `ui/src/components/log-viewer.tsx` keeps its props, the dark
monospace look, `role="log"` / `aria-live`, the "Waiting for logs…" empty
state and its scroll contract, but the `lines.map` at `:86` (parsing per
row at `:87`, splitting memoised at `:59-62`) becomes a
`@tanstack/react-virtual` list (new dependency, `^3.14`). Lines are parsed
once per body change into an array of `{raw, ts, stream, step, line}` or a
legacy plain-text marker, instead of being JSON-parsed on every render.
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
top of the viewer reads "Showing the last ~2,100 lines (256 KiB of up to
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
  `appendTail(displayed: Line[], tail: Line[]): {lines, gap}` that applies
  the SAME rule the server's `merge_jsonl_logs` applies — a union by exact
  line — and nothing about time, because a file window is not a time range
  (§ 1):
  1. `tailSet = Set(tail.map(l => l.raw))`.
  2. `kept = displayed.filter(l => !tailSet.has(l.raw))`.
  3. `lines = kept.concat(tail)`.
  4. `gap = displayed.length > 0 && tail.length > 0 && kept.length ===
     displayed.length` — no tail line had been displayed, so nothing ties
     the tail to the view; the viewer inserts a visible "gap" divider row.
  Properties: idempotent (applying the same tail again removes and re-adds
  exactly the tail); no duplication under arrival-ordered tails (the
  revision-2 counterexample `[B, A, C]` + same tail yields `[B, A, C]`); a
  line another replica recovered in the middle is shown (displayed `[A, C]`,
  tail `[A, B, C]` → `[A, B, C]`); a stale mirrored straggler in the tail is
  appended once, not used to cut history. Byte-identical lines collapse to
  one occurrence, which is exactly what the server merge does today
  (`HashSet<&str>` on the whole line, `log_storage.rs:36-45`). Ordering is
  "everything not in the tail, then the tail", which for a running step
  means older content then the newest window; a straggler can sit out of
  time order, as it does in the file. Cost per poll is `O(|displayed| +
  |tail|)` string-set lookups (~475 k for the incident log), tens of
  milliseconds; if that ever matters the filter can be limited to a suffix
  window, not done now. `appendTail` lives in `ui/src/lib/log-tail.ts` with
  Vitest coverage (§ 5).

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
   4 MiB is the most a single tail request may ask for (its full request
   cost is in § 3.4).
3. **Tail by default for every caller,** with `truncated`/`total_bytes` in
   every envelope, rather than tail only where the UI asks. The other
   callers are exactly the ones that would reproduce the outage, and an
   MCP agent is better served by a tail anyway.
4. **No offsets, no paging, no time windows.** Offsets are replica-local
   and files are arrival-ordered (§ 1). Every request is stateless: tail or
   full.
5. **Streamed NDJSON for full, not a streamed JSON string.** Escaping a JSON
   string while streaming is possible but pointless; `full=true` is an
   opt-in, so it may have its own content type.
6. **Terminal tails union both sources** (revision 2). A local-only tail
   could report `truncated: false` on a partial replica and hide the banner
   over missing lines; two bounded tails through the existing merge keep
   the HA recovery the current tests pin, at one archive stream per fetch.
7. **Exact merge gate from the gzip trailer, guarded by `take` on BOTH
   inputs** (revisions 2–3). A compression-ratio estimate is not a bound
   (repetitive JSONL compresses 300 ×); `ISIZE` is exact under 4 GiB for the
   single-member objects we write, and `take` makes a wrong, wrapped or
   stale trailer — and a local file that grew — safe.
8. **Above the caps, local first, archive last** (revision 2). A streaming
   response cannot fall back after its headers; the local file is the source
   least likely to fail mid-stream and the only one holding post-upload
   lines. Neither source is complete; the header names which answered.
9. **Keep the in-memory merge under the caps.** Removing it would silently
   drop the HA-recovery behaviour for the common case where it costs a
   bounded, stated amount.
10. **A line cap beside the byte cap** (revision 3). The merge's set and
    index cost ~48 bytes per line regardless of length; a byte cap alone
    lets one-byte lines multiply it by 40.
11. **Our own splitter on `fill_buf`/`consume`, not `FramedRead`**
    (revisions 2–3). `FramedRead` ends the stream after any decoder error
    and grows its buffer by doubling; one preallocated accumulator gives a
    bound that is a constant, not a growth policy.
12. **Range reads, not the SDK stream, for the archive** (revision 3). The
    server sizes every archive read to `R`; the SDK's chunking is not ours
    to bound.
13. **A borrowed `step`-only deserializer** (revision 3). The step matcher
    is on the hot path of every filtered read; a `Value` per line is an
    allocation multiplier no table can bound.
14. **Virtualised viewer that can load the whole log** (the user's choice
    over "page backwards" and "download only"), TanStack Virtual, no Pretext.
15. **`appendTail` is a union by exact line, like the server's merge**
    (revision 3). Timestamp cuts duplicated under arrival-ordered tails; an
    overlap search dropped recovered lines; exact-line union is idempotent,
    duplication-free, and already the system's definition of "the same
    line".
16. **Remove the String-returning read functions** so the unbounded path
    cannot be reintroduced by a future caller; every existing call site,
    including the nine in integration tests, moves to the bounded API.

## 5. Testing

Unit (`log_storage.rs`):
- tail: cut at a line boundary; larger than the file → whole file,
  `truncated: false`; empty file; a single line longer than the window →
  empty + truncated; **a file that grows after the snapshot returns exactly
  the snapshot range** (append from a second task between `metadata` and
  the read; assert `returned_bytes <= total_bytes` and no torn line); **a
  torn trailing line at the snapshot is dropped** and reappears whole on
  the next read.
- step tail: matching line straddling a window edge; quiet step behind a
  chatty one found beyond the first window; **the match that would cross
  `T` is not taken and sets `truncated`** (including when the scan then
  reaches the file start); scan cap reached → truncated; scan reaching the
  start with nothing excluded → not truncated; `build` vs `build-docs`;
  carried line over `L` dropped and flagged.
- resolution: missing `.jsonl` falls to `.log`; both missing, non-terminal
  → empty, `source: None`, no error (retention TOCTOU); both missing,
  terminal → archive.
- matcher: same results as the `Value` implementation on the existing
  fixtures (substring false positive, non-string `step`, escaped step
  names, non-JSON lines).
- terminal tail: local + archive → merged content equals the union of the
  two tails, cut to `T`, `source: merged`, `total_bytes = local_len +
  isize`; archive error → local tail, `source: local`; local missing →
  archive ring buffer with correct `total_bytes`; step tail from the
  archive; **line cap exceeded → local only, `truncated`, `source: local`**.
- `bounded_lines`: oversize line yields `Skipped` and the FOLLOWING line is
  delivered; the stream never ends early; **the accumulator's capacity is
  `max_line` before and after an oversize line** (assert on
  `Vec::capacity`); lines split across `fill_buf` boundaries; final line
  without newline; empty input.
- `ArchiveRangeReader`: object shorter than `R`; object an exact multiple
  of `R`; `get_range` past the end returns empty; `size` of a missing key
  is `None`.
- merge gate: `ISIZE` read from a real gzip member; under both caps →
  merged; over the byte cap → single source; **over the line cap → single
  source**; **a trailer claiming a small size for a large body fills `take`
  and falls to single source before any output**; **a local file that grew
  past the gate fills its `take` likewise**; object under 18 bytes → archive
  error → local.
- `ReaderStream`/splitter pipelines deliver more than one chunk for a body
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
- Terminal job under the caps → `merged` and the union content; above the
  byte cap with local present → `local`; above the cap, local deleted →
  `archive`; archive missing and local missing → empty, `none`.
- The nine migrated call sites in `ha_test.rs`, `s3_integration_test.rs`
  and `integration_test.rs` keep their assertions; new S3 tests: `size` and
  `get_range` (including an unsatisfiable range → empty) on a real bucket,
  a real gzipped object streamed into a tail through range reads.

Memory regression (the test this incident needs):
- `#[ignore]`d, Linux-only integration test that writes a 100 MiB synthetic
  JSONL log, reads `/proc/self/statm` before and after a `full=true` read,
  a step tail, and a terminal merged tail against a 100 MiB archive object,
  and asserts RSS grew by less than the § 3.4 bound for that mode plus
  8 MiB of allocator slack. A second variant uses one-byte lines to
  exercise the line cap. Run on demand and in the release checklist.

CLI: `--full` handles NDJSON and the JSON envelope; the truncation notice
goes to stderr; `--tail-bytes` is forwarded.

UI (Vitest): `appendTail` — plain overlap; **the same tail applied twice is
a no-op**; **the arrival-ordered counterexample `[B, A, C]` does not
duplicate**; recovered middle line shown; straggler appended once; gap only
when no tail line was displayed; identical lines collapse like the server;
empty inputs. Banner rendered when `truncated`, absent otherwise;
confirmation above 64 MiB; `step-detail.test.tsx` updated for the envelope;
a `log-viewer` test that a 10 000-line body renders fewer than 200 rows
(virtualisation is in effect; jsdom needs the size mock the TanStack docs
describe). Playwright (`ui/e2e/log-streaming.spec.ts`): banner appears for
a log above the default tail and "Load full log" renders the first line.

## 6. Documentation

- `docs/src/content/docs/reference/api.md`: query parameters, the three new
  fields (with `total_bytes` as an upper bound), NDJSON mode, the source
  header, WS backfill note.
- `docs/src/content/docs/operations/log-storage.md`: the six keys, the new
  read order and source rules, the header; the `curl … | jq -r .logs` recipe
  stays for the tail, and a SEPARATE `curl '…?full=true'` recipe with no
  `jq` is added for the raw NDJSON stream (the envelope is gone in that
  mode, so `.logs` would be `null`).
- CLI help text; `docs/src/content/docs/guides/mcp.md` for `tail_bytes`.
- `CLAUDE.md` § Log Storage / § WebSocket Log Streaming: read modes and
  bounds; correct two stale facts found on the way — the NOTIFY segment cap
  is 3 500 bytes (`events.rs:43-57`), not 7 000, and
  `ui/src/hooks/use-job-logs.ts` no longer exists.
- `CONTEXT.md`: **Tail read**, **Full read**, **Log source**.
- `docs/internal/TODO.md`: mark the incident entry done; add the WS
  backfill-to-live gap and the post-upload lines as open items next to the
  existing non-goals.
- Release notes: behaviour change for scripts, old CLIs and WS consumers.

## 7. Risks and open points

- **Browser memory in full mode.** An 83 MiB log is ~83 MiB of string plus
  the parsed array; the 64 MiB confirmation is the only guard. Accepted by
  the user's choice of a virtualised full view; download is one click away.
- **Step tails on a huge, quiet-step log** cost a sequential read of up to
  64 MiB every 2 s per open viewer (page cache, `contains` fast path). CPU,
  not memory; the cap bounds it. If it proves expensive, lower
  `tail_scan_max_bytes` — the trade is a `_server` card that may go blank on
  a very large job until "Load full log".
- **`truncated` is conservative for step tails** (§ 3.2). The UI wording
  and the docs say "may have earlier lines".
- **Every terminal tail streams the whole archive object** (no random
  access into gzip) — bounded memory, one S3 download per fetch on a
  finished job, two per page view. If it proves costly, the archive tail can
  be skipped when the local file's length equals or exceeds `ISIZE`; not
  done now because that heuristic can hide a partial replica whose file
  happens to be large.
- **Above the caps the full read is one source and incomplete by
  construction.** The header says which; the docs say why. A streaming
  union would need sorted inputs, which arrival-ordered files are not.
- **`total_bytes` is an upper bound of the job log, not the step's share.**
  The banner says "of up to".
- **Adversarial one-byte lines** push a 4 MiB terminal tail to ~59 MiB.
  Bounded and stated; lower `tail_max_bytes` or `merge_max_lines` if that
  headroom is not available. The deploy-side limit bump is the real fix for
  headroom and is the user's.
