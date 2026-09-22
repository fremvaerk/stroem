# Log reads: tail by default, streamed full log

Status: revision 4, proposed (2026-09-22)

Companion of `docs/internal/TODO.md` § Performance ("Opening the job page of a
job with a large log OOM-kills every server replica") and of the HA log
mirroring record in TODO.md § "Review: HA Log Mirroring (2026-05-21)".

## Revision history

**Revision 4 (2026-09-22).** After the third Codex review (5 of 9 round-two
items resolved, 4 partial, 13 new). Mechanisms whose allocation was named
but not pinned are now pinned: the line splitter LENDS a borrow of its one
preallocated accumulator instead of yielding owned bytes (F1); the step
matcher is a hand-written serde visitor that compares inside the callback,
copies nothing out, and keeps today's object-only, last-key-wins semantics
(F2, F10); the tail envelope is serialised into a `Vec` preallocated to its
exact worst case instead of axum's growing buffer (F4); the merge term
counts the merger's own byte-based reservation and the sort scratch, both
bounded by the input cap (F3); the archive path is stated as two range
buffers plus one reader buffer with the decoder pinned to
`async-compression 0.4` (F5). Behaviour changes agreed with the user:
`appendTail` removes, from the end, as many copies of each line as the tail
contains, so real repeats survive (F11); an unterminated last line is
dropped only from a LIVE `.jsonl` file (F9); an unfiltered archive tail is
a byte ring with no line cap, and a skipped line always sets `truncated`
(F7); archive reads are pinned to the object version seen at open (F8).
`total_bytes` uses the archive's counted bytes, never the trailer (F6);
the step tail applies `L` like every filtered read, so a whole line inside
a `T > L` window cannot exceed the matcher term (F2); citations and the
fixture list corrected (F12, F13); the table's figures are ceilings.

**Revision 3 (2026-09-22).** After the second Codex review. Every memory
figure derived from an enforced limit; `merge_max_lines`; range-read
archive access; own line splitter; borrowed step deserializer; exact
snapshot-range tails; `total_bytes` as an upper bound; `appendTail` as a
union by exact line.

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
  signal-only and stay on the publishing replica (`ha_test.rs:527-537`),
  chunks published while a listener is down are lost to that replica
  (`ha_test.rs:770-780`), and a mirror disk write can fail. So a replica's
  file is typically most of the log, never guaranteed all of it — and for
  this job it was large enough on both replicas to kill both.
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
  inputs retained, a `HashSet` and a `Vec` reserved from the inputs' byte
  length (`:36-38`), an output sized to both inputs (`:50`), and a stable
  sort (`:48`). The per-step variant (`get_step_log_from_archive`,
  `:523-547`) buffers the compressed object and decompresses line by line,
  accumulating only matches — smaller, but still unbounded in the matches.

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
   every bound in § 3.4 is a buffer the code allocates to a fixed capacity,
   a `take`, or a configured cap — with ONE named exception, the gzip
   decoder's internal state, which is a pinned library's constant and is
   checked by the RSS test (§ 5).
2. The UI keeps a live, auto-following view of a running step, and can still
   show the whole log of a long run on demand.
3. Every existing invariant of the read path survives: `.jsonl` → legacy
   `.log` → (terminal) archive fallback with `NotFound` never a 500
   (retention TOCTOU), archive errors degrade to local whenever a fallback
   is still possible, archive only for terminal jobs, union recovery of
   mirrored-away lines for terminal jobs whenever it fits the caps,
   exact-match step filter with today's semantics, `_server` as a
   pseudo-step, "keep last non-empty body" in the UI, EOF content of
   legacy and finished logs never hidden.
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
envelope is NOT produced by `axum::Json` (which serialises into a growing
`BytesMut`, `axum-0.8.9/src/json.rs:233-237`): the handler serialises
`TailResponse` with `serde_json::to_writer` into a `Vec::with_capacity(6 ×
logs.len() + 256)` — `6 ×` is serde_json's worst-case escape (`\u00XX` for
one byte) and 256 bytes cover the four field names, punctuation and two
`u64`s — and returns that `Vec` as the body with the JSON content type. The
capacity is the bound; a `debug_assert!` that no reallocation happened
guards it in tests.

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
  a match or an oversize line was left out (§ 3.2, § 3.3), or the merged
  tail was cut (§ 3.3). For a step tail it can be `true` even when no
  earlier line of that step exists; `false` is exact.
