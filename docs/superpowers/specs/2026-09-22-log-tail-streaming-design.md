# Log reads: tail by default, streamed full log

Status: revision 8, proposed (2026-09-23)

Companion of `docs/internal/TODO.md` § Performance ("Opening the job page of a
job with a large log OOM-kills every server replica") and of the HA log
mirroring record in TODO.md § "Review: HA Log Mirroring (2026-05-21)".

## Revision history

**Revision 8 (2026-09-23, implementation review).** After Codex reviewed the
implementation (thread 01a0c805) and found nine real issues, independently
verified. MCP formatting is now part of the bounded path, not outside it:
`format_logs` (§ 3.1) parses only the four fields it renders — `ts`,
`step`, `stream`, `line` — through a borrowing visitor that drains every
other field with `IgnoredAny` without materialising it, so a tail line
carrying a large unrelated array costs only that field's syntax scan, not
its size; § 3.4 gains a row for the resulting peak, `tail formula + 2 T`.
A filtered archive tail's ring (§ 3.3) empties itself, not just skips the
line, when a single match's own size exceeds the whole window — the same
outcome as the local scan, which stops scanning further back the moment a
match can't fit, so the two no longer disagree on which older matches an
oversize record evicts. The single-source archive stream — filtered or
not — is served exactly as stored, including an unterminated final
record: there is only one source, so nothing else could ever complete it,
and the FILTERED branch was wrongly dropping it. The pre-first-byte
archive fallback (§ 3.3) covers more than `open`: the stream is now PRIMED
(one `fill_buf` through a `BufReader` wrapping the decoder — the first
range read, the gzip header, and the first inflate) before the response's
headers are committed, so a failure in any of those three still falls back
to local when present, else an empty `LogSource::None` response, instead
of aborting an already-started body; priming's extra `BufReader` adds one
more `K` to the unfiltered single-source archive stream's peak, `R + H + D
+ 3 K` → `R + H + D + 4 K` (the filtered branch already read through a
`BufReader` it reuses for priming, so its own formula is unchanged).
§ 5's fixture (d) is corrected: a MATCHING line can only reach `~L` of
matcher scratch, not `2 L` — matching this fixture's design needs the
decoded step name to also appear literally elsewhere in the line (to pass
the `contains` fast guard on an escaped value), which halves the room left
for the escaped value itself; the `2 L` case is a NON-matching line whose
guard passes because the target appears in `line` instead, freeing nearly
the whole line for one escaped string. Ranged archive reads (§ 3.3) pin
the object's version with `If-Match` when the endpoint returned an `ETag`,
else `If-Unmodified-Since` against its `Last-Modified` when THAT exists,
else neither — some S3-compatible endpoints omit `ETag` — logging one
`warn!` at `open` when neither header exists, since ranged reads of that
object are then not version-pinned at all. Where the sections below
differ, this paragraph wins.

**Revision 7 (2026-09-23, planning).** Ten amendments decided while turning
this design into an executable task plan. Config keys are nested under
`log_storage.read` (`STROEM__LOG_STORAGE__READ__TAIL_MAX_BYTES`), not flat
under `log_storage`, because `LogStorageConfig` is built by struct literal
in about 80 places and one nested field is one line per literal, while
`#[serde(flatten)]` would break the `config` crate's env-string coercion.
An escaped step VALUE does not match, exactly as today, since
`line_matches_step` keeps its `line.contains(step_name)` fast guard and
`"build"` does not contain `build`; § 5's fixture list is corrected
accordingly, plus a companion case where the name also appears literally
elsewhere in the line, which does match. Filtered stream output buffers
are `K + L + 1` bytes, and a chunk is yielded once it reaches `K`, so a
matching line (≤ L) always fits without the splitter having to hold a
line back; filtered-full local peak becomes `5L + 4K`, and unfiltered-full
local adds the backward newline scan buffer, `3K`. Full reads obey the
torn-line rule for local `.jsonl` files — the stream stops at the last
newline of the snapshot — the merged full read trims both inputs, and the
single-source archive stream is served exactly as stored. The merged full
read decompresses the archive through `take(isize + 1)` into a buffer of
`isize + 33`; any size other than exactly `isize` abandons the merge
before output, bounding the allocation by the real input instead of by
`C − local_len`. The viewer parses only the rows it renders (virtualised),
not the whole body up front; the row-height estimate uses the raw line
length minus a fixed JSON overhead. There is no in-memory
"version bump mid-read" test — the trait's default `open` holds a
whole-object snapshot, consistent by construction, and the local backend
(held descriptor) and S3 (`If-Match` → 412) carry the version tests
instead. `appendTail` inserts the gap marker into its returned `lines`
itself. Peak-test fixture (e) runs with `merge_max_lines = 2` so the
one-line inputs still merge, fixture (f) is covered by a unit test on
`merge_jsonl_logs`' capacity instead, and the S3 case lives in the peak
binary, which runs serially, instead of `s3_integration_test.rs`, whose
parallel tests would pollute a global counter. The unfiltered tail reads
one byte before its window to decide whether the window's first line is
complete, instead of always dropping through the first newline. Where the
sections below differ, this paragraph wins.

**Revision 6 (2026-09-22).** After the fifth Codex review (8 of 12
round-four items resolved, 4 partial, 9 remaining, "none requires
abandoning the revised design"). The peak-allocation test gets an explicit
measurement policy — what is counted, baseline subtraction, isolation,
full consumption of streams, the envelope included — and fixtures that
reach the paths they claim to (distinct short lines for the merger,
an under-cap line with a trailing escape for the parser scratch,
newline normalisation through the full-merge path, the archive path
measured in the S3 suite) (findings 1, 2, 5). The matcher's divergence is
documented as a CLASS — malformed content in a field the matcher ignores —
with the three known members pinned by tests (3). The full merge reads the
local file's snapshot length so the two input buffers sum to `C + 65`, not
`2 C` (4). The server-events card shows the banner when a tail is empty but
truncated (7). `appendTail`'s remedy sentence no longer claims a full
reload shows every identical line, since the merger collapses them (8).
Table entries and two citations corrected (6, 9).

**Revision 5 (2026-09-22).** After the fourth Codex review (7 of 13
round-three items resolved, 6 partial, 12 new). Framing change agreed with
the user: the per-mode formulas in § 3.4 are the DESIGN INTENT; the
ENFORCEMENT is a counting global allocator in the test binary that records
peak bytes in use and asserts each mode's measured peak against its formula
× 1.5 on incident-sized and adversarial fixtures (§ 5). That replaces RSS
sampling, which proves nothing about peaks (R12), and turns the remaining
allocator-arithmetic disputes — serde's scratch doubling (R2), hashbrown
rounding and the sort's full-size scratch (R4), SDK segment descriptors
(R5) — into a test instead of prose; the formulas now carry generous
factors for those effects. Concrete fixes: output frames of a filtered
stream are sized to the line cap (R1); the matcher's key visitor accepts
escaped keys, drains compound `step` values, and calls `end()` so trailing
garbage is rejected, with its ONE remaining divergence (an invalid
surrogate escape in an unrelated field) documented and pinned by a test
(R3); every buffer that `read_to_end` fills is preallocated to its limit
plus 32 bytes so tokio never reserves (R4); the S3 range is read through
the async adapter into a preallocated buffer, the previous range buffer
dropped first, in-flight hyper buffer counted (R5); the archive session
carries the key (R6); the merger's output capacity and `total_bytes` allow
for newline normalisation (R7); a dropped torn line sets `truncated` (R8);
since hook events can still append to a finished job, EVERY `.jsonl` file
drops an unterminated suffix and flags it, only legacy `.log` keeps one
(R9); `appendTail`'s guarantee is weakened to observed multiplicity (R10);
ceilings and the envelope constant fixed (R11). `merge_max_lines` default
lowered to 131 072.

**Revision 4 (2026-09-22).** After the third Codex review. Lending line
splitter; hand-written step visitor; exactly preallocated envelope;
versioned archive range reads; unfiltered archive byte ring; torn-line
drop for live files; counted archive bytes in `total_bytes`; multiset
`appendTail`.

**Revision 3 (2026-09-22).** After the second Codex review. Every memory
figure derived from an enforced limit; `merge_max_lines`; range-read
archive access; own line splitter; exact snapshot-range tails;
`total_bytes` as an upper bound.

**Revision 2 (2026-09-22).** After the first Codex review (19 findings).
Terminal tails union bounded local and archive tails; exact `ISIZE` merge
gate guarded by `take`; local-first single source above the cap; migrated
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
  length (`:36-38`), an output sized to both inputs (`:50`) though every
  retained line gets a newline appended (`:52-53`), and a stable sort
  (`:48`). The per-step variant (`get_step_log_from_archive`, `:523-547`)
  buffers the compressed object and decompresses line by line,
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

1. No log read, from any client, can allocate more than a small, stated
   multiple of the configured caps on the server, whatever the log's size
   or content. § 3.4 states a formula per read mode built only from
   buffers the code preallocates, `take` limits and configured caps, with
   explicit factors for allocator growth. The formulas are the design
   intent; the ENFORCEMENT is the peak-allocation test of § 5, which
   measures each mode with a counting global allocator on incident-sized
   and adversarial fixtures and fails when a peak exceeds its formula
   × 1.5. Where a library's internal allocation is involved (serde_json's
   scratch, hashbrown's rounding, the sort's scratch, the gzip decoder, an
   SDK body), the formula carries a factor and the test is the proof.
2. The UI keeps a live, auto-following view of a running step, and can still
   show the whole log of a long run on demand.
3. Every existing invariant of the read path survives: `.jsonl` → legacy
   `.log` → (terminal) archive fallback with `NotFound` never a 500
   (retention TOCTOU), archive errors degrade to local whenever a fallback
   is still possible, archive only for terminal jobs, union recovery of
   mirrored-away lines for terminal jobs whenever it fits the caps,
   exact-match step filter with today's semantics (one documented class
   of divergence, § 3.2), `_server` as a pseudo-step, "keep last
   non-empty body" in the UI, EOF content of legacy logs never hidden.
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
  never does. Today and after this change. (This is also why a terminal
  job's `.jsonl` file is still a file with a writer, § 3.2.)
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
  a match or an oversize line was left out, an unterminated last line was
  dropped (§ 3.2), the union was skipped for the line cap, or the merged
  tail was cut (§ 3.3). For a step tail it can be `true` even when no
  earlier line of that step exists; `false` is exact.
- `total_bytes` is an UPPER BOUND on the size of the complete job log as
  the server would return it: the local file's length snapshot; the
  archive's decompressed length as COUNTED while streaming it (never the
  gzip trailer, which wraps at 4 GiB); their SUM plus 2 when both were
  consulted — a union of two sources is at most their sum, and the merger
  terminates each input's last line with a newline if it lacked one
  (`log_storage.rs:52-53`), at most one byte per input. `returned_bytes <=
  total_bytes` always holds. It is the size of the JOB log, not of the
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
extra line, `[truncated: showing the last 256.0 KiB of up to 83.3 MiB —
earlier lines omitted]`, appended after `format_logs` runs (revision 8:
the trailer's own tests are unaffected, but `format_logs` itself is now
part of the bounded path — it parses only `ts`/`step`/`stream`/`line` out
of each line through a borrowing visitor, draining every other field with
`IgnoredAny`, so it no longer builds a full `serde_json::Value` per line;
§ 3.4 states its peak).

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
chunk size; `R = 1 MiB`, the archive range-read size; `H`, hyper's HTTP/1
read buffer, capped by its default `max_buf_size` (`hyper-1.10.1/src/proto/h1/io.rs`,
408 KiB); `D`, the gzip decoder's internal state (§ 3.3); `C =
merge_max_bytes`; `N = merge_max_lines`.

**Preallocation rule.** Every buffer that a `take(limit).read_to_end(&mut
buf)` fills is created as `Vec::with_capacity(limit + 32)`. tokio's
`read_to_end` reserves 32 bytes only when fewer than 32 bytes of spare
capacity remain (`tokio-1.52.3/src/io/util/read_to_end.rs:86-104`), so with
`limit + 32` of capacity and at most `limit` bytes read it never
reallocates. The peak-allocation test asserts `capacity()` is unchanged
after each such read.

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
(`serde_json-1.0.150/src/value/de.rs:137-141`), and with trailing garbage
after the object rejected, as `from_str`'s `end()` does today
(`serde_json…/de.rs:2507-2517`) — but its body changes from
`from_str::<Value>` to a hand-written `serde::de::Visitor` driven by `let
mut de = serde_json::Deserializer::from_str(line); de.deserialize_map(v)?;
de.end()?`:

- `visit_map` walks the entries. Each KEY is deserialised through a key
  visitor that accepts BOTH `visit_borrowed_str` and `visit_str` (escaped
  keys such as `"step"` arrive through the latter,
  `serde_json…/de.rs:2219-2224`) and records whether the key equals
  `step`; any other key shape records "not step".
- A non-`step` VALUE is consumed with `IgnoredAny`.
- A `step` VALUE is deserialised through a value visitor whose
  `visit_borrowed_str` and `visit_str` record `v == step_name`
  (overwriting on repeat, so last wins), and whose `visit_map`/`visit_seq`
  drain the compound value through `IgnoredAny` and record `false`, and
  whose scalar `visit_*` record `false`. `{"step":{"x":1},"step":"build"}`
  therefore drains the object and then matches on the second key.
- Arrays and scalars fail `deserialize_map`, so `["build"]` does not match
  — as today.

Nothing is copied out of the parser: an unescaped `step` is compared in
place, an escaped one is compared from serde_json's scratch buffer. That
scratch is a `Vec` that grows geometrically as the string is decoded
(`serde_json…/de.rs:59-63`, `read.rs:526-529`, `:882-889`), so for a line
of `n` bytes it can reach `2 n` of capacity; every filtered read hands the
matcher lines of `<= L` bytes, so the matcher term in § 3.4 is `2 L`.

**One documented class of divergence: malformed content in a field the
matcher ignores.** `Value` validates the whole line; the new matcher
validates the structure it needs and skips the rest with `IgnoredAny`,
whose scanner is syntactic. A line with a well-formed `step` that today is
rejected as non-JSON only because of an UNRELATED field is therefore
matched by the new matcher. Known members of the class, from serde_json
1.0.150: an invalid surrogate escape such as `"\uDC00"` in an ignored
string (`read.rs:1033-1040` skips escapes unvalidated; `:911-913`
validates them for real strings); nesting deeper than 128 levels in an
ignored value (`de.rs:1102-1214` iterates without the recursion check that
`Value` parsing applies at `:1372-1378`, `:1910-1915`); an out-of-range
number such as `1e400` in an ignored value (`:1261-1281` checks syntax
only, `:650-664` rejects overflow when `arbitrary_precision` is off, which
is the workspace's configuration, `Cargo.toml:24`). The worker serialises
lines with serde_json, which emits none of these, so no line the system
writes is affected; the class exists only for hand-crafted files. Three
named tests pin the new behaviour (§ 5) so the choice is visible, not
accidental, and any further member found later is added there.

**Torn-line rule** (both tails, local). Every writer of a `.jsonl` file
terminates every line: the worker chunk handler joins lines with `\n` and
appends a final `\n` (`web/worker_api/jobs.rs:984-1004`), `server_log`
builds a newline-terminated record (`settlement/mod.rs:111-119`), and the
mirror copies those chunks verbatim. So an unterminated last line in a
`.jsonl` file is, at any time, a chunk write in progress at snapshot time —
including for a TERMINAL job, whose file can still receive hook events
(§ 2). A `.jsonl` tail therefore drops an unterminated trailing line and
sets `truncated`; the line returns whole on the next poll, and a finished
job's viewer, which does not poll, shows the banner rather than a broken
record. A legacy `.log` file has no writer and no line contract; its
unterminated last line is kept (`:406-411` today; fixture at `:1160`).

**Unfiltered tail (`StepFilter::All`), local.** `len = metadata().len()` is
the snapshot and `total_bytes`. `start = max(0, len - T)`; `seek(start)`;
`take(len - start).read_to_end(&mut buf)` with `buf` preallocated per the
rule above — exactly the snapshot range `[start, len)`, never a byte past
it. `append_log` (`:192-199`) writes and flushes under its own handle lock
that readers do not share, so the file can grow after the snapshot; those
bytes are left for the next poll, which keeps `returned_bytes <=
total_bytes`. Then: if `start > 0`, drop everything up to and including the
first `\n`; then apply the torn-line rule. `truncated = start > 0 ||
torn_line_dropped`. Peak: `buf` (`T + 32`) + tokio's file buffer (`<= min(T,
2 MiB)`, sized to the read request, `tokio-1.52.3/src/fs/file.rs:290`,
`:620-628`) = **2 T**.

**Step tail (`StepFilter::Step`), local.** A quiet step next to a chatty one
would return nothing from the last 256 KiB, so the step tail scans BACKWARDS
over the snapshot range in windows of `T`, each read with `seek` + `take`
into one reused window buffer of `T + 32`. Each window is split at newlines;
the partial first line is carried into the next window in a carry buffer of
capacity `L`. It is a filtered read, so `L` applies to EVERY line, not only
carried ones: a whole line longer than `L`, whether inside a window (`T` may
exceed `L`) or assembled across windows, is skipped with a `warn!` and sets
`truncated`; that is what keeps the matcher's input `<= L`. Each whole line
`<= L` is tested with `line_matches_step`; matches are collected
newest-first into a result buffer preallocated to `T`. The scan stops when
the next match would push the result OVER `T` (that match is NOT taken and
sets `truncated`), when it reaches the start of the file, or when it has
covered `tail_scan_max_bytes` (64 MiB, sets `truncated`). Output is
reversed back into file order. The torn-line rule applies to the newest
window before splitting. `truncated = !reached_start || excluded_match ||
scan_cap_hit || oversize_line_skipped || torn_line_dropped`; so a `false`
is exact and a `true` may be conservative (an earlier line of that step may
not exist). The cap keeps a 3 s `_server` poll on a gigabyte log from
re-reading the whole file every tick; 64 MiB covers this incident's log in
one scan. Peak: window `T` + file buffer `T` + result `T` + carry `L` +
matcher scratch `2 L` = **3 T + 3 L**.

**Full, unfiltered, local.** `tokio::fs::File` →
`ReaderStream::with_capacity(_, K)` → `axum::body::Body::from_stream`
(polls one item per frame, `axum-core` `body.rs:209-218`). Tokio's file
buffer is sized to the request, so `<= K`. Peak **2 K**. It never
materialises the log.

**Full, filtered, local.** Not `FramedRead`: its `BytesMut` grows by
doubling (`tokio-util-0.7.18/src/codec/framed_impl.rs:218`,
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
owned by anyone else. A final line without a newline is returned as a
`Line`. The stream never ends early on an oversize line. The HTTP body is
built with `futures_util::stream::unfold` over a state struct that OWNS the
splitter, the step name and the current output buffer, so the in-progress
`next()` future and its partially consumed line survive a `Poll::Pending`
(a body that re-created `next()` on each poll would lose consumed bytes).
Output buffers are `Vec::with_capacity(L + 1)`: each poll calls `next`
until the next match would not fit, appending each matching `Line` plus
`\n`; a match is `<= L` by construction so it always fits an empty buffer;
the buffer is yielded as `Bytes` and a fresh one allocated, so at most one
buffer is in flight while one is being filled. `Skipped` frames are logged
and dropped. Unfiltered full mode has no line cap because it never splits
lines. The reader is `BufReader::with_capacity(K, file)`. Peak: reader `K`
+ file buffer `K` + `acc` `L` + matcher scratch `2 L` + two output buffers
`2 L` = **5 L + 2 K**.

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
  when the object does not exist. `ArchiveObject { key: String, size: u64,
  version: Version }`: S3 via `head_object` (`size` = content length,
  `version` = the `ETag` when the endpoint returned one, ELSE its
  `Last-Modified`, ELSE neither — revision 8: some S3-compatible endpoints
  omit `ETag`, so `open` also carries `Last-Modified` and logs one `warn!`
  when the HEAD gave neither, since ranged reads of that object are then
  not version-pinned at all); local opens the file and keeps the descriptor
  (`size` = `metadata().len()` of THAT descriptor, `version` = the
  descriptor itself). A local publish replaces the path atomically
  (`:172-177`) and an S3 write replaces the key (`:280-289`), so the
  session, not the key alone, is what a read is consistent with.
- `async fn read_range(&self, obj: &ArchiveObject, offset: u64, len: u64,
  into: &mut Vec<u8>) -> Result<()>` — appends the bytes in `[offset,
  min(offset + len, size))` to `into`, NOTHING when `offset >= size`, never
  more than `len`. The caller passes a buffer preallocated to `len + 32`
  and cleared; the callee never reallocates it. S3 sends
  `get_object().key(obj.key).range("bytes=offset-(offset+len-1)")`, pinned
  by `If-Match` when the opened `ArchiveObject` carries an `ETag`, else by
  `If-Unmodified-Since` against its `Last-Modified` when that exists,
  else neither header is sent; a 412 from EITHER precondition (object
  replaced) is an archive error with the same message, a 416
  (unsatisfiable range) appends nothing; the body is read through the SDK's
  `into_async_read()` adapter (`aws-smithy-types` `rt-tokio`,
  `byte_stream.rs:434-450`, a `StreamReader` that holds one body chunk,
  `tokio-util/src/io/stream_reader.rs:274-285`) with
  `take(len).read_to_end(into)` — no `collect()`, so no `SegmentedBuf`, no
  segment descriptors and no contiguous copy. What is alive besides `into`
  is the SDK HTTP client's connection buffering plus that one retained
  chunk; the client speaks HTTP/1 or HTTP/2 (`blob_storage.rs:259-268`),
  so this is MODELLED as `H = 408 KiB` (hyper's HTTP/1 read-buffer cap) and
  MEASURED, not derived: the peak-allocation test's archive cases run in
  the S3 suite against a real MinIO through the real SDK client (§ 5).
  Local reads from the held descriptor at `offset` with `seek` + `take` +
  `read_to_end`, unaffected by a later replacement of the path.

`ArchiveRangeReader { archive, obj, pos, buf: Vec<u8>, consumed: usize }`
implements `AsyncBufRead` directly over ONE range buffer of `R + 32`:
`fill_buf` returns `buf[consumed..]`, and when that is empty it CLEARS
`buf` (capacity retained) and refills it with `read_range(pos, R)` — so a
finished range never coexists with the next one. `GzipDecoder` from
`async-compression` **0.4** (features `tokio`, `gzip`; the exact patch
version is what `Cargo.lock` resolves and is recorded in the implementation
commit) decodes it; its state `D` is a library constant the peak test
measures, taken as 64 KiB (the 32 KiB inflate window plus tables) in the
formulas. The decoder implements `AsyncRead`, so a
`BufReader::with_capacity(K, decoder)` sits between it and any line
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
  capacity `T` (two `Vec`s of `T` swapped, allocated once) with oldest-first
  eviction, no line splitter and no line cap, so a long line survives
  exactly as it does from the local file; at EOF the ring is cut at its
  first newline unless the ring holds the whole object; the torn-line rule
  applies (the archive is a `.jsonl` snapshot); `archive.truncated =
  decompressed > T || torn_line_dropped`. FILTERED: `LineSplitter` (§ 3.2,
  `max_line = L`) over the decoder's `BufReader`; each matching `Line` is
  appended to a ring of whole lines capped at `T` bytes with oldest-first
  eviction; a match whose OWN size exceeds the whole `T`-byte window
  (revision 8) empties the ring entirely rather than merely being skipped
  — the same outcome as the local scan, which stops scanning further back
  the moment a match can't fit, so the forward archive scan and the
  backward local scan agree on which older matches an oversize record
  evicts; `archive.truncated` is set when a line is evicted (including this
  whole-ring case), a `Skipped` frame is seen, or the torn-line rule fires.
  Both count decompressed bytes
  for `total_bytes`. Computed COMPLETELY before the response starts, so an
  archive error at any point (open, 412, download, gzip, footer) degrades to
  the local tail exactly as today (`:358-366`; `test at :1419-1425`).
- If both exist and `lines(local) + lines(archive) <= N`, the two tails
  (each `<= T`) go through `merge_jsonl_logs` and the result is cut from the
  front at a line boundary to `<= T`. `source: merged`. If the line cap is
  exceeded the local tail is served alone (`source: local`).
- `truncated = local.truncated || archive.truncated || merged_was_cut ||
  union_skipped_for_lines`. `total_bytes = local_len +
  archive_decompressed_count + 2` when both were consulted (§ 3.1).
- Cost: one archive stream per FETCH on a terminal job. The job page
  issues one fetch for the expanded step (`step-detail.tsx:69`) and one for
  `_server` (`server-events.tsx:21`), each once, since a terminal job is not
  polled — two archive streams per page view, plus one per further step the
  user expands.
- Peak (the merge phase dominates; the scan buffers are released before
  it): local result `T` + archive ring `T` + range buffer `R` + hyper `H` +
  decoder `D` + decoder reader `K` + `acc` `L` + matcher scratch `2 L` +
  merge overhead `M` (below) + merge output `2 T + 2` + cut result `T` +
  envelope `E` = **11 T + 3 L + R + H + D + K + M + 256**.

**The merger and its overhead `M`.** `merge_jsonl_logs` is unchanged except
for one line: its output is reserved as `a.len() + b.len() + 2` (today
`:50` reserves the sum alone, and `:52-53` appends a newline to every
retained line, so two unterminated inputs make the output exceed the
reservation by two bytes and reallocate). It reserves `(a.len() + b.len()) /
80 + 1` entries in a `HashSet<&str>` (24 B per slot) and a `Vec<&str>` (16
B per slot) (`:36-38`), grows them if more lines exist, and stable-sorts
the `Vec` (`:48`). With `I` the input bytes and `N'` the actual line count
(`<= N` by the gate), the formula allows a factor 2 on both collections for
`Vec` doubling and hashbrown's power-of-two bucket rounding, and a full-size
scratch for the sort (Rust's stable sort allocates one for slices below
its large-size threshold): `M = 2 × 40 × max(I / 80, N') + 16 N'`, i.e. `M
<= I + 96 N'`. Both terms are bounded: `I` by `2 T` (tail) or `C` (full),
`N'` by `N`. Whether the factor 2 is enough is exactly what the peak test
checks.

