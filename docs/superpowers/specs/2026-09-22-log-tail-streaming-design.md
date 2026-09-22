# Log reads: tail by default, streamed full log

Status: revision 2, proposed (2026-09-22)

Companion of `docs/internal/TODO.md` § Performance ("Opening the job page of a
job with a large log OOM-kills every server replica") and of the HA log
mirroring record in TODO.md § "Review: HA Log Mirroring (2026-05-21)".

## Revision history

**Revision 2 (2026-09-22).** After the first Codex review (19 findings, all
verified against the code). Decisions changed with the user: a terminal
job's TAIL is the union of a bounded local tail and a bounded archive tail
(finding 1); the merge gate reads the gzip trailer's exact decompressed size
and guards the decoder with `take`, instead of guessing a compression ratio
(finding 4); above the cap the full read streams the LOCAL file first and
the archive only when local is absent, because a stream cannot fall back
after its headers are sent (findings 2, 3); `appendTail` replaces the view
from the tail's first timestamp instead of searching for an overlap
(finding 16). Corrections without a decision change: `take(N)` after the
seek so a growing file cannot overrun the window (6); the step tail's result
is strictly `<= tail_bytes` (7); a bounded line splitter that never yields a
codec error, because `FramedRead` ends the stream after one (15); honest
allocation accounting for every mode, including the serialised envelope,
tokio's file buffer and per-line parsing (5, 7–10); the nine integration-test
call sites of the removed functions are migrated, not "unchanged" (17); the
WebSocket backfill-to-live gap is recorded as pre-existing and out of scope
(18); citations and the curl recipe fixed (11–14, 19).

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
  (`crates/stroem-server/src/events.rs:415-449`; oversize segments are
  signal-only and stay on the publishing replica, `ha_test.rs:527-537`), so
  every replica holds nearly the whole log, not a share of it.
- `ui/src/components/step-detail.tsx:85-92` polls
  `GET /api/jobs/{id}/steps/{step}/logs` every 2 s while the step runs.
- `LogStorage::get_step_log` (`crates/stroem-server/src/log_storage.rs:421`)
  → `filter_step_from_file_if_present` (`:475`) reads the file line by line
  but accumulates every matching line into ONE `String`. The handler
  (`crates/stroem-server/src/web/api/jobs.rs:561`) wraps that string in
  `json!({"logs": ..})` (a second copy) and axum serialises it (a third):
  roughly 200 MB per poll against ~100 MiB of headroom.
- `get_job_logs` (`jobs.rs:597`), the WebSocket backfill
  (`crates/stroem-server/src/web/api/ws.rs:164`, the whole log as one text
  frame), the MCP tool `get_job_logs`
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
- **Byte offsets are not stable across replicas.** Each replica's local file
  interleaves direct worker pushes and mirrored peer chunks in its own arrival
  order (`events.rs:424-431`), so an offset obtained from one replica means
  nothing on the next request behind the load balancer. Any design that pages
  or delta-polls by offset is wrong in HA.

## 2. Goals and non-goals

Goals:

1. No log read, from any client, can allocate more than a stated, fixed
   amount of memory on the server, whatever the log size. The bound is
   stated per read mode in § 3.4 and includes every buffer on the path.
2. The UI keeps a live, auto-following view of a running step, and can still
   show the whole log of a long run on demand.
3. Every existing invariant of the read path survives: `.jsonl` → legacy
   `.log` → (terminal) archive fallback with `NotFound` never a 500
   (retention TOCTOU), archive errors degrade to local whenever a fallback
   is still possible, archive only for terminal jobs, union recovery of
   mirrored-away lines for terminal jobs, exact-match step filter, `_server`
   as a pseudo-step, "keep last non-empty body" in the UI.
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
- The WebSocket backfill-to-live gap: `ws.rs` sends the backfill (`:164`)
  BEFORE subscribing to the broadcast (`:177`), so chunks pushed in between
  never reach that connection. Pre-existing; a tail backfill neither widens
  nor closes it. Recorded as an open TODO item.
- Lines appended to a job AFTER its archive upload (e.g. failed-hook events
  written to the original job by `settlement/terminal.rs:215-230` while the
  upload is spawned at `:250`). They exist only in the appending replica's
  local file, today and after this change.
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
- `truncated` is `true` when a consulted source was NOT read completely —
  there MAY be earlier lines. For a step tail it can be `true` even when no
  earlier line of that step exists (§ 3.2); `false` is exact.
- `total_bytes` is the size of the largest source consulted: the local file
  length snapshot, or, for a terminal job, the larger of that and the
  archive's decompressed size. It is the size of the JOB log, not of the
  step's share.
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
unit tests and the nine integration-test call sites (`s3_integration_test.rs`
`:143`, `:188`, `:256`, `:259`; `ha_test.rs` `:318`, `:383`, `:534`;
`integration_test.rs` `:29180`, `:29498`) migrate to the new entry points
with their assertions unchanged.

```rust
pub enum StepFilter<'a> { All, Step(&'a str) }

pub struct Tail {
    pub logs: String,        // whole lines, len() <= tail_bytes
    pub truncated: bool,
    pub total_bytes: u64,
    pub source: LogSource,   // Local | Archive | Merged | None
}

pub async fn read_tail(&self, job_id, meta, is_terminal, filter, tail_bytes) -> Result<Tail>;
pub async fn stream_full(&self, job_id, meta, is_terminal, filter)
    -> Result<(LogSource, BoxStream<'static, io::Result<Bytes>>)>;
```

**Local file resolution** (both entry points): `.jsonl` first, then legacy
`.log`, exactly the order of today's `read_local_log` (`log_storage.rs:386`).
`NotFound` on both is `Ok(None)` — retention can delete the file mid-request
(`:380-385` keeps its comment and its behaviour) — and the caller falls
through to the archive when the job is terminal (§ 3.3), else to an empty
result with `source: None`. Every other I/O error propagates as today.

**Unfiltered tail (`StepFilter::All`), local.** `len = metadata().len()`
(the snapshot; `total_bytes`) → `seek(max(0, len - tail_bytes))` →
`take(tail_bytes).read_to_end(&mut buf)` → if the seek point was `> 0`, drop
everything up to and including the first `\n`. `take` is what enforces the
window: `append_log` (`:192-199`) writes and flushes under its own handle
lock that readers do not share, so a file can grow between the `metadata`
call and the read; bytes past the snapshot are simply left for the next
poll. `truncated = seek_point > 0`.

**Step tail (`StepFilter::Step`), local.** A quiet step next to a chatty one
would return nothing from the last 256 KiB, so the step tail scans BACKWARDS
in windows of `tail_bytes` (each window read with `seek` + `take`, so the
snapshot `len` bounds the whole scan): split the window at newlines, carry
the partial first line into the next window, test each whole line with the
existing `line_matches_step` (`:508` — the `contains` fast guard, then a
`serde_json` parse and exact `step` equality, so `build` never matches
`build-docs`), collect matching lines newest-first, and stop when the next
match would push the collected bytes OVER `tail_bytes` (that match is not
taken, so `logs.len() <= tail_bytes` holds strictly), or the scan reaches the
start of the file, or the scan has covered `log_storage.tail_scan_max_bytes`
(64 MiB). Output is reversed back into file order. `truncated` is `true`
unless the scan reached the start of the file — so a truncated step tail may
in fact be complete; the UI wording ("showing the last …") is chosen for
that. A carried partial line longer than `max_line_bytes` (1 MiB) is dropped
with a `warn!`. The scan cap keeps a 3 s `_server` poll on a gigabyte log
from re-reading the whole file every tick; 64 MiB covers this incident's log
in one scan.

**Full, unfiltered, local.** `tokio::fs::File` →
`ReaderStream::with_capacity(_, 64 KiB)` → `axum::body::Body::from_stream`
(polls one item per frame, `axum-core` `body.rs:209-218`). Tokio's file
reader owns a further buffer of up to `DEFAULT_MAX_BUF_SIZE` (2 MiB,
`tokio/src/io/blocking.rs:75`) sized to the read request, so the pipeline
holds at most one 64 KiB chunk in flight plus tokio's copy of it; it never
materialises the log.

**Full, filtered, local.** `FramedRead` with our own
`BoundedLineSplitter: Decoder<Item = LineFrame>` where
`LineFrame = Line(Bytes) | Skipped { bytes: usize }`. It is written so that
`decode` NEVER returns `Err`: a line that exceeds `max_line_bytes` is
discarded byte-range by byte-range as it arrives and reported as `Skipped`
once its newline is seen. This matters because `FramedRead` sets
`has_errored` on any decoder error and returns `None` on the next poll
(`tokio-util-0.7.18/src/codec/framed_impl.rs:161-168`, `:199-204`), which
would silently END the stream at the first oversize line — `LinesCodec`'s
own "discard until newline" recovery never gets to run under `FramedRead`.
Each `Line` goes through `line_matches_step`; matches are re-emitted with
their `\n`; `Skipped` frames are logged at `warn!` and dropped. `FramedRead`
grows its buffer in 8 KiB reserves independent of any line limit
(`framed_impl.rs:25-51`, `:215-220`); with the splitter consuming or
discarding every complete prefix on each `decode`, the buffer holds at most
one incomplete line, so `<= max_line_bytes + 8 KiB`. Unfiltered full mode
has no line cap because it never splits lines. `tokio-util` gains the
`codec` and `io` features (workspace `Cargo.toml`).

### 3.3 Terminal jobs and the archive

Today a terminal job's read is a line-level union of the local file and the
archive (`merge_jsonl_logs`): each replica's local file holds only the
chunks that reached it (`log_storage.rs:20-30`), the archive holds what the
orchestrating replica had at upload time, and the union is the most complete
view (`test at :1665-1690` requires disjoint lines from both to survive).
Neither source is complete on its own (§ 2 non-goals list what each can
miss). The union is kept in every bounded form it can take:

- **Tail (both filters).** Two bounded tails are read: the local tail as in
  § 3.2, and an archive tail — the object streamed through a gunzip decoder
  into a ring buffer of `tail_bytes` (for a step tail the ring buffer holds
  matching lines only, newest-first eviction), counting the decompressed
  length for `total_bytes`, cut at the first newline. The two tails, each
  `<= tail_bytes`, go through the existing `merge_jsonl_logs` unchanged;
  the merged result is then cut again from the front at a line boundary to
  `<= tail_bytes`. `truncated = local.truncated || archive.truncated ||
  merged_was_cut`. `source: merged` when both existed, else `local` or
  `archive`. The archive tail is computed COMPLETELY before the response
  starts, so an archive error (download, gzip corruption, footer) degrades
  to the local tail exactly as today (`:358-366`; `test at :1419-1425`).
  Cost: one streamed archive download per tail request on a terminal job,
  which the UI does not poll (the step is not running) — one per page view.
- **Full, under the merge cap.** The gate uses the archive's EXACT
  decompressed size: the last 4 bytes of a gzip member are `ISIZE`, the
  uncompressed length mod 2^32. `upload_to_archive` writes one member
  (`GzEncoder::new` … `finish()`, `log_storage.rs:280-288`), so a
  `get_suffix(key, 4)` range read gives the size without a download. If
  `local_len + isize <= log_storage.merge_max_bytes` (16 MiB): local is read
  whole, the archive is decompressed through `take(merge_max_bytes + 1)`
  (so a wrong or wrapped trailer cannot overrun — an overrun aborts the
  merge and falls to the single-source path below, BEFORE any response
  byte), `merge_jsonl_logs` runs unchanged, and the result is streamed from
  that one string. `source: merged`. This preserves today's behaviour for
  every normal-sized log.
- **Full, above the cap, or archive size unknown.** ONE source is streamed:
  the LOCAL file when it exists, else the archive. Local first because a
  plain file cannot fail mid-stream and because it is the only place the
  post-upload lines (§ 2) can be; the archive is the last resort because
  once headers are sent a mid-stream download or gzip error can only end the
  body early — there is no source to fall back to and the header cannot be
  rewritten. An archive error BEFORE the first byte (missing object, HEAD
  failure) falls back to local when present, else answers an empty 200 with
  `source: none`. `source: local` or `archive`, and the docs state that
  neither is guaranteed complete.
- Non-terminal jobs never touch the archive (unchanged; the upload happens
  at terminal time, so a pre-terminal archive read is a guaranteed miss).

**`BlobArchive` additions** (`crates/stroem-server/src/blob_storage.rs:24`):

- `async fn get_suffix(&self, key, n: u64) -> Result<Option<Bytes>>` — the
  last `n` bytes of an object. `S3BlobArchive` (`:279`) via
  `get_object().range("bytes=-n")`, `LocalBlobArchive` (`:108`) via
  `seek(End(-n))`, the in-memory/test archives via slicing. Used for the
  ISIZE gate.
- Real streaming `get_stream` overrides: the trait's default (`:50-58`)
  buffers the whole object through `get`, which would defeat both the
  archive tail and the archive full stream. `S3BlobArchive` returns the SDK
  `ByteStream` as a `BoxStream` (the SDK yields chunks of up to a few
  hundred KiB; the bound in § 3.4 counts one); `LocalBlobArchive` a
  `ReaderStream::with_capacity(_, 64 KiB)` over the file. `put_stream` is
  out of scope.

The gunzip decoder is `async_compression::tokio::bufread::GzipDecoder`
(new dependency `async-compression`, features `tokio`, `gzip`, pinned to the
version resolved at implementation time in `Cargo.lock`), wrapped over
`StreamReader` of the archive stream, which retains one upstream chunk
(`tokio-util/src/io/stream_reader.rs:274-285`).

### 3.4 Memory bounds

All figures are per request and count every simultaneous buffer on the
path, with `T = tail_bytes`, `L = max_line_bytes` (1 MiB), `C =
merge_max_bytes` (16 MiB). "Line parse" is the `serde_json::Value`
`line_matches_step` builds for one line, at most ~3 × the line's bytes.

| Read | Bound |
|---|---|
| Tail, unfiltered, local | `T` (buffer) + `T` (serialised envelope, escaped JSON ≈ 1.3 ×) ≈ **2.3 T** |
| Tail, step, local | window `T` + result `T` + carried line `L` + one line parse `3 L` + envelope `1.3 T` ≈ **3.3 T + 4 L** |
| Tail, terminal (merged) | the local tail above + archive ring `T` + upstream chunk (≤ 1 MiB) + decoder state (≈ 64 KiB) + merge of two `T` inputs (inputs `2 T` + set/vec ≈ 48 B per line + output `2 T`) ≈ **8 T + 4 L + 1 MiB** |
| Full, unfiltered, local | one 64 KiB chunk + tokio's copy (≤ 2 MiB) ≈ **2.1 MiB** |
| Full, filtered, local | `FramedRead` buffer `L + 8 KiB` + decoded line `L` + line parse `3 L` + one 64 KiB out chunk ≈ **5 L** |
| Full, archive stream (above cap, local absent) | upstream chunk (≤ 1 MiB) + decoder state + 64 KiB chunk (+ `5 L` when filtered) ≈ **1.1 MiB (+ 5 L)** |
| Full, merged (under cap) | inputs `≤ C` + set/vec (48 B × lines, ≤ 48 B × C / 40 B ≈ 1.2 C) + output `≤ C` + upstream chunk ≈ **3.2 C + 1 MiB** |

With defaults (`T` = 256 KiB, `L` = 1 MiB, `C` = 16 MiB): a UI poll costs
under 1 MiB; the largest tail a client may request (`T` = 4 MiB, terminal,
merged) costs about 37 MiB; the capped merge costs about 52 MiB. Against
~100 MiB of headroom in production today, and against the 512 Mi limit the
helmfile bump raises to 2 Gi, both fit; two concurrent worst-case merges do
not fit today's headroom, which is one more reason the merge cap is
configurable and the bump is due. Every other mode is bounded by
single-digit MiB.

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

Env overrides follow the existing convention
(`STROEM__LOG_STORAGE__TAIL_DEFAULT_BYTES`).

### 3.6 UI

**Viewer.** `ui/src/components/log-viewer.tsx` keeps its props, the dark
monospace look, `role="log"` / `aria-live`, the "Waiting for logs…" empty
state and its scroll contract, but the `lines.map` at `:86` (parsing per
row at `:87`, splitting memoised at `:59-62`) becomes a
`@tanstack/react-virtual` list (new dependency, `^3.14`). Lines are parsed
once per body change into an array of `{ts, stream, step, line}` or a legacy
plain-text marker, instead of being JSON-parsed on every render. Soft
wrapping stays; the row-height estimate is
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
  `appendTail(displayed: Line[], tail: Line[]): {lines, gap}` whose rule is
  "the tail is authoritative for the time it covers", the same rule
  whole-body replacement gives tail mode today:
  1. Let `t0` be the `ts` of the first tail line. Keep every displayed line
     with `ts < t0`, and every displayed line with `ts == t0` whose raw text
     is not present in the tail (a tail window cut mid-millisecond must not
     drop the earlier lines of that millisecond). Drop the rest.
  2. Append the whole tail.
  3. `gap = true` when `t0` is later than the last displayed `ts`, i.e. the
     step outran a 256 KiB tail within one poll and no time overlaps; the
     viewer inserts a visible "gap" divider row so nobody mistakes the view
     for complete.
  A displayed line without a `ts` (legacy plain text) is always kept; such
  lines belong to pre-JSONL jobs, which are never running, so they cannot
  meet full-mode stitching in practice. Because the tail replaces the
  covered window wholesale, a line another replica recovered in the middle
  (displayed `[A(ts1), C(ts3)]`, tail `[A(ts1), B(ts2), C(ts3)]`) is shown,
  exactly as today's replacement would show it; no overlap search is needed.
  `appendTail` lives in `ui/src/lib/log-tail.ts` with Vitest coverage
  (§ 5).

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
4. **No offsets, no paging.** Offsets are replica-local (§ 1). Every request
   is stateless: tail or full.
5. **Streamed NDJSON for full, not a streamed JSON string.** Escaping a JSON
   string while streaming is possible but pointless; `full=true` is an
   opt-in, so it may have its own content type.
6. **Terminal tails union both sources** (revision 2). A local-only tail
   could report `truncated: false` on a partial replica and hide the banner
   over missing lines; two bounded tails through the existing merge keep
   the HA recovery the current tests pin, at one archive stream per view.
7. **Exact merge gate from the gzip trailer, guarded by `take`** (revision
   2). A compression-ratio estimate is not a bound (repetitive JSONL
   compresses 300 ×); `ISIZE` is exact for the single-member objects we
   write, and `take` makes even a wrong trailer safe.
8. **Above the cap, local first, archive last** (revision 2). A streaming
   response cannot fall back after its headers; the local file is the source
   that cannot fail mid-stream and the only one holding post-upload lines.
   Neither source is complete; the header names which answered.
9. **Keep the in-memory merge under the cap.** Removing it would silently
   drop the HA-recovery behaviour for the common case where it costs a
   bounded, stated amount.
10. **Our own bounded line splitter, not `LinesCodec`** (revision 2).
    `FramedRead` ends the stream after any decoder error, so a codec whose
    oversize handling is an error cannot skip a line and continue.
11. **Virtualised viewer that can load the whole log** (the user's choice
    over "page backwards" and "download only"), TanStack Virtual, no Pretext.
12. **`appendTail` replaces from the tail's first timestamp** (revision 2).
    An overlap search discarded lines a replica recovered in the middle of
    the window; wholesale replacement of the covered time is what tail mode
    already does and needs no heuristics.
13. **Remove the String-returning read functions** so the unbounded path
    cannot be reintroduced by a future caller; every existing call site,
    including the nine in integration tests, moves to the bounded API.

## 5. Testing

Unit (`log_storage.rs`):
- tail cut at a line boundary; tail larger than the file returns the whole
  file with `truncated: false`; empty file; a single line longer than the
  window → empty + truncated; **a file that grows between `metadata` and
  the read returns at most `tail_bytes` and reports the snapshot length**
  (append from a second task after the snapshot is taken).
- step tail: matching line straddling a window edge; quiet step behind a
  chatty one found beyond the first window; the match that would cross
  `tail_bytes` is not taken (`logs.len() <= tail_bytes`); scan cap reached →
  truncated; scan reaching the file start → not truncated; `build` vs
  `build-docs`; carried line over `max_line_bytes` dropped.
- missing `.jsonl` falls to `.log`; both missing, non-terminal → empty,
  `source: None`, no error (retention TOCTOU); both missing, terminal →
  archive.
- terminal tail: local + archive → merged content equals the union of the
  two tails, `source: merged`; archive error → local tail, `source: local`;
  local missing → archive ring buffer with correct `total_bytes`; step tail
  from the archive.
- `BoundedLineSplitter`: oversize line yields `Skipped` and the FOLLOWING
  line is delivered; the stream never ends early on an oversize line;
  buffer stays `<= max_line_bytes + 8 KiB` while an oversize line streams
  in (assert on `BytesMut::capacity`); split across chunk boundaries;
  final line without newline.
- merge gate: `ISIZE` read from a real gzip member; under cap → merged;
  over cap → single source; **a trailer claiming a small size for a large
  body trips `take` and falls to single source before any output**;
  `choose_full_source(local, isize, cap)` as a pure function.
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
- Terminal job under the cap → `merged` and the union content; above the
  cap with local present → `local`; above the cap, local deleted →
  `archive`; archive missing and local missing → empty, `none`.
- The nine migrated call sites in `ha_test.rs`, `s3_integration_test.rs`
  and `integration_test.rs` keep their assertions; one new S3 test streams a
  real gzipped object into a tail and one reads `get_suffix` for `ISIZE`.

Memory regression (the test this incident needs):
- `#[ignore]`d, Linux-only integration test that writes a 100 MiB synthetic
  JSONL log, reads `/proc/self/statm` before and after a `full=true` read,
  a step tail and a terminal merged tail against a 100 MiB archive object,
  and asserts RSS grew by less than 16 MiB in each. Run on demand and in the
  release checklist.

CLI: `--full` handles NDJSON and the JSON envelope; the truncation notice
goes to stderr; `--tail-bytes` is forwarded.

UI (Vitest): `appendTail` — plain overlap, recovered middle line shown,
boundary-millisecond lines kept, gap flagged only when no time overlaps,
ts-less lines kept, empty inputs; banner rendered when `truncated`, absent
otherwise; confirmation above 64 MiB; `step-detail.test.tsx` updated for the
envelope; a `log-viewer` test that a 10 000-line body renders fewer than 200
rows (virtualisation is in effect; jsdom needs the size mock the TanStack
docs describe). Playwright (`ui/e2e/log-streaming.spec.ts`): banner appears
for a log above the default tail and "Load full log" renders the first
line.

## 6. Documentation

- `docs/src/content/docs/reference/api.md`: query parameters, the three new
  fields, NDJSON mode, the source header, WS backfill note.
- `docs/src/content/docs/operations/log-storage.md`: the five keys, the new
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
  access into gzip) — bounded memory, one S3 download per page view of a
  finished job. If it proves costly, the archive tail can be skipped when
  the local file is at least as large as `ISIZE`; not done now because that
  heuristic can hide a partial replica whose file happens to be large.
- **Above the merge cap the full read is one source and incomplete by
  construction.** The header says which; the docs say why. A streaming
  union would need sorted inputs, which arrival-ordered files are not.
- **`total_bytes` is the job log size, not the step's**, even on the step
  endpoint. Computing the step's own size would need a full scan; the
  banner wording uses the job size explicitly.
- **Two concurrent worst-case merges exceed today's production headroom**
  (§ 3.4). The merge cap is configurable; the deploy-side limit bump is
  the real fix for headroom and is the user's.