- `total_bytes` is an UPPER BOUND on the size of the complete job log, from
  the sources consulted: the local file's length snapshot; the archive's
  decompressed length as COUNTED while streaming it (never the gzip
  trailer, which wraps at 4 GiB); their SUM when both were consulted (a
  union of two sources is at most their sum; `max` would be wrong for
  disjoint sources). `returned_bytes <= total_bytes` always holds. It is
  the size of the JOB log, not of the step's share.
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
tail_bytes`; `L = max_line_bytes`; `K = 64 KiB`, the reader and output
chunk size; `R = 1 MiB`, the archive range-read size; `D`, the gzip
decoder's internal state (§ 3.3); `C = merge_max_bytes`; `N =
merge_max_lines`.

**Local file resolution** (both entry points): `.jsonl` first, then legacy
`.log`, exactly the order of today's `read_local_log` (`log_storage.rs:386`).
`NotFound` on both is `Ok(None)` — retention can delete the file mid-request
(`:380-385` keeps its comment and its behaviour) — and the caller falls
through to the archive when the job is terminal (§ 3.3), else to an empty
result with `source: None`. Every other I/O error propagates as today
(`:393-409`), including one that occurs mid-read.

**The step matcher.** `line_matches_step` (`:508-516`) keeps its contract —
the `contains` fast guard, then the line must parse as a JSON OBJECT whose
`step` is a string exactly equal to the name (so `build` never matches
`build-docs`), with the LAST `step` key winning if the key repeats, which is
what `Value`'s repeated map insertion does today
(`serde_json-1.0.150/src/value/de.rs:137-141`) — but its body changes from
`from_str::<Value>` to a hand-written `serde::de::Visitor` driven by
`serde_json::Deserializer::from_str(line).deserialize_map(visitor)`:
`visit_map` walks the entries, deserialises every key as `&str`, skips every
value except `step`'s with `IgnoredAny`, deserialises `step`'s value
through a nested visitor whose `visit_borrowed_str` and `visit_str` both
just record `v == step_name` (overwriting on repeat, so last wins), and
whose every other `visit_*` records `false`. Arrays and scalars fail
`deserialize_map`, so `["build"]` does not match — as today. Nothing is
copied out of the parser: an unescaped `step` is compared in place, an
escaped one is compared from serde_json's scratch buffer, which holds at
most the line's length (`serde_json-1.0.150/src/read.rs:520-529`). So
matching a line of `n` bytes allocates at most `n` bytes, and every
filtered read only ever hands it lines of `<= L` bytes. A derived struct
was rejected because it accepts the positional form and rejects duplicate
keys (`serde_derive-1.0.228/src/de/struct_.rs:70-94`, `:267-269`).

**Unfiltered tail (`StepFilter::All`), local.** `len = metadata().len()` is
the snapshot and `total_bytes`. `start = max(0, len - T)`; `seek(start)`;
`take(len - start).read_to_end(&mut buf)` with `buf` preallocated to `len -
start <= T` — exactly the snapshot range `[start, len)`, never a byte past
it. `append_log` (`:192-199`) writes and flushes under its own handle lock
that readers do not share, so the file can grow after the snapshot; those
bytes are left for the next poll, which keeps `returned_bytes <=
total_bytes`. Then: if `start > 0`, drop everything up to and including the
first `\n`. Then the TORN-LINE rule: if the buffer does not end in `\n` AND
the file is `.jsonl` AND the job is not terminal, drop the trailing partial
line — the writer terminates every JSONL line, so an unterminated tail of a
live file is a chunk write in progress at snapshot time, and it comes back
whole on the next poll. A terminal job's file and a legacy `.log` file keep
their last line however it ends: nobody will finish it, and today's reads
return it (`:406-411`; fixture at `:1160` is a newline-free legacy file).
`truncated = start > 0`. Peak: `buf` (`<= T`) + tokio's file buffer (`<=
min(T, 2 MiB)`, sized to the read request,
`tokio-1.52.3/src/fs/file.rs:290`, `:620-628`) = **2 T**.

**Step tail (`StepFilter::Step`), local.** A quiet step next to a chatty one
would return nothing from the last 256 KiB, so the step tail scans BACKWARDS
over the snapshot range in windows of `T`, each read with `seek` + `take`
into one reused window buffer of `T`. Each window is split at newlines; the
partial first line is carried into the next window in a carry buffer of
capacity `L`. It is a filtered read, so `L` applies to EVERY line, not only
carried ones: a whole line longer than `L`, whether inside a window (`T` may
exceed `L`) or assembled across windows, is skipped with a `warn!` and sets
`truncated`; that is what keeps the matcher's input, and so its scratch,
`<= L`. Each whole line `<= L` is tested with `line_matches_step`; matches
are collected newest-first into a result buffer preallocated to `T`. The
scan stops when the next match would push the result OVER `T` (that match
is NOT taken and sets `truncated`), when it reaches the start of the file,
or when it has covered `tail_scan_max_bytes` (64 MiB, sets `truncated`).
Output is reversed back into file order. `truncated = !reached_start ||
excluded_match || scan_cap_hit || oversize_line_skipped`; so a `false` is
exact and a `true` may be conservative (an earlier line of that step may
not exist). The cap keeps a 3 s `_server` poll on a gigabyte log from
re-reading the whole file every tick; 64 MiB covers this incident's log in
one scan. The torn-line rule above applies to the newest window before
splitting. Peak: window `T` + file buffer `T` + result `T` + carry `L` +
matcher scratch `L` = **3 T + 2 L**.

**Full, unfiltered, local.** `tokio::fs::File` →
`ReaderStream::with_capacity(_, K)` → `axum::body::Body::from_stream`
(polls one item per frame, `axum-core` `body.rs:209-218`). Tokio's file
buffer is sized to the request, so `<= K`. Peak **2 K**. It never
materialises the log.

**Full, filtered, local.** Not `FramedRead`: its `BytesMut` grows by
doubling (`tokio-util-0.7.18/src/codec/framed_impl.rs:215`,
`bytes-1.12.0/src/bytes_mut.rs:700-760`) and it ends the stream after any
decoder error (`framed_impl.rs:161-168`, `:199-204`). Instead a LENDING
splitter:

```rust
pub struct LineSplitter<R: AsyncBufRead> { reader: R, acc: Vec<u8> /* with_capacity(max_line), allocated once */, max_line: usize }
pub enum LineRef<'a> { Line(&'a [u8]), Skipped { bytes: u64 } }
impl<R> LineSplitter<R> {
    pub async fn next(&mut self) -> io::Result<Option<LineRef<'_>>>;
}
```

`next` clears `acc` (capacity retained), copies from `fill_buf` into `acc`
up to the next `\n`, `consume`s what it copied, and returns `Line(&acc)`;
when `acc` would exceed `max_line` it switches to discard mode, `consume`s
bytes without copying until the newline, and returns `Skipped`. The
returned `LineRef` borrows `acc` until the next call — no line is ever
owned by anyone else, so there is no second `L`. A final line without a
newline is returned as a `Line`. The stream never ends early on an oversize
line. The HTTP body is a hand-written `Stream` that OWNS the splitter, the
step name and one output buffer `Vec::with_capacity(K)`: on each poll it
calls `next` until the output buffer would overflow, appending each
matching `Line` plus `\n` (a match longer than the remaining space is
emitted as its own chunk; it is `<= L` by construction), yields the buffer
as `Bytes` (the `Vec` is handed over, a new one of `K` is allocated for the
next chunk, so at most one `K` in flight plus one being filled), logs and
drops `Skipped` frames. Unfiltered full mode has no line cap because it
never splits lines. The reader is `BufReader::with_capacity(K, file)`.
Peak: reader `K` + file buffer `K` + `acc` `L` + matcher scratch `L` + two
output buffers `2 K` = **2 L + 4 K**.

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
this feature does not use `get_stream`. It adds a versioned range-read
session with identical semantics on every backend (`S3BlobArchive` `:279`,
`LocalBlobArchive` `:108`, the in-memory and test archives):

- `async fn open(&self, key) -> Result<Option<ArchiveObject>>` — `None`
  when the object does not exist. `ArchiveObject { size: u64, version:
  Version }`: S3 via `head_object` (`size` = content length, `version` =
  the ETag); local opens the file and keeps the descriptor (`size` =
  `metadata().len()` of THAT descriptor, `version` = the descriptor
  itself). A local publish replaces the path atomically (`:172-177`) and
  an S3 write replaces the key (`:280-289`), so the session, not the key,
  is what a read is consistent with.
- `async fn read_range(&self, obj: &ArchiveObject, offset: u64, len: u64)
  -> Result<Bytes>` — the bytes in `[offset, min(offset + len, size))`,
  EMPTY when `offset >= size`, never more than `len`. S3 sends
  `get_object().range("bytes=offset-(offset+len-1)").if_match(etag)`; a
  412 (object replaced) and a 416 (unsatisfiable range) are archive errors
  and empty respectively; the SDK body is `collect()`ed, which holds the
  response's segments (`<= len` in total) and a contiguous copy (`<= len`),
  so a range read costs **2 R** at its peak
  (`aws-smithy-types-1.5.0/src/byte_stream.rs:574-581`,
  `bytes-utils-0.1.4/src/segmented.rs:391-394`). Local reads from the held
  descriptor at `offset`, unaffected by a later replacement of the path.

`ArchiveRangeReader { archive, obj, pos, buf: Bytes }` implements
`AsyncBufRead` directly over the last `read_range` result (`fill_buf`
returns the unread part of `buf`, fetching the next `R` when it is empty),
so it adds no buffer of its own beyond that `Bytes`. `GzipDecoder` from
`async-compression` **0.4** (features `tokio`, `gzip`; the exact patch
version is what `Cargo.lock` resolves and is recorded in the implementation
commit) decodes it; its state `D` — the 32 KiB inflate window plus tables —
is the single library-internal term in § 3.4; it is not enforced by us and
the RSS test (§ 5) is what checks it. The decoder implements `AsyncRead`,
so a `BufReader::with_capacity(K, decoder)` sits between it and any line
splitter.

`ISIZE`: the last 4 bytes of a gzip member are the uncompressed length mod
2^32. `upload_to_archive` writes ONE member (`GzEncoder::new` … `finish()`,
`log_storage.rs:280-288`), so `read_range(obj, size - 4, 4)` on an object of
`size >= 18` (the smallest valid gzip) gives the decompressed length exactly
for objects under 4 GiB; above that it wraps. It is used for ONE purpose,
the merge GATE, and every decompression runs through `take` so a wrong,
wrapped or stale value cannot cause an allocation. It is never reported as
`total_bytes`. An object shorter than 18 bytes is treated as corrupt (an
archive error).

**Tail (both filters), terminal.** Two bounded tails are read, then merged:

- The local tail as in § 3.2.
- The archive tail. UNFILTERED: the decoded bytes flow into a byte ring of
  capacity `T` (two `Vec`s of `T` swapped, or a `VecDeque<u8>` with
  capacity `T`, either way allocated once) with oldest-first eviction, no
  line splitter and no line cap, so a long line survives exactly as it does
  from the local file; at EOF the ring is cut at its first newline unless
  the ring holds the whole object; `archive.truncated = decompressed >
  T`. FILTERED: `LineSplitter` (§ 3.2, `max_line = L`) over the decoder's
  `BufReader`; each matching `Line` is appended to a ring of whole lines
  capped at `T` bytes with oldest-first eviction; `archive.truncated` is
  set when a line is evicted or a `Skipped` frame is seen. Both count
  decompressed bytes for `total_bytes`. Computed COMPLETELY before the
  response starts, so an archive error at any point (open, 412, download,
  gzip, footer) degrades to the local tail exactly as today (`:358-366`;
  `test at :1419-1425`).
- If both exist and `lines(local) + lines(archive) <= N`, the two tails
  (each `<= T`) go through the existing `merge_jsonl_logs` unchanged and the
  result is cut from the front at a line boundary to `<= T`. `source:
  merged`. If the line cap is exceeded the local tail is served alone
  (`source: local`).
- `truncated = local.truncated || archive.truncated || merged_was_cut ||
  union_skipped_for_lines`. `total_bytes = local_len +
  archive_decompressed_count` when both were consulted (an upper bound;
  `returned <= total` holds).
- Cost: one archive stream per FETCH on a terminal job. The job page
  issues one fetch for the expanded step (`step-detail.tsx:69`) and one for
  `_server` (`server-events.tsx:21`), each once, since a terminal job is not
  polled — two archive streams per page view, plus one per further step the
  user expands.
- Peak (the merge phase dominates; the scan buffers are released before
  it): local result `T` + archive ring `T` + range buffers `2 R` + decoder
  `D` + decoder reader `K` + `acc` `L` + matcher scratch `L` + merge
  overhead `M` (below) + merge output `2 T` + cut result `T` + envelope
  `6 T` = **11 T + 2 L + 2 R + K + D + M**.

**The merge overhead `M`.** `merge_jsonl_logs` is unchanged. It reserves
`(a.len() + b.len()) / 80 + 1` entries in both a `HashSet<&str>` (24 B per
slot) and a `Vec<&str>` (16 B per slot) (`:36-38`), grows them if more
lines exist, and stable-sorts the `Vec` (`:48`), whose scratch is at most
half the `Vec`. With `I` the input bytes and `N'` the actual line count (`<=
N` by the gate): `M = 40 × max(I / 80, N') + 8 × N'`, i.e. `M <= 0.5 I +
48 N'`. Both terms are bounded: `I` by `2 T` (tail) or `C` (full), `N'` by
`N`.

**Full, terminal, under the caps.** Gate: `local_len + isize <= C`. If it
holds, local is read through `take(C)` into a buffer, then the archive is
decompressed through `take(C - local_read + 1)`; if either `take` fills
(the local file grew past its snapshot, the trailer lied or wrapped, the
object changed), the merge is ABANDONED before any output and the
single-source rule below applies. Lines are counted while reading; if
`lines > N` the merge is likewise abandoned. Otherwise `merge_jsonl_logs`
runs unchanged and the result is streamed from that one string in `K`
chunks. `source: merged`. This preserves today's behaviour for every
normal-sized log. Peak: inputs `<= C` + `M` (`<= 0.5 C + 48 N`) + output
`<= C` + range buffers `2 R` + decoder `D` + reader `K` = **2.5 C + 48 N +
2 R + K + D**.

**Full, terminal, above a cap or archive size unknown.** ONE source is
streamed: the LOCAL file when it exists (§ 3.2 full modes), else the
archive through `ArchiveRangeReader` → `GzipDecoder` → `BufReader(K)` (→
`LineSplitter` when filtered) in `K` chunks. Local first because a
plain-file read fails far less often mid-stream than a multi-request
download and because it is the only place the post-upload lines (§ 2) can
be; either source can still error mid-stream (`:393-409` propagates a
mid-read I/O error), and when that happens the body simply ends early —
once headers are sent there is no fallback and the header cannot be
rewritten. An archive error BEFORE the first byte (missing object, `open`
failure) falls back to local when present, else answers an empty 200 with
`source: none`. `source: local` or `archive`; the docs state that neither
is guaranteed complete. Peak: local as in § 3.2; archive **2 R + D + K +
2 K** unfiltered (reader plus two output buffers), **2 R + D + K + 2 L +
2 K** filtered.

Non-terminal jobs never touch the archive (unchanged; the upload happens at
terminal time, so a pre-terminal archive read is a guaranteed miss).

### 3.4 Memory bounds

Per request, counting every buffer alive at the peak. Enforcement of each
symbol: `T` — `take` and buffers preallocated to `T`, `<= tail_max_bytes`;
`L` — the splitter's single accumulator and the carry buffer, preallocated,
and every filtered line rejected above `L`; `K = 64 KiB` — `BufReader`,
`ReaderStream` and output buffer capacities; `R = 1 MiB` — the `len` of
every `read_range`, whose response cannot exceed it; `D` — the pinned
decoder's inflate state, the one library constant, checked by the RSS test;
`C` — `take` on both merge inputs; `N` — the line counter; `E = 6 T + 256`
— the envelope `Vec`'s preallocated capacity; `M <= 0.5 I + 48 N'` — the
merger's reservation and sort scratch, with `I` and `N'` capped as above.
Figures are CEILINGS of the formulas, with `D` taken as 64 KiB.

| Read | Peak | Default (`T` 256 KiB) | Max (`T` 4 MiB, `N'` = N) |
|---|---|---|---|
| Tail, unfiltered, local | `2 T + E` | 2.0 MiB | 32.0 MiB |
| Tail, step, local | `3 T + 2 L + E` | 4.3 MiB | 38.0 MiB |
| Tail, terminal, merged | `11 T + 2 L + 2 R + K + D + M`, `M <= T + 48 N'` | 7.2 MiB + `48 N'` (≤ 12 MiB adversarial; ~0.1 MiB typical) → 19.2 MiB | 48.2 MiB + 4 MiB + 12 MiB → 64.2 MiB |
| Full, unfiltered, local | `2 K` | 0.2 MiB | 0.2 MiB |
| Full, filtered, local | `2 L + 4 K` | 2.3 MiB | 2.3 MiB |
| Full, archive, single source | `2 R + D + 3 K` (`+ 2 L` filtered) | 2.3 MiB (4.3 MiB) | same |
| Full, terminal, merged | `2.5 C + 48 N + 2 R + K + D` | 54.2 MiB | 54.2 MiB |

With defaults, a UI poll costs at most 4.3 MiB and the most expensive read
any client can trigger costs about 64.2 MiB (a 4 MiB terminal tail over
adversarial one-byte lines), against ~100 MiB of headroom in production
today and against the 512 Mi limit the helmfile bump raises to 2 Gi. Two
concurrent worst-case reads do not fit today's headroom, which is one more
reason every cap is configurable and the bump is due. The "typical" figure
is not a bound and is not relied on.

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
| `max_line_bytes` | 1048576 (1 MiB) | longest single line a FILTERED read will carry; longer lines are skipped with a warning and set `truncated` |
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
  `appendTail(displayed: Line[], tail: Line[]): {lines, gap}` — a MULTISET
  difference from the end, then append; nothing about time, because a file
  window is not a time range (§ 1):
  1. `counts = Map<raw, number>` over the tail (how many copies of each
     line the tail holds).
  2. Walk `displayed` from the END; for each line, if `counts.get(raw) >
     0`, decrement and drop the line, else keep it. Removed lines are the
     newest copies, which is where the tail overlaps.
  3. `lines = kept.concat(tail)`.
  4. `gap = displayed.length > 0 && tail.length > 0 && nothing was
     removed` — no tail line had been displayed, so nothing ties the tail
     to the view; the viewer inserts a visible "gap" divider row.
  Properties: idempotent (re-applying the same tail removes and re-adds
  exactly its lines); no cross-duplication under arrival-ordered tails
  (`[B, A, C]` + the same tail → `[B, A, C]`); a line another replica
  recovered in the middle is shown (`[A, C]` + `[A, B, C]` → `[A, B, C]`);
  a stale mirrored straggler in the tail is appended once, not used to cut
  history; real repeated lines survive with their multiplicity (`[X, X]` +
  `[X]` → `[X, X]`; `[A]` + `[B, B]` → `[A, B, B]`). It is deliberately NOT
  the server merge's collapse of identical lines: repeated lines in a log
  are real events and the viewer shows them. Ordering is "everything not
  in the tail, then the tail", which for a running step means older content
  then the newest window; a straggler can sit out of time order, as it does
  in the file. Cost per poll is `O(|displayed| + |tail|)` map lookups (~475 k
  for the incident log), tens of milliseconds; if that ever matters the walk
  can stop once every tail count is zero, not done now. `appendTail` lives
  in `ui/src/lib/log-tail.ts` with Vitest coverage (§ 5).

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
   inputs, trailer never reported** (revisions 2–4). A compression-ratio
   estimate is not a bound (repetitive JSONL compresses 300 ×); `ISIZE` is
   exact under 4 GiB for the single-member objects we write, `take` makes
   a wrong, wrapped or stale trailer — and a local file that grew — safe,
   and `total_bytes` uses the counted length so a wrapped trailer cannot
   break `returned <= total`.
8. **Above the caps, local first, archive last** (revision 2). A streaming
   response cannot fall back after its headers; the local file is the source
   least likely to fail mid-stream and the only one holding post-upload
   lines. Neither source is complete; the header names which answered.
9. **Keep the in-memory merge under the caps.** Removing it would silently
   drop the HA-recovery behaviour for the common case where it costs a
   bounded, stated amount; its reservation and sort scratch are counted.
10. **A line cap beside the byte cap** (revision 3). The merge's per-line
    entries do not depend on line length; a byte cap alone lets one-byte
    lines multiply them by 40.
11. **A lending splitter on `fill_buf`/`consume`, not `FramedRead` and not
    an owned-item stream** (revisions 2–4). `FramedRead` ends the stream
    after any decoder error and grows by doubling; an owned item per line
    is a second `L`; a borrow of one preallocated accumulator is a
    constant.
12. **Versioned range reads, not the SDK stream, for the archive**
    (revisions 3–4). The server sizes every archive read to `R`, and pins
    it to the object version it opened, so a replaced object is a clean
    412, not a silent mix.
13. **A hand-written `step` visitor** (revisions 3–4). The step matcher is
    on the hot path of every filtered read; a `Value` per line is an
    allocation multiplier no table can bound, and a derived struct changes
    the accepted shapes. The visitor compares in place and keeps today's
    semantics.
14. **Virtualised viewer that can load the whole log** (the user's choice
    over "page backwards" and "download only"), TanStack Virtual, no Pretext.
15. **`appendTail` is a multiset difference from the end, then append**
    (revisions 3–4). Timestamp cuts duplicated under arrival-ordered tails;
    an overlap search dropped recovered lines; a set-based union collapsed
    real repeats; the multiset rule is idempotent, duplication-free and
    keeps every real line.
16. **Torn lines are dropped only where a writer will finish them**
    (revision 4): a live `.jsonl`. Finished and legacy files keep their
    last line however it ends.
17. **Unfiltered reads never apply the line cap** (revision 4), from either
    source; `L` belongs to filtered reads, where the matcher needs a whole
    line in memory.
18. **Remove the String-returning read functions** so the unbounded path
    cannot be reintroduced by a future caller; every existing call site,
    including the nine in integration tests, moves to the bounded API.

## 5. Testing

Unit (`log_storage.rs`):
- tail: cut at a line boundary; larger than the file → whole file,
  `truncated: false`; empty file; a single line longer than the window →
  empty + truncated; a file that grows after the snapshot returns exactly
  the snapshot range (append from a second task between `metadata` and the
  read; assert `returned_bytes <= total_bytes`); the torn-line matrix —
  live `.jsonl` unterminated last line dropped and returned whole on the
  next read; terminal `.jsonl` unterminated last line kept; legacy `.log`
  without a final newline (the `:1160` fixture) returned in full with
  `truncated: false`.
- step tail: matching line straddling a window edge; quiet step behind a
  chatty one found beyond the first window; the match that would cross `T`
  is not taken and sets `truncated` (including when the scan then reaches
  the file start); scan cap reached → truncated; scan reaching the start
  with nothing excluded → not truncated; `build` vs `build-docs`; a whole
  line over `L` inside one window skipped and flagged; a carried line over
  `L` skipped and flagged.
- resolution: missing `.jsonl` falls to `.log`; both missing, non-terminal
  → empty, `source: None`, no error (retention TOCTOU); both missing,
  terminal → archive.
- matcher: every existing fixture (`:768-930`, `:1478-1505`) unchanged; NEW
  fixtures — positional form `["build"]` does not match; duplicate `step`
  keys, last wins; non-string `step` does not match; escaped step name
  (`"build"`) matches `build`; `step` absent; nested object with an
  inner `step` does not match.
- terminal tail: local + archive → merged content equals the union of the
  two tails, cut to `T`, `source: merged`, `total_bytes = local_len +
  counted archive bytes`; archive error → local tail, `source: local`;
  local missing → archive ring with correct `total_bytes`; step tail from
  the archive; line cap exceeded → local only, `truncated`, `source:
  local`; UNFILTERED archive tail with `T` = 4 MiB keeps a whole 2 MiB
  line; FILTERED archive tail skips it and sets `truncated`; an evicted
  matching line sets `truncated`.
- `LineSplitter`: oversize line yields `Skipped` and the FOLLOWING line is
  delivered; the stream never ends early; `acc.capacity()` equals
  `max_line` before and after an oversize line and after 10 000 lines;
  lines split across `fill_buf` boundaries; final line without newline;
  empty input.
- `ArchiveRangeReader` / `open` / `read_range`: object shorter than `R`;
  object an exact multiple of `R`; `read_range` past the end returns empty;
  `open` of a missing key is `None`; on the local backend, the path
  replaced mid-read still serves the opened version; on the in-memory test
  archive, a version bump mid-read is an archive error (the S3 412 path is
  covered in the S3 suite).
- merge gate: `ISIZE` read from a real gzip member; under both caps →
  merged; over the byte cap → single source; over the line cap → single
  source; a trailer claiming a small size for a large body fills `take` and
  falls to single source before any output; a local file that grew past the
  gate fills its `take` likewise; object under 18 bytes → archive error →
  local.
- envelope: the preallocated `Vec` never reallocates for a body of `T` bytes
  of `\u0001` (the worst escape), asserted on `capacity()` before and after
  `to_writer`.
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
  and `integration_test.rs` keep their assertions; new S3 tests: `open`,
  `read_range` (including an unsatisfiable range → empty), a 412 when the
  object is overwritten between `open` and `read_range`, and a real gzipped
  object streamed into a tail through range reads.

Memory regression (the test this incident needs):
- `#[ignore]`d, Linux-only integration test that writes a 100 MiB synthetic
  JSONL log, reads `/proc/self/statm` before and after a `full=true` read,
  a step tail, and a terminal merged tail against a 100 MiB archive object,
  and asserts RSS grew by less than the § 3.4 bound for that mode plus
  8 MiB of allocator slack — this is also what validates `D`. A second
  variant uses one-byte lines to exercise the line cap; a third uses
  `\u0001`-only content to exercise the envelope's worst case. Run on
  demand and in the release checklist.

CLI: `--full` handles NDJSON and the JSON envelope; the truncation notice
goes to stderr; `--tail-bytes` is forwarded.

UI (Vitest): `appendTail` — plain overlap; the same tail applied twice is a
no-op; the arrival-ordered counterexample `[B, A, C]` does not duplicate;
recovered middle line shown; straggler appended once; gap only when nothing
was removed; `[X, X]` + `[X]` → `[X, X]`; `[A]` + `[B, B]` → `[A, B, B]`;
`[A, A]` + `[B]` → `[A, A, B]`; empty inputs. Banner rendered when
`truncated`, absent otherwise; confirmation above 64 MiB;
`step-detail.test.tsx` updated for the envelope; a `log-viewer` test that a
10 000-line body renders fewer than 200 rows (virtualisation is in effect;
jsdom needs the size mock the TanStack docs describe). Playwright
(`ui/e2e/log-streaming.spec.ts`): banner appears for a log above the
default tail and "Load full log" renders the first line.

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
  be skipped when the local file's length equals or exceeds the counted
  archive size of a previous read; not done now because that heuristic can
  hide a partial replica whose file happens to be large.
- **Above the caps the full read is one source and incomplete by
  construction.** The header says which; the docs say why. A streaming
  union would need sorted inputs, which arrival-ordered files are not.
- **`D` is the one term this spec does not enforce.** It is the pinned
  decoder's inflate state; the RSS test measures it. If a future
  `async-compression` changes it materially the test fails, which is the
  intended signal.
- **`total_bytes` is an upper bound of the job log, not the step's share.**
  The banner says "of up to".
- **Adversarial one-byte lines** push a 4 MiB terminal tail to ~64 MiB.
  Bounded and stated; lower `tail_max_bytes` or `merge_max_lines` if that
  headroom is not available. The deploy-side limit bump is the real fix for
  headroom and is the user's.