**Full, terminal, under the caps.** Gate: `local_len + isize <= C`, with
`local_len` the snapshot length. If it holds, local is read as exactly its
snapshot range — `take(local_len)` into a buffer of `local_len + 32`, the
same snapshot discipline as the tail, so a file that grows afterwards
neither overruns the buffer nor changes the gate — then the archive is
decompressed through `take(C - local_len + 1)` into a buffer of `C -
local_len + 33`; the two input buffers therefore sum to `C + 65`. If the
archive `take` fills (the trailer lied or wrapped, the object changed), the
merge is ABANDONED before any output and the single-source rule below
applies. Lines are counted while reading; if `lines > N` the merge is
likewise abandoned. Otherwise `merge_jsonl_logs` runs and the result is
streamed from that one string in `K` chunks. `source: merged`. This
preserves today's behaviour for every normal-sized log. Peak: inputs `<= C
+ 65` + `M` (`<= C + 96 N`) + output `<= C + 2` + range buffer `R` + HTTP
client `H` + decoder `D` + reader `K` = **3 C + 96 N + R + H + D + K**.

**Full, terminal, above a cap or archive size unknown.** ONE source is
streamed: the LOCAL file when it exists (§ 3.2 full modes), else the
archive through `ArchiveRangeReader` → `GzipDecoder` → `BufReader(K)` (→
`LineSplitter` when filtered) in `K` chunks, served exactly as stored,
filtered or not — an unterminated final record (a snapshot taken
mid-write) is kept, since there is only one source and nothing else could
ever complete it. Local first because a plain-file read fails far less
often mid-stream than a multi-request download and because it is the only
place the post-upload lines (§ 2) can be; either source can still error
mid-stream (`:393-409` propagates a mid-read I/O error), and when that
happens the body simply ends early — once headers are sent there is no
fallback and the header cannot be rewritten. Revision 8: the archive
branch is now PRIMED before the response is committed — a `BufReader`
wraps the decoder and one `fill_buf` runs the first range read, the gzip
header parse and the first inflate — so an archive error BEFORE THE FIRST
BYTE covers more than a missing object or an `open` failure: a bad first
range read (a 404 after a stale HEAD, a 412, a transient 5xx) or a bad
gzip header caught during priming falls back to local when present, else
answers an empty 200 with `source: none`, exactly like an `open` failure,
instead of committing a 200 and then aborting the body. `source: local` or
`archive`; the docs state that neither is guaranteed complete. Peak: local
as in § 3.2; archive **R + H + D + 4 K** unfiltered (the decoder's own
reader, the priming `BufReader`, and two `K` output buffers), **R + H + D
+ K + 5 L** filtered (the same `BufReader`, reused for priming, then
`acc`, scratch, two `L + 1` output buffers).

Non-terminal jobs never touch the archive (unchanged; the upload happens at
terminal time, so a pre-terminal archive read is a guaranteed miss).

### 3.4 Memory bounds

Per request, counting every buffer alive at the peak. What each symbol is
and how it is pinned: `T` — `take` and buffers preallocated to `T + 32`,
`<= tail_max_bytes`; `L` — the splitter's single accumulator, the carry
buffer and the output buffers, preallocated, and every filtered line
rejected above `L`; `K = 64 KiB` — `BufReader`, `ReaderStream` and
unfiltered output capacities; `R = 1 MiB` — the `len` of every
`read_range`, into a buffer of `R + 32`; `H = 408 KiB` — hyper's HTTP/1
read buffer at its default cap; `D = 64 KiB` — the pinned decoder's
inflate state (library constant); `C` — `take` on both merge inputs; `N` —
the line counter; `E = 6 T + 256` — the envelope `Vec`'s preallocated
capacity; `M <= I + 96 N'` — the merger's collections with a factor 2 for
growth and rounding, plus a full-size sort scratch. The peak-allocation
test (§ 5) asserts each mode's measured peak `<=` its formula × 1.5; the
1.5 is the allowance for allocator behaviour the formulas do not model
(per-allocation headers, bin rounding, transient duplicates during a
`Vec` move). Figures are CEILINGS to one decimal.

| Read | Peak | Default (`T` 256 KiB) | Max (`T` 4 MiB, `N'` = N) |
|---|---|---|---|
| Tail, unfiltered, local | `2 T + E` | 2.1 MiB | 32.1 MiB |
| MCP tail, formatted | tail formula + `2 T` = `4 T + E` (revision 8) | 2.6 MiB | 40.1 MiB |
| Tail, step, local | `3 T + 3 L + E` | 5.3 MiB | 39.1 MiB |
| Tail, terminal, merged | `11 T + 3 L + R + H + D + K + M + 256`, `M <= 2 T + 96 N'` | 7.8 MiB + `96 N'` (≤ 12 MiB adversarial; ~0.2 MiB typical) → 19.8 MiB | 56.6 MiB + 12 MiB → 68.6 MiB |
| Full, unfiltered, local | `2 K` | 0.2 MiB | 0.2 MiB |
| Full, filtered, local | `5 L + 2 K` | 5.2 MiB | 5.2 MiB |
| Full, archive, single source | `R + H + D + 4 K` (revision 8; `R + H + D + K + 5 L` filtered, unchanged) | 1.8 MiB (6.6 MiB) | same |
| Full, terminal, merged | `3 C + 96 N + R + H + D + K` | 61.6 MiB | 61.6 MiB |

With defaults, a UI poll costs at most 5.3 MiB and the most expensive read
any client can trigger costs about 68.6 MiB by formula (a 4 MiB terminal
tail over adversarial short distinct lines), i.e. up to ~103 MiB at the
test's × 1.5 allowance — which is why `tail_max_bytes` is configurable and
why the production limit bump (256 Mi/512 Mi → 1 Gi/2 Gi, already in the
helmfile) is due before this ships; on today's ~100 MiB of headroom the
operator should set `tail_max_bytes` to 1 MiB. The "typical" figure is not
a bound and is not relied on.

### 3.5 Configuration

Under `log_storage:` (`crates/stroem-server/src/config.rs:51`), all
optional with defaults; `ServerConfig::validate` rejects any zero,
`tail_default_bytes > tail_max_bytes`, `tail_max_bytes >
tail_scan_max_bytes`, and `max_line_bytes > merge_max_bytes`:

| Key | Default | Meaning |
|---|---|---|
| `read.tail_default_bytes` | 262144 (256 KiB) | tail size when the request names none |
| `read.tail_max_bytes` | 4194304 (4 MiB) | largest `tail_bytes` a request may ask for |
| `read.tail_scan_max_bytes` | 67108864 (64 MiB) | how far back a step tail scans before giving up |
| `read.max_line_bytes` | 1048576 (1 MiB) | longest single line a FILTERED read will carry; longer lines are skipped with a warning and set `truncated` |
| `read.merge_max_bytes` | 16777216 (16 MiB) | sum of local length + archive decompressed length under which a terminal full read still merges in memory |
| `read.merge_max_lines` | 131072 | most lines a union merge (tail or full) will hold; above it a single source is served |

Env overrides follow the existing convention
(`STROEM__LOG_STORAGE__READ__TAIL_DEFAULT_BYTES`).

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
body from a cold replica still never clears the view. One guard changes:
`server-events.tsx:56` returns `null` whenever `logs` is empty, which would
hide the banner for a finished job whose `_server` tail is empty because
its only record was torn (§ 3.2) — the card's visibility becomes `logs ||
truncated`, and an empty-but-truncated tail renders the banner with its
two actions and no lines. The step panel already renders for every
non-skipped step, so it needs no such change.

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
     newest copies, which is where the tail overlaps. Kept lines keep their
     original relative order.
  3. `lines = kept.concat(tail)`.
  4. `gap = displayed.length > 0 && tail.length > 0 && nothing was
     removed` — no tail line had been displayed, so nothing ties the tail
     to the view; the viewer inserts a visible "gap" divider row.
  Properties: idempotent (re-applying the same tail removes and re-adds
  exactly its lines); no cross-duplication under arrival-ordered tails
  (`[B, A, C]` + the same tail → `[B, A, C]`); a line another replica
  recovered in the middle is shown (`[A, C]` + `[A, B, C]` → `[A, B, C]`);
  a stale mirrored straggler in the tail is appended once, not used to cut
  history; repeated lines keep their OBSERVED multiplicity (`[X, X]` + `[X]`
  → `[X, X]`; `[A]` + `[B, B]` → `[A, B, B]`). It is deliberately NOT the
  server merge's collapse of identical lines. Its limit: a NEW
  byte-identical event that arrives entirely inside a window the previous
  poll already covered is indistinguishable from the unchanged overlap and
  is not added — the view shows the multiplicity it has observed across
  polls, not necessarily every occurrence; "Load full log" again, or the
  download, shows what the SOURCE holds, which for a running job is the
  file as written and for a finished job under the merge cap is the
  merger's union, in which identical lines collapse to one (§ 3.3). Ordering is "everything not in the tail, then
  the tail", which for a running step means older content then the newest
  window; a straggler can sit out of time order, as it does in the file.
  Cost per poll is `O(|displayed| + |tail|)` map lookups (~475 k for the
  incident log), tens of milliseconds; if that ever matters the walk can
  stop once every tail count is zero, not done now. `appendTail` lives in
  `ui/src/lib/log-tail.ts` with Vitest coverage (§ 5).

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
- Operators on the current 512 Mi limit should set `tail_max_bytes: 1048576`
  until the limit bump is applied (§ 3.4).

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
   over missing lines; two bounded tails through the merge keep the HA
   recovery the current tests pin, at one archive stream per fetch.
7. **Exact merge gate from the gzip trailer, guarded by `take` on BOTH
   inputs, trailer never reported** (revisions 2–4).
8. **Above the caps, local first, archive last** (revision 2). A streaming
   response cannot fall back after its headers; the local file is the source
   least likely to fail mid-stream and the only one holding post-upload
   lines. Neither source is complete; the header names which answered.
9. **Keep the in-memory merge under the caps.** Removing it would silently
   drop the HA-recovery behaviour for the common case where it costs a
   bounded, stated amount; its reservation, growth and sort scratch are in
   the formula and the test.
10. **A line cap beside the byte cap** (revision 3). The merge's per-line
    entries do not depend on line length; a byte cap alone lets one-byte
    lines multiply them by 40.
11. **A lending splitter on `fill_buf`/`consume`, not `FramedRead` and not
    an owned-item stream** (revisions 2–4), driven through `unfold` so its
    state survives `Pending`.
12. **Versioned range reads, not the SDK stream, for the archive**
    (revisions 3–5), read through the async adapter into a preallocated
    buffer, never `collect()`ed.
13. **A hand-written `step` visitor** (revisions 3–5) that keeps today's
    semantics — object root, last key wins, escaped keys, trailing garbage
    rejected — with one documented and tested divergence on invalid
    surrogate escapes in unrelated fields.
14. **Virtualised viewer that can load the whole log** (the user's choice
    over "page backwards" and "download only"), TanStack Virtual, no Pretext.
15. **`appendTail` is a multiset difference from the end, then append**
    (revisions 3–5); it preserves observed multiplicity and says so.
16. **Torn lines are dropped, and flagged, from every `.jsonl` file**
    (revision 5): every writer of that format terminates its lines, so an
    unterminated suffix is always a write in progress, even on a finished
    job. Legacy `.log` files keep theirs.
17. **Unfiltered reads never apply the line cap** (revision 4), from either
    source; `L` belongs to filtered reads, where the matcher needs a whole
    line in memory.
18. **Bounds are measured, not argued** (revision 5). The formulas state the
    intent with explicit factors; a counting allocator in the test binary
    is the enforcement. A future library that allocates differently fails
    the test, which is the signal we want.
19. **Remove the String-returning read functions** so the unbounded path
    cannot be reintroduced by a future caller; every existing call site,
    including the nine in integration tests, moves to the bounded API.

## 5. Testing

**Peak-allocation test** (the regression test this incident needs;
`crates/stroem-server/tests/log_peak_alloc_test.rs`, its own binary so it
can install a `#[global_allocator]`).

*What it measures.* A wrapper around `System` that tracks bytes currently
allocated and the high-water mark in two atomics, counting the REQUESTED
sizes of `alloc`, `realloc` and `dealloc` (a `realloc` counts the new size
and releases the old; the transient copy inside `System::realloc` is not
observable and is one of the things the × 1.5 allowance covers). It
measures Rust-heap allocations made through the global allocator by the
code under test: not allocator metadata or fragmentation, not stack, not
memory mapped outside `GlobalAlloc`, not the process RSS. It is a
regression detector for the paths it exercises, not a proof for arbitrary
inputs; that is what goal 1 says.

*Measurement policy.* One request at a time: the binary runs its cases
serially (a global mutex, and `harness = false` with a hand-rolled main so
no test-thread pool exists). Fixtures live on DISK — JSONL files under a
temp dir, archive objects in a `LocalBlobArchive` on disk (never the
in-memory mock, which clones its bytes on every read,
`log_storage.rs:615`, `:628`) — so no fixture bytes sit on the heap. For
each case: build the request, record `baseline = current()`, `reset_peak()`,
then run the WHOLE server-side path — for a tail, `read_tail` followed by
the envelope serialisation exactly as the handler does it; for a full
read, `stream_full` followed by draining the returned stream to completion,
dropping each chunk as it arrives — and assert `peak() - baseline <=
formula(T, L, K, R, H, D, C, N') × 1.5`, with `N'` the fixture's actual
line count. The same case asserts, after every preallocated read, that the
buffer's `capacity()` is unchanged. The archive cases also run against a
real S3-compatible store through the real SDK client in
`s3_integration_test.rs` (MinIO via testcontainers), which is the only
place the `H` term is measured rather than modelled.

*Fixtures*, each chosen to reach the term it exercises: (a) an
incident-shaped JSONL file of ~475 k realistic lines (~83 MiB), local and
as a gzipped archive object; (b) DISTINCT short lines — `{"step":"s","line":"<n>"}`
with a counter — up to the line cap, so the merger holds near `N` distinct
entries (blank or repeated one-byte lines would be dropped or collapsed by
`merge_jsonl_logs`, `:40-44`, and exercise nothing); (c) `\u0001`-only line
content (exercises `E`); (d) a MATCHING line whose total length is `L - 64`
bytes, consisting of a literal run followed by ONE escape sequence at the
end, so the line passes the `<= L` filter and serde_json's scratch has to
grow to hold the decoded prefix (exercises `~L`, not `2 L`: matching this
fixture's design needs the decoded step name to also appear literally
elsewhere in the line — to pass the `contains` fast guard on an escaped
value — which halves the room left for the escaped value itself), plus a
companion NON-matching line (revision 8) whose `contains` guard passes
because the target appears in `line` instead, freeing nearly the whole
line for one escaped string and so reaching the full `2 L`; (e) a 15 MiB
single line under
`N = 1` (exercises the merger's byte-based reservation); (f) for newline
normalisation, two inputs whose last lines lack a newline, merged through
the FULL-merge path (the tail path's torn-line rule would drop them first),
asserting the merger's output did not reallocate and `returned <= total`.
It runs in CI on fixtures sized to the defaults (`C` = 16 MiB, so the
merged-full case uses a 12 MiB local + 4 MiB archive pair) and has an
`#[ignore]`d 100 MiB variant for the release checklist. It is also what
measures `D`.

Unit (`log_storage.rs`):
- tail: cut at a line boundary; larger than the file → whole file,
  `truncated: false`; empty file; a single line longer than the window →
  empty + truncated; a file that grows after the snapshot returns exactly
  the snapshot range (append from a second task between `metadata` and the
  read; assert `returned_bytes <= total_bytes`); the torn-line matrix — a
  `.jsonl` unterminated last line dropped and `truncated` set, live AND
  terminal, and returned whole once the newline arrives; a legacy `.log`
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
  keys, last wins; a compound first `step` value followed by the real one
  matches; non-string `step` does not match; escaped step VALUE does not
  match (the `contains` fast guard, as today); with the name also present
  literally elsewhere in the line, it matches; escaped
  step KEY (the key `step` with its `e` written as a JSON unicode escape)
  matches; an unrelated key written as a JSON unicode escape does not
  break the match; trailing
  garbage after the object does not match; `step` absent; nested object
  with an inner `step` does not match; and the DOCUMENTED DIVERGENCE CLASS
  pinned as three named tests, each asserting the NEW behaviour and
  carrying a comment that the old matcher rejected the line: an invalid
  surrogate escape in an unrelated string, a 129-level nested array in an
  unrelated value, and `1e400` in an unrelated value.
- terminal tail: local + archive → merged content equals the union of the
  two tails, cut to `T`, `source: merged`, `total_bytes = local_len +
  counted archive bytes + 2` (the newline-normalisation case itself is
  exercised through the full-merge path, § 5 fixture (f), because the tail
  path drops an unterminated last line before merging); archive error →
  local tail, `source: local`;
  local missing → archive ring with correct `total_bytes`; step tail from
  the archive; line cap exceeded → local only, `truncated`, `source:
  local`; UNFILTERED archive tail with `T` = 4 MiB keeps a whole 2 MiB
  line; FILTERED archive tail skips it and sets `truncated`; an evicted
  matching line sets `truncated`.
- `LineSplitter`: oversize line yields `Skipped` and the FOLLOWING line is
  delivered; the stream never ends early; `acc.capacity()` equals
  `max_line` before and after an oversize line and after 10 000 lines;
  lines split across `fill_buf` boundaries; final line without newline;
  empty input; the `unfold` body stream survives a `Pending` in the middle
  of a line (a reader that returns `Pending` every other poll) without
  losing bytes.
- `ArchiveRangeReader` / `open` / `read_range`: object shorter than `R`;
  object an exact multiple of `R`; `read_range` past the end appends
  nothing; the range buffer's capacity is unchanged across ranges and the
  previous range is not alive during the next fetch (asserted with the
  counting allocator); `open` of a missing key is `None`; on the local
  backend, the path replaced mid-read still serves the opened version; on
  the in-memory test archive, a version bump mid-read is an archive error
  (the S3 412 path is covered in the S3 suite).
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
  `read_range` (including an unsatisfiable range → nothing appended), a 412
  when the object is overwritten between `open` and `read_range`, and a
  real gzipped object streamed into a tail through range reads.

CLI: `--full` handles NDJSON and the JSON envelope; the truncation notice
goes to stderr; `--tail-bytes` is forwarded.

UI (Vitest): `appendTail` — plain overlap; the same tail applied twice is a
no-op; the arrival-ordered counterexample `[B, A, C]` does not duplicate;
recovered middle line shown; straggler appended once; gap only when nothing
was removed; `[X, X]` + `[X]` → `[X, X]`; `[A]` + `[B, B]` → `[A, B, B]`;
`[A, A]` + `[B]` → `[A, A, B]`; kept lines keep their order; empty inputs.
Banner rendered when `truncated`, absent otherwise; a `server-events` test
that an empty body with `truncated: true` renders the card with the banner
and no lines, and that an empty body with `truncated: false` still renders
nothing; confirmation above 64 MiB; `step-detail.test.tsx` updated for the
envelope; a `log-viewer`
test that a 10 000-line body renders fewer than 200 rows (virtualisation is
in effect; jsdom needs the size mock the TanStack docs describe).
Playwright (`ui/e2e/log-streaming.spec.ts`): banner appears for a log
above the default tail and "Load full log" renders the first line.

## 6. Documentation

- `docs/src/content/docs/reference/api.md`: query parameters, the three new
  fields (with `total_bytes` as an upper bound), NDJSON mode, the source
  header, WS backfill note.
- `docs/src/content/docs/operations/log-storage.md`: the six keys and the
  `tail_max_bytes` advice for 512 Mi deployments, the new read order and
  source rules, the header; the `curl … | jq -r .logs` recipe stays for the
  tail, and a SEPARATE `curl '…?full=true'` recipe with no `jq` is added for
  the raw NDJSON stream (the envelope is gone in that mode, so `.logs`
  would be `null`).
- CLI help text; `docs/src/content/docs/guides/mcp.md` for `tail_bytes`.
- `CLAUDE.md` § Log Storage / § WebSocket Log Streaming: read modes,
  bounds and the peak test; correct two stale facts found on the way — the
  NOTIFY segment cap is 3 500 bytes (`events.rs:43-57`), not 7 000, and
  `ui/src/hooks/use-job-logs.ts` no longer exists.
- `CONTEXT.md`: **Tail read**, **Full read**, **Log source**.
- `docs/internal/TODO.md`: mark the incident entry done; add the WS
  backfill-to-live gap and the post-upload lines as open items next to the
  existing non-goals.
- Release notes: behaviour change for scripts, old CLIs and WS consumers;
  the `tail_max_bytes` advice.

## 7. Risks and open points

- **The bounds are measured, not proven.** The formulas model the buffers
  the code controls and carry factors for the ones it does not; the peak
  test is the enforcement, and a library upgrade that changes an internal
  allocation shows up as a test failure, not as a production OOM. That is
  the intended trade after four review rounds established that
  allocator-level arithmetic cannot be settled in prose.
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
- **`total_bytes` is an upper bound of the job log, not the step's share.**
  The banner says "of up to".
- **Adversarial maxima.** A 4 MiB terminal tail over short distinct lines
  is ~68.6 MiB by formula, ~103 MiB at the test allowance. Bounded and
  stated; set `tail_max_bytes` to 1 MiB on a 512 Mi deployment. The
  deploy-side limit bump is the real fix for headroom and is the user's.
- **The matcher's divergence class** (malformed content in fields it
  ignores) has three known members, each pinned by a named test so that a
  future reader knows it was a decision; a newly found member joins the
  list rather than silently changing behaviour.
