# Log Reads — Tail by Default, Streamed Full Log — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** No log read, from any client, can allocate more than a small stated multiple of the configured caps: every log endpoint serves a bounded tail by default and streams the full log on request, and the UI shows a virtualised tail with "Load full log" and "Download".

**Architecture:** A new `crate::log_read` module owns every read primitive — the step matcher, a lending line splitter, local tail/stream readers, archive range reads with gzip decoding, and the bounded merge. `LogStorage` gains two entry points (`read_tail`, `stream_full`) that choose local, archive or merged sources by the spec's rules; the String-returning `get_log`/`get_step_log` are removed so the unbounded path cannot come back. A counting-allocator test binary is the enforcement for the memory bounds.

**Tech Stack:** Rust (tokio 1.52, tokio-util 0.7 `io`, axum 0.8, serde_json 1.0.150, new `async-compression` 0.4 with `tokio`+`gzip`, aws-sdk-s3 `rt-tokio`), React 19 + TypeScript + new `@tanstack/react-virtual` ^3.14, Vitest, Playwright.

**Spec:** `docs/superpowers/specs/2026-09-22-log-tail-streaming-design.md` — revision 6, reviewed over five Codex rounds (thread `01a0c805-be6b-7053-8be2-52c6bb019745`). Task 1 amends it to revision 7 with the planning decisions listed below. Read §§ 2, 3.1–3.6 and 5 before starting any task.

## Global Constraints

- Defaults, copied from spec § 3.5: `tail_default_bytes` 262144 (256 KiB), `tail_max_bytes` 4194304 (4 MiB), `tail_scan_max_bytes` 67108864 (64 MiB), `max_line_bytes` 1048576 (1 MiB), `merge_max_bytes` 16777216 (16 MiB), `merge_max_lines` 131072.
- Code constants: `CHUNK` (K) = 64 KiB, `RANGE` (R) = 1 MiB, `READ_SLACK` = 32 bytes.
- Query parameters `tail_bytes` and `full` are mutually exclusive; `tail_bytes` of `0`, above `tail_max_bytes`, or non-integer → 400; `full` accepts only `true`/`false`.
- Header on every REST log response: `X-Stroem-Log-Source: local | archive | merged | none`.
- Full mode content type: `application/x-ndjson; charset=utf-8`, no JSON envelope.
- Tail envelope: `{"logs", "truncated", "total_bytes", "returned_bytes"}`; `logs` is whole lines only and at most `tail_bytes` long; `total_bytes` is an upper bound and `returned_bytes <= total_bytes` always.
- Preserve: `.jsonl` → legacy `.log` → (terminal only) archive; `NotFound` never a 500; archive errors degrade to local while a fallback is possible; `_server` is a pseudo-step; the UI's "keep last non-empty body" rule.
- Error handling `anyhow::Result` + `.context(..)`; logging `tracing`; no whole request structs in `#[instrument]` spans.
- Every task ends green on `cargo fmt --check --all` and `cargo clippy --workspace --all-targets -- -D warnings` plus the task's own tests. UI tasks also end green on `cd ui && bun run lint && bunx tsc -b && bun run test`.
- Commits: conventional prefixes (`feat(logs):`, `fix(logs):`, `test(logs):`, `docs(logs):`), **no AI co-author trailer** — the user's global CLAUDE.md forbids it and overrides any harness attribution reminder.
- The branch `feat/log-tail-streaming` is already rebased onto local `main` (`15a1b9d`, 26 commits ahead of `origin/main`, containing the workspace-scale merge). Do not rebase onto `origin/main`.
- A fresh worktree needs `mkdir -p crates/stroem-server/static` before the server crate builds (rust-embed; the directory is gitignored).
- Integration tests need Docker (testcontainers Postgres and MinIO).

## Spec amendments decided while planning (applied to the spec in Task 1)

1. **Config keys are nested under `log_storage.read`** (`STROEM__LOG_STORAGE__READ__TAIL_MAX_BYTES`), not flat under `log_storage`. `LogStorageConfig` is built by struct literal in about 80 places; one nested field is one line per literal (the `workspace_reload` precedent), and `#[serde(flatten)]` would break the `config` crate's env-string coercion (the same buffering problem CLAUDE.md documents for `lenient_bool`).
2. **An escaped step VALUE does not match**, exactly as today: `line_matches_step` keeps its `line.contains(step_name)` fast guard, and `"build"` does not contain `build`. Spec § 5's fixture list said it matches; the corrected expectation is "no match, same as the old matcher", plus a companion case where the name also appears literally elsewhere in the line, which does match.
3. **Filtered stream output buffers are `K + L + 1` bytes**, and a chunk is yielded once it reaches `K`, so a matching line (≤ L) always fits without the splitter having to hold a line back. Filtered-full local peak becomes `5L + 4K`; unfiltered-full local adds the backward newline scan buffer: `3K`.
4. **Full reads obey the torn-line rule for local `.jsonl` files**: the stream stops at the last newline of the snapshot. The merged full read trims both inputs. The single-source archive stream is served exactly as stored.
5. **The merged full read decompresses the archive through `take(isize + 1)`** into a buffer of `isize + 33`; any size other than exactly `isize` abandons the merge before output. This bounds the allocation by the real input instead of by `C − local_len`.
6. **The viewer parses only the rows it renders** (virtualised), not the whole body up front; the row-height estimate uses the raw line length minus a fixed JSON overhead.
7. **No in-memory "version bump mid-read" test.** The trait's default `open` holds a whole-object snapshot, which is consistent by construction; the local backend (held descriptor) and S3 (`If-Match` → 412) carry the version tests.
8. **`appendTail` inserts the gap marker into its returned `lines`** itself.
9. **Peak-test fixtures:** (e) runs with `merge_max_lines = 2` so the one-line inputs still merge; (f) is covered by a unit test on `merge_jsonl_logs`' capacity; the S3 case lives in the peak binary, which runs serially, instead of `s3_integration_test.rs`, whose parallel tests would pollute a global counter.
10. **The unfiltered tail reads one byte before its window** to decide whether the window's first line is complete, instead of always dropping through the first newline.

## File Structure

| File | Responsibility |
|---|---|
| `crates/stroem-server/src/log_read/mod.rs` (new) | Constants, `StepFilter`, `LogSource`, `Tail`, envelope serialisation, small byte helpers |
| `crates/stroem-server/src/log_read/matcher.rs` (new) | `line_matches_step` — hand-written serde visitor |
| `crates/stroem-server/src/log_read/splitter.rs` (new) | Lending `LineSplitter`, `filtered_stream` |
| `crates/stroem-server/src/log_read/local.rs` (new) | Local file resolution, unfiltered and step tails, full streams |
| `crates/stroem-server/src/log_read/archive.rs` (new) | `ArchiveRangeReader`, gzip `ISIZE`, archive tails, archive stream, bounded merge |
| `crates/stroem-server/src/log_storage.rs` | `read_tail`, `stream_full`, `combine_tails`; old readers removed; merge reservation fix |
| `crates/stroem-server/src/blob_storage.rs` | `ArchiveObject`, `ArchiveVersion`, `BlobArchive::{open, read_range}` |
| `crates/stroem-server/src/config.rs` | `LogReadConfig`, validation |
| `crates/stroem-server/src/web/api/logs.rs` (new) | The two REST log handlers, query parsing, tail and full responses |
| `crates/stroem-server/src/web/api/ws.rs`, `mcp/tools.rs` | Tail backfill; MCP `tail_bytes` + truncation trailer |
| `crates/stroem-server/tests/log_peak_alloc_test.rs` (new) | Counting-allocator peak test, `harness = false` |
| `crates/stroem-cli/src/remote/logs.rs`, `remote/mod.rs` | `--full`, `--tail-bytes`, truncation note |
| `ui/src/lib/log-lines.ts`, `log-tail.ts`, `download.ts` (new) | Line helpers, `appendTail`, blob download |
| `ui/src/lib/api.ts` | `LogTail`, `getStepLogs`, `getStepLogsFull`, `downloadStepLog` |
| `ui/src/hooks/use-step-log.ts` (new) | Polling, keep-last-non-empty, full mode, stitching |
| `ui/src/components/log-viewer.tsx`, `log-tail-banner.tsx` (new) | Virtualised viewer, banner |
| `ui/src/components/step-detail.tsx`, `server-events.tsx` | Wire the hook |
| `workspace/.workflows/big-log.yaml` (new), `ui/e2e/log-streaming.spec.ts`, `tests/e2e.sh` | End-to-end coverage |

---

### Task 1: `LogReadConfig`, wiring, and spec revision 7

**Files:**
- Modify: `crates/stroem-server/src/config.rs` (struct next to `LogStorageConfig` at `:49-57`; `ServerConfig::validate` at `:538`; tests module)
- Modify: `crates/stroem-server/src/log_storage.rs:99-129` (field + builder)
- Modify: `crates/stroem-server/src/main.rs:213-220`
- Modify: every `LogStorageConfig { .. }` literal (≈80, scripted)
- Modify: `docs/superpowers/specs/2026-09-22-log-tail-streaming-design.md`

**Interfaces:**
- Produces: `pub struct LogReadConfig { pub tail_default_bytes: u64, pub tail_max_bytes: u64, pub tail_scan_max_bytes: u64, pub max_line_bytes: usize, pub merge_max_bytes: u64, pub merge_max_lines: usize }` (`Copy`, `Default`, `validate()`); field `LogStorageConfig::read: LogReadConfig`; `LogStorage::with_read_config(self, LogReadConfig) -> Self`; `LogStorage::read_config(&self) -> LogReadConfig`.

- [ ] **Step 1: Write the failing config tests**

Append to the `#[cfg(test)] mod tests` in `crates/stroem-server/src/config.rs`:

```rust
    #[test]
    fn log_read_defaults_when_absent() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://x"
log_storage:
  local_dir: /tmp/logs
worker_token: "0123456789abcdef0123456789abcdef"
"#;
        let cfg: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        let r = cfg.log_storage.read;
        assert_eq!(r.tail_default_bytes, 262_144);
        assert_eq!(r.tail_max_bytes, 4_194_304);
        assert_eq!(r.tail_scan_max_bytes, 67_108_864);
        assert_eq!(r.max_line_bytes, 1_048_576);
        assert_eq!(r.merge_max_bytes, 16_777_216);
        assert_eq!(r.merge_max_lines, 131_072);
        cfg.validate().unwrap();
    }

    #[test]
    fn log_read_overrides_parse_and_unknown_keys_are_rejected() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://x"
log_storage:
  local_dir: /tmp/logs
  read:
    tail_max_bytes: 1048576
worker_token: "0123456789abcdef0123456789abcdef"
"#;
        let cfg: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.log_storage.read.tail_max_bytes, 1_048_576);
        assert_eq!(cfg.log_storage.read.tail_default_bytes, 262_144);
        cfg.validate().unwrap();

        let bad = yaml.replace("tail_max_bytes", "tail_maxx_bytes");
        assert!(serde_yaml::from_str::<ServerConfig>(&bad).is_err());
    }

    #[test]
    fn log_read_validation_rejects_zero_and_inverted_limits() {
        let base = minimal_config();
        for mutate in [
            (|c: &mut ServerConfig| c.log_storage.read.tail_default_bytes = 0)
                as fn(&mut ServerConfig),
            |c| c.log_storage.read.tail_max_bytes = 0,
            |c| c.log_storage.read.tail_scan_max_bytes = 0,
            |c| c.log_storage.read.max_line_bytes = 0,
            |c| c.log_storage.read.merge_max_bytes = 0,
            |c| c.log_storage.read.merge_max_lines = 0,
            |c| c.log_storage.read.tail_default_bytes = c.log_storage.read.tail_max_bytes + 1,
            |c| c.log_storage.read.tail_max_bytes = c.log_storage.read.tail_scan_max_bytes + 1,
            |c| c.log_storage.read.max_line_bytes = c.log_storage.read.merge_max_bytes as usize + 1,
        ] {
            let mut c = base.clone();
            mutate(&mut c);
            let err = c.validate().unwrap_err().to_string();
            assert!(err.contains("log_storage.read"), "unexpected error: {err}");
        }
    }

    #[test]
    fn log_read_env_override_coerces_strings() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://placeholder:5432/stroem"
log_storage:
  local_dir: "./logs"
worker_token: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, yaml.as_bytes()).unwrap();
        std::io::Write::flush(&mut file).unwrap();
        // SAFETY: test-only, serialized by ENV_MUTEX
        unsafe {
            std::env::set_var("STROEM__LOG_STORAGE__READ__TAIL_MAX_BYTES", "1048576");
        }
        let config = load_config(file.path().to_str().unwrap());
        unsafe {
            std::env::remove_var("STROEM__LOG_STORAGE__READ__TAIL_MAX_BYTES");
        }
        assert_eq!(config.unwrap().log_storage.read.tail_max_bytes, 1_048_576);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p stroem-server --lib config::tests::log_read`
Expected: compile error — `no field read on type LogStorageConfig`.

- [ ] **Step 3: Add `LogReadConfig` and the field**

In `crates/stroem-server/src/config.rs`, add the field to `LogStorageConfig` (after `archive`):

```rust
    /// Bounds on log reads (tail sizes, line and merge caps).
    #[serde(default)]
    pub read: LogReadConfig,
```

and add the type directly below `impl LogStorageConfig`:

```rust
/// Bounds on log reads — `docs/superpowers/specs/2026-09-22-log-tail-streaming-design.md` § 3.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogReadConfig {
    /// Tail size when a request names none.
    pub tail_default_bytes: u64,
    /// Largest `tail_bytes` a request may ask for.
    pub tail_max_bytes: u64,
    /// How far back a step tail scans before giving up.
    pub tail_scan_max_bytes: u64,
    /// Longest single line a filtered read carries; longer lines are skipped.
    pub max_line_bytes: usize,
    /// Local length + archive decompressed length under which a terminal
    /// full read still merges the two sources in memory.
    pub merge_max_bytes: u64,
    /// Most lines a union merge (tail or full) will hold.
    pub merge_max_lines: usize,
}

impl Default for LogReadConfig {
    fn default() -> Self {
        Self {
            tail_default_bytes: 256 * 1024,
            tail_max_bytes: 4 * 1024 * 1024,
            tail_scan_max_bytes: 64 * 1024 * 1024,
            max_line_bytes: 1024 * 1024,
            merge_max_bytes: 16 * 1024 * 1024,
            merge_max_lines: 131_072,
        }
    }
}

impl LogReadConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        for (name, value) in [
            ("tail_default_bytes", self.tail_default_bytes),
            ("tail_max_bytes", self.tail_max_bytes),
            ("tail_scan_max_bytes", self.tail_scan_max_bytes),
            ("max_line_bytes", self.max_line_bytes as u64),
            ("merge_max_bytes", self.merge_max_bytes),
            ("merge_max_lines", self.merge_max_lines as u64),
        ] {
            if value == 0 {
                anyhow::bail!("log_storage.read.{name} must be greater than 0");
            }
        }
        if self.tail_default_bytes > self.tail_max_bytes {
            anyhow::bail!(
                "log_storage.read.tail_default_bytes ({}) must not exceed tail_max_bytes ({})",
                self.tail_default_bytes,
                self.tail_max_bytes
            );
        }
        if self.tail_max_bytes > self.tail_scan_max_bytes {
            anyhow::bail!(
                "log_storage.read.tail_max_bytes ({}) must not exceed tail_scan_max_bytes ({})",
                self.tail_max_bytes,
                self.tail_scan_max_bytes
            );
        }
        if self.max_line_bytes as u64 > self.merge_max_bytes {
            anyhow::bail!(
                "log_storage.read.max_line_bytes ({}) must not exceed merge_max_bytes ({})",
                self.max_line_bytes,
                self.merge_max_bytes
            );
        }
        Ok(())
    }
}
```

In `ServerConfig::validate`, directly after the `recovery` checks, add:

```rust
        self.log_storage.read.validate()?;
```

- [ ] **Step 4: Add `read: Default::default()` to every struct literal**

Save as `/tmp/add_read_field.py` (or the scratchpad) and run it from the worktree root:

```python
import pathlib, sys

def patch(text):
    out, i, changed = [], 0, False
    needle = "LogStorageConfig {"
    while True:
        j = text.find(needle, i)
        if j < 0:
            out.append(text[i:])
            return "".join(out), changed
        k = j + len(needle)
        depth, m = 1, k
        while depth:
            c = text[m]
            depth += (c == "{") - (c == "}")
            m += 1
        body = text[k:m - 1]
        # Skip the struct definition, the impl block, and literals already patched.
        if "pub local_dir" in body or "fn " in body or "read:" in body:
            out.append(text[i:m]); i = m; continue
        line_start = text.rfind("\n", 0, m - 1) + 1
        indent = text[line_start:m - 1]
        if indent.strip():
            sys.exit(f"single-line LogStorageConfig literal, patch by hand: {text[j:m][:80]!r}")
        out.append(text[i:line_start] + indent + "    read: Default::default(),\n" + text[line_start:m])
        i, changed = m, True

for path in pathlib.Path("crates").rglob("*.rs"):
    src = path.read_text()
    if "LogStorageConfig {" not in src:
        continue
    new, changed = patch(src)
    if changed:
        path.write_text(new)
        print("patched", path)
```

Run: `python3 /tmp/add_read_field.py`
Expected: about 30 files printed, including `crates/stroem-server/tests/integration_test.rs`, `crates/stroem-server/src/config.rs` and `crates/stroem-e2e/tests/harness.rs`.

- [ ] **Step 5: Give `LogStorage` the read config**

In `crates/stroem-server/src/log_storage.rs`, add the import `use crate::config::LogReadConfig;`, the field `read: LogReadConfig,` to `struct LogStorage`, `read: LogReadConfig::default(),` in `LogStorage::new`, and these methods after `with_archive`:

```rust
    /// Use `read` for this storage's read bounds (defaults otherwise).
    pub fn with_read_config(mut self, read: LogReadConfig) -> Self {
        self.read = read;
        self
    }

    /// The read bounds this storage enforces.
    pub fn read_config(&self) -> LogReadConfig {
        self.read
    }
```

In `crates/stroem-server/src/main.rs:214`, change the base construction to:

```rust
        let base = LogStorage::new(&config.log_storage.local_dir)
            .with_read_config(config.log_storage.read);
```

- [ ] **Step 6: Run the tests and build everything**

Run: `mkdir -p crates/stroem-server/static && cargo test -p stroem-server --lib config::tests::log_read && cargo build --workspace --all-targets`
Expected: 4 tests PASS; the whole workspace, tests included, builds.

- [ ] **Step 7: Amend the spec to revision 7**

In `docs/superpowers/specs/2026-09-22-log-tail-streaming-design.md`: change the status line to `Status: revision 7, proposed (2026-09-23)`; insert above "**Revision 6 (2026-09-22).**" a paragraph starting `**Revision 7 (2026-09-23, planning).**` that lists the ten amendments of this plan's "Spec amendments" section, one sentence each, and ends with "Where the sections below differ, this paragraph wins."; in § 3.5 replace the table's key column values with `read.tail_default_bytes`, `read.tail_max_bytes`, `read.tail_scan_max_bytes`, `read.max_line_bytes`, `read.merge_max_bytes`, `read.merge_max_lines`, and the env example with `STROEM__LOG_STORAGE__READ__TAIL_DEFAULT_BYTES`; in § 5's matcher fixtures replace "escaped step VALUE … matches `build`" with "escaped step VALUE does not match (the `contains` fast guard, as today); with the name also present literally elsewhere in the line, it matches".

- [ ] **Step 8: Commit**

```bash
git add -A crates docs/superpowers/specs/2026-09-22-log-tail-streaming-design.md
git commit -m "feat(logs): log_storage.read config with validation; spec revision 7"
```

---

### Task 2: The step matcher

**Files:**
- Create: `crates/stroem-server/src/log_read/mod.rs`, `crates/stroem-server/src/log_read/matcher.rs`
- Modify: `crates/stroem-server/src/lib.rs` (add `pub mod log_read;`)
- Modify: `crates/stroem-server/src/log_storage.rs:504-520` (delegate the old method)

**Interfaces:**
- Produces: `crate::log_read::matcher::line_matches_step(line: &str, step_name: &str) -> bool`; `crate::log_read::matcher::matches_bytes(line: &[u8], step_name: &str) -> bool`; constants `CHUNK: usize`, `RANGE: u64`, `READ_SLACK: usize`; `pub enum StepFilter<'a> { All, Step(&'a str) }` with `fn step(self) -> Option<&'a str>`.

- [ ] **Step 1: Create the module skeleton**

`crates/stroem-server/src/log_read/mod.rs`:

```rust
//! Bounded log reads: tails, full streams and the terminal-job merge.
//! Spec: `docs/superpowers/specs/2026-09-22-log-tail-streaming-design.md`.
// Pieces land task by task and are wired into `LogStorage` in Task 8;
// Task 10 removes this allow.
#![allow(dead_code)]

pub(crate) mod matcher;

/// Chunk size of streamed bodies and reader buffers (K).
pub const CHUNK: usize = 64 * 1024;
/// Size of one archive range read (R).
pub const RANGE: u64 = 1024 * 1024;
/// Spare capacity so tokio's `read_to_end` never grows a preallocated
/// buffer: it reserves only when fewer than 32 bytes of spare capacity
/// remain (`tokio/src/io/util/vec_with_initialized.rs`).
pub const READ_SLACK: usize = 32;

/// Which lines of a job log a read returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepFilter<'a> {
    All,
    Step(&'a str),
}

impl<'a> StepFilter<'a> {
    pub fn step(self) -> Option<&'a str> {
        match self {
            Self::All => None,
            Self::Step(s) => Some(s),
        }
    }
}
```

Add `pub mod log_read;` to `crates/stroem-server/src/lib.rs` next to `pub mod log_storage;`.

- [ ] **Step 2: Write the failing matcher tests**

`crates/stroem-server/src/log_read/matcher.rs`:

```rust
//! Exact-match step filter for JSONL log lines.

#[cfg(test)]
mod tests {
    use super::line_matches_step as m;

    #[test]
    fn matches_the_exact_step() {
        assert!(m(r#"{"ts":"t","stream":"stdout","step":"build","line":"x"}"#, "build"));
    }

    #[test]
    fn rejects_another_step_that_contains_the_name() {
        assert!(!m(r#"{"step":"build-docs","line":"build"}"#, "build"));
    }

    #[test]
    fn rejects_plain_text_and_non_objects() {
        assert!(!m("build started", "build"));
        assert!(!m(r#"["build"]"#, "build"));
        assert!(!m(r#""build""#, "build"));
    }

    #[test]
    fn the_last_duplicate_step_key_wins() {
        assert!(m(r#"{"step":"test","step":"build"}"#, "build"));
        assert!(!m(r#"{"step":"build","step":"test"}"#, "build"));
        assert!(!m(r#"{"step":"build","step":1}"#, "build"));
    }

    #[test]
    fn a_compound_first_step_value_is_drained() {
        assert!(m(r#"{"step":{"x":[1,{"y":"build"}]},"step":"build"}"#, "build"));
    }

    #[test]
    fn non_string_step_values_do_not_match() {
        assert!(!m(r#"{"step":1,"line":"build"}"#, "build"));
        assert!(!m(r#"{"step":null,"line":"build"}"#, "build"));
        assert!(!m(r#"{"step":true,"line":"build"}"#, "build"));
        assert!(!m(r#"{"step":["build"]}"#, "build"));
    }

    #[test]
    fn an_escaped_step_key_matches() {
        assert!(m(r#"{"step":"build"}"#, "build"));
    }

    #[test]
    fn an_escaped_step_value_is_rejected_by_the_fast_guard_as_before() {
        // The raw line does not contain "build", so the `contains` guard
        // rejects it before parsing — the old matcher behaved the same way.
        assert!(!m(r#"{"step":"build"}"#, "build"));
        assert!(!super::legacy_value_matcher(r#"{"step":"build"}"#, "build"));
    }

    #[test]
    fn an_escaped_step_value_matches_when_the_name_also_appears_literally() {
        assert!(m(r#"{"step":"build","line":"build"}"#, "build"));
    }

    #[test]
    fn an_unrelated_escaped_key_does_not_break_the_match() {
        assert!(m(r#"{"step":"build","a":0}"#, "build"));
    }

    #[test]
    fn trailing_garbage_rejects_the_line() {
        assert!(!m(r#"{"step":"build"} garbage"#, "build"));
    }

    #[test]
    fn missing_or_nested_step_does_not_match() {
        assert!(!m(r#"{"line":"build"}"#, "build"));
        assert!(!m(r#"{"meta":{"step":"build"}}"#, "build"));
    }

    #[test]
    fn agrees_with_the_old_value_matcher_on_ordinary_lines() {
        for (line, step) in [
            (r#"{"ts":"t","stream":"stdout","step":"build","line":"compiling"}"#, "build"),
            (r#"{"ts":"t","stream":"stderr","step":"build","line":"warn"}"#, "build"),
            (r#"{"ts":"t","stream":"stdout","step":"build-notify","line":"build"}"#, "build"),
            (r#"{"ts":"t","stream":"stderr","step":"_server","line":"hook failed"}"#, "_server"),
            ("plain text line", "build"),
        ] {
            assert_eq!(m(line, step), super::legacy_value_matcher(line, step), "{line}");
        }
    }

    // ── The documented divergence class: malformed content in a field the
    // matcher ignores. Each test asserts the NEW behaviour and checks the
    // premise that the old `Value` matcher rejected the line.

    #[test]
    fn divergence_invalid_surrogate_in_an_unrelated_field_now_matches() {
        let line = r#"{"step":"build","line":"\uDC00"}"#;
        assert!(m(line, "build"));
        assert!(!super::legacy_value_matcher(line, "build"), "premise: old matcher rejected it");
    }

    #[test]
    fn divergence_nesting_deeper_than_128_in_an_unrelated_field_now_matches() {
        let line = format!(r#"{{"step":"build","line":{}{}}}"#, "[".repeat(129), "]".repeat(129));
        assert!(m(&line, "build"));
        assert!(!super::legacy_value_matcher(&line, "build"), "premise: old matcher rejected it");
    }

    #[test]
    fn divergence_out_of_range_number_in_an_unrelated_field_now_matches() {
        let line = r#"{"step":"build","n":1e400}"#;
        assert!(m(line, "build"));
        assert!(!super::legacy_value_matcher(line, "build"), "premise: old matcher rejected it");
    }
}
```

If any divergence test's premise assertion fails, that member is not a divergence: flip its first assertion to `!m(..)`, delete the premise line, and remove the member from the spec's divergence list in § 3.2.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p stroem-server --lib log_read::matcher`
Expected: compile error — `cannot find function line_matches_step`.

- [ ] **Step 4: Implement the matcher**

Insert above the tests module in `matcher.rs`:

```rust
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::fmt;

/// `true` when `line` is a JSON object whose `step` field — the last one if
/// the key repeats — is a string equal to `step_name`. Same contract as the
/// `serde_json::Value` matcher it replaces, without building a `Value`:
/// ignored fields are skipped by `IgnoredAny`, the `step` value is compared
/// inside the visitor, so matching an `n`-byte line allocates at most
/// serde_json's escape scratch for that line.
pub fn line_matches_step(line: &str, step_name: &str) -> bool {
    // Fast path: most lines of a multi-step job belong to other steps.
    if !line.contains(step_name) {
        return false;
    }
    let mut de = serde_json::Deserializer::from_str(line);
    let matched = match de::Deserializer::deserialize_map(&mut de, LineVisitor { step_name }) {
        Ok(matched) => matched,
        Err(_) => return false,
    };
    // Reject trailing content, as `serde_json::from_str` does.
    de.end().is_ok() && matched
}

/// [`line_matches_step`] over raw bytes; invalid UTF-8 never matches.
pub fn matches_bytes(line: &[u8], step_name: &str) -> bool {
    std::str::from_utf8(line).is_ok_and(|s| line_matches_step(s, step_name))
}

/// The matcher this module replaced, kept for tests that pin equivalence
/// and the documented divergences.
#[cfg(test)]
pub(crate) fn legacy_value_matcher(line: &str, step_name: &str) -> bool {
    if !line.contains(step_name) {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(line)
        .map(|v| v.get("step").and_then(|s| s.as_str()) == Some(step_name))
        .unwrap_or(false)
}

struct LineVisitor<'s> {
    step_name: &'s str,
}

impl<'de> Visitor<'de> for LineVisitor<'_> {
    type Value = bool;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
        let mut matched = false;
        while let Some(is_step) = map.next_key_seed(KeyIsStep)? {
            if is_step {
                matched = map.next_value_seed(StepValueEquals {
                    step_name: self.step_name,
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(matched)
    }
}

/// Deserialises a map key and reports whether it is `step`. Escaped keys
/// arrive through `visit_str`, unescaped ones through `visit_borrowed_str`,
/// which forwards to `visit_str` by default.
struct KeyIsStep;

impl<'de> DeserializeSeed<'de> for KeyIsStep {
    type Value = bool;

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<bool, D::Error> {
        d.deserialize_str(self)
    }
}

impl<'de> Visitor<'de> for KeyIsStep {
    type Value = bool;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string key")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<bool, E> {
        Ok(v == "step")
    }
}

/// Deserialises the `step` value and compares it in place. Compound values
/// are drained so a later duplicate `step` key is still reached.
struct StepValueEquals<'s> {
    step_name: &'s str,
}

impl<'de> DeserializeSeed<'de> for StepValueEquals<'_> {
    type Value = bool;

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<bool, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for StepValueEquals<'_> {
    type Value = bool;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<bool, E> {
        Ok(v == self.step_name)
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_unit<E: de::Error>(self) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(false)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(false)
    }
}
```

- [ ] **Step 5: Route the old method through the new matcher**

Replace the body of `LogStorage::line_matches_step` (`log_storage.rs:508-520`) with:

```rust
    fn line_matches_step(line: &str, step_name: &str) -> bool {
        crate::log_read::matcher::line_matches_step(line, step_name)
    }
```

- [ ] **Step 6: Run the matcher tests and the existing step-log tests**

Run: `cargo test -p stroem-server --lib log_read::matcher && cargo test -p stroem-server --lib log_storage::tests && cargo test -p stroem-server --lib state::tests`
Expected: all PASS — the existing `get_step_log` fixtures (`log_storage.rs:768-930`, `:1478-1505`) now run through the new matcher unchanged.

- [ ] **Step 7: Commit**

```bash
git add crates/stroem-server/src/lib.rs crates/stroem-server/src/log_read crates/stroem-server/src/log_storage.rs
git commit -m "feat(logs): allocation-free step matcher with the old matcher's semantics"
```

---

### Task 3: Shared read helpers and the merge reservation fix

**Files:**
- Modify: `crates/stroem-server/src/log_read/mod.rs`
- Modify: `crates/stroem-server/src/log_storage.rs:50` (`merge_jsonl_logs` output reservation) and its tests

**Interfaces:**
- Consumes: `matcher::matches_bytes` (Task 2).
- Produces (all in `crate::log_read`): `pub enum LogSource { Local, Archive, Merged, None }` with `as_str()`; `pub const LOG_SOURCE_HEADER: &str`; `pub struct Tail { pub logs: String, pub truncated: bool, pub total_bytes: u64, pub source: LogSource }` with `Tail::empty()` and `returned_bytes()`; `pub fn tail_body(&Tail) -> anyhow::Result<Vec<u8>>`; `pub fn human_bytes(u64) -> String`; `pub(crate) fn count_lines(&[u8]) -> usize`; `pub(crate) fn cut_front_to_lines(String, u64) -> (String, bool)`; `pub(crate) fn trim_torn(&mut Vec<u8>) -> bool`; `pub(crate) fn retain_matching_lines(&mut Vec<u8>, &str, usize)`; `pub(crate) fn bytes_stream(String) -> BoxStream<'static, std::io::Result<Bytes>>`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/stroem-server/src/log_read/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::TryStreamExt;

    #[test]
    fn human_bytes_uses_iec_units_with_one_decimal() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(262_144), "256.0 KiB");
        assert_eq!(human_bytes(87_325_871), "83.3 MiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn count_lines_skips_blank_lines_and_counts_an_unterminated_one() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"a\n\nb\n"), 2);
        assert_eq!(count_lines(b"a\nb"), 2);
    }

    #[test]
    fn cut_front_to_lines_keeps_whole_lines_only() {
        let s = || String::from("aa\nbb\ncc\n");
        assert_eq!(cut_front_to_lines(s(), 100), (s(), false));
        assert_eq!(cut_front_to_lines(s(), 6), ("bb\ncc\n".to_string(), true));
        assert_eq!(cut_front_to_lines(s(), 5), ("cc\n".to_string(), true));
        assert_eq!(cut_front_to_lines("aaaaaa\n".to_string(), 3), (String::new(), true));
    }

    #[test]
    fn cut_front_to_lines_does_not_allocate() {
        let s = String::from("aa\nbb\ncc\n");
        let cap = s.capacity();
        let (out, _) = cut_front_to_lines(s, 6);
        assert_eq!(out.capacity(), cap);
    }

    #[test]
    fn trim_torn_drops_only_an_unterminated_suffix() {
        let mut a = b"a\nb".to_vec();
        assert!(trim_torn(&mut a));
        assert_eq!(a, b"a\n");
        let mut b = b"a\n".to_vec();
        assert!(!trim_torn(&mut b));
        assert_eq!(b, b"a\n");
        let mut c = b"abc".to_vec();
        assert!(trim_torn(&mut c));
        assert!(c.is_empty());
        let mut d = Vec::new();
        assert!(!trim_torn(&mut d));
    }

    #[test]
    fn retain_matching_lines_filters_in_place() {
        let mut buf = concat!(
            r#"{"step":"a","line":"1"}"#, "\n",
            r#"{"step":"b","line":"2"}"#, "\n",
            r#"{"step":"a","line":"3"}"#
        )
        .as_bytes()
        .to_vec();
        let cap = buf.capacity();
        retain_matching_lines(&mut buf, "a", 1024);
        assert_eq!(
            String::from_utf8(buf.clone()).unwrap(),
            concat!(r#"{"step":"a","line":"1"}"#, "\n", r#"{"step":"a","line":"3"}"#)
        );
        assert_eq!(buf.capacity(), cap);
    }

    #[test]
    fn retain_matching_lines_drops_lines_over_the_line_cap() {
        let long = format!(r#"{{"step":"a","line":"{}"}}"#, "x".repeat(100));
        let mut buf = format!("{long}\n{}\n", r#"{"step":"a","line":"1"}"#).into_bytes();
        retain_matching_lines(&mut buf, "a", 50);
        assert_eq!(String::from_utf8(buf).unwrap(), "{\"step\":\"a\",\"line\":\"1\"}\n");
    }

    #[test]
    fn tail_body_never_reallocates_for_worst_case_escapes() {
        let tail = Tail {
            logs: "\u{1}".repeat(4096),
            truncated: true,
            total_bytes: u64::MAX,
            source: LogSource::Merged,
        };
        let body = tail_body(&tail).unwrap();
        assert_eq!(body.capacity(), 6 * 4096 + 256, "the envelope buffer must not grow");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["returned_bytes"], 4096);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["total_bytes"], u64::MAX);
        assert_eq!(v["logs"].as_str().unwrap().len(), 4096);
    }

    #[test]
    fn log_source_header_values() {
        assert_eq!(LogSource::Local.as_str(), "local");
        assert_eq!(LogSource::Archive.as_str(), "archive");
        assert_eq!(LogSource::Merged.as_str(), "merged");
        assert_eq!(LogSource::None.as_str(), "none");
        assert_eq!(Tail::empty().source, LogSource::None);
    }

    #[tokio::test]
    async fn bytes_stream_yields_chunk_sized_slices() {
        let s = "x".repeat(CHUNK * 2 + 10);
        let chunks: Vec<bytes::Bytes> = bytes_stream(s.clone()).try_collect().await.unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.concat(), s.into_bytes());
    }
}
```

Append to the tests module of `crates/stroem-server/src/log_storage.rs`:

```rust
    #[test]
    fn merge_jsonl_logs_reserves_room_for_two_missing_newlines() {
        // Each input's last line gains a '\n' it did not have; the output
        // must be reserved for that, not reallocated.
        let merged = merge_jsonl_logs("AAAA", "BBBB");
        assert_eq!(merged, "AAAA\nBBBB\n");
        assert_eq!(merged.capacity(), merged.len(), "output reallocated");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p stroem-server --lib log_read::tests; cargo test -p stroem-server --lib merge_jsonl_logs_reserves`
Expected: compile errors for the missing helpers; the merge test FAILS with capacity 16 ≠ 10.

- [ ] **Step 3: Implement the helpers**

Insert into `log_read/mod.rs` after `impl StepFilter` (before the tests module):

```rust
use bytes::Bytes;
use futures_core::stream::BoxStream;
use serde::Serialize;

/// Response header naming the source that served a log read.
pub const LOG_SOURCE_HEADER: &str = "x-stroem-log-source";

/// Which source served a log read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    Local,
    Archive,
    Merged,
    None,
}

impl LogSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Archive => "archive",
            Self::Merged => "merged",
            Self::None => "none",
        }
    }
}

/// A bounded read of a log's end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail {
    /// Whole JSONL lines, at most the requested `tail_bytes`.
    pub logs: String,
    /// `true` when something that exists was left out; `false` is exact.
    pub truncated: bool,
    /// Upper bound on the size of the complete job log.
    pub total_bytes: u64,
    pub source: LogSource,
}

impl Tail {
    pub fn empty() -> Self {
        Self {
            logs: String::new(),
            truncated: false,
            total_bytes: 0,
            source: LogSource::None,
        }
    }

    pub fn returned_bytes(&self) -> u64 {
        self.logs.len() as u64
    }
}

/// Serialise the tail envelope into a buffer preallocated to its worst
/// case: serde_json escapes one byte into at most six (`\u00XX`), and 256
/// bytes cover the field names, punctuation, a bool and two `u64`s.
pub fn tail_body(tail: &Tail) -> anyhow::Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Envelope<'a> {
        logs: &'a str,
        truncated: bool,
        total_bytes: u64,
        returned_bytes: u64,
    }
    let mut body = Vec::with_capacity(6 * tail.logs.len() + 256);
    serde_json::to_writer(
        &mut body,
        &Envelope {
            logs: &tail.logs,
            truncated: tail.truncated,
            total_bytes: tail.total_bytes,
            returned_bytes: tail.returned_bytes(),
        },
    )?;
    Ok(body)
}

/// `262144` → `256.0 KiB`.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 3] = ["KiB", "MiB", "GiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Non-empty lines in a JSONL buffer; an unterminated last line counts.
pub(crate) fn count_lines(buf: &[u8]) -> usize {
    buf.split(|&b| b == b'\n').filter(|l| !l.is_empty()).count()
}

/// Drop whole lines from the front until `s` is at most `max` bytes, in
/// place. Returns whether anything was dropped.
pub(crate) fn cut_front_to_lines(mut s: String, max: u64) -> (String, bool) {
    let max = usize::try_from(max).unwrap_or(usize::MAX);
    if s.len() <= max {
        return (s, false);
    }
    let start = s.len() - max;
    let bytes = s.as_bytes();
    let cut = if bytes[start - 1] == b'\n' {
        start
    } else {
        bytes[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(s.len(), |p| start + p + 1)
    };
    s.drain(..cut);
    (s, true)
}

/// Drop an unterminated last line. Every writer of a `.jsonl` log ends each
/// line with `\n`, so an unterminated suffix is a write in progress.
/// Returns whether anything was dropped.
pub(crate) fn trim_torn(buf: &mut Vec<u8>) -> bool {
    if buf.last().is_none_or(|&b| b == b'\n') {
        return false;
    }
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
    buf.truncate(end);
    true
}

/// Keep only the lines of `buf` whose step is `step`, in place. Lines
/// longer than `max_line` are dropped, as every filtered read does.
pub(crate) fn retain_matching_lines(buf: &mut Vec<u8>, step: &str, max_line: usize) {
    let len = buf.len();
    let (mut read, mut write) = (0, 0);
    while read < len {
        let end = buf[read..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(len, |p| read + p + 1);
        let keep = {
            let line = &buf[read..end];
            let body = line.strip_suffix(b"\n").unwrap_or(line);
            body.len() <= max_line && matcher::matches_bytes(body, step)
        };
        if keep {
            buf.copy_within(read..end, write);
            write += end - read;
        }
        read = end;
    }
    buf.truncate(write);
}

/// Serve an in-memory string as a body of `CHUNK`-sized slices without
/// copying it.
pub(crate) fn bytes_stream(s: String) -> BoxStream<'static, std::io::Result<Bytes>> {
    let bytes = Bytes::from(s);
    let len = bytes.len();
    Box::pin(futures_util::stream::iter(
        (0..len)
            .step_by(CHUNK)
            .map(move |i| Ok(bytes.slice(i..(i + CHUNK).min(len)))),
    ))
}
```

In `log_storage.rs`, change the reservation in `merge_jsonl_logs` (line 50):

```rust
    // +2: each input's last line gains a '\n' if it had none.
    let mut out = String::with_capacity(a.len() + b.len() + 2);
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p stroem-server --lib log_read::tests && cargo test -p stroem-server --lib merge_jsonl_logs`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/log_read/mod.rs crates/stroem-server/src/log_storage.rs
git commit -m "feat(logs): tail envelope, byte helpers; reserve merge output for added newlines"
```

### Task 4: The lending line splitter and the filtered stream

**Files:**
- Create: `crates/stroem-server/src/log_read/splitter.rs`
- Modify: `crates/stroem-server/src/log_read/mod.rs` (add `pub(crate) mod splitter;`)

**Interfaces:**
- Consumes: `matcher::matches_bytes`, `CHUNK`.
- Produces: `pub enum LineRef<'a> { Line(&'a [u8]), Unterminated(&'a [u8]), Skipped { bytes: u64 } }`; `pub struct LineSplitter<R>` with `new(reader: R, max_line: usize)`, `async fn next(&mut self) -> io::Result<Option<LineRef<'_>>>`, `max_line()`, `bytes_consumed() -> u64`; `pub fn filtered_stream<R>(reader: R, step: String, max_line: usize, keep_unterminated: bool) -> BoxStream<'static, io::Result<Bytes>>` where `R: AsyncBufRead + Unpin + Send + 'static`.

- [ ] **Step 1: Write the failing tests**

`crates/stroem-server/src/log_read/splitter.rs`:

```rust
//! Splits a reader into lines lent from one preallocated buffer.

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::TryStreamExt;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, BufReader, ReadBuf};

    async fn collect(input: &[u8], max_line: usize, buf_cap: usize) -> Vec<String> {
        let mut split = LineSplitter::new(BufReader::with_capacity(buf_cap, input), max_line);
        let mut out = Vec::new();
        while let Some(item) = split.next().await.unwrap() {
            out.push(match item {
                LineRef::Line(l) => format!("L:{}", String::from_utf8_lossy(l)),
                LineRef::Unterminated(l) => format!("U:{}", String::from_utf8_lossy(l)),
                LineRef::Skipped { bytes } => format!("S:{bytes}"),
            });
        }
        out
    }

    #[tokio::test]
    async fn splits_lines_and_reports_an_unterminated_final_line() {
        assert_eq!(collect(b"a\nbb\nccc", 16, 4).await, ["L:a", "L:bb", "U:ccc"]);
    }

    #[tokio::test]
    async fn lines_split_across_fill_buf_boundaries() {
        assert_eq!(collect(b"hello world\nx\n", 64, 3).await, ["L:hello world", "L:x"]);
    }

    #[tokio::test]
    async fn an_oversize_line_is_skipped_and_the_next_line_delivered() {
        assert_eq!(
            collect(b"ok\n0123456789\nnext\n", 5, 4).await,
            ["L:ok", "S:10", "L:next"]
        );
    }

    #[tokio::test]
    async fn an_oversize_final_line_without_newline_is_skipped() {
        assert_eq!(collect(b"ok\n0123456789", 5, 4).await, ["L:ok", "S:10"]);
    }

    #[tokio::test]
    async fn empty_input_yields_nothing() {
        assert!(collect(b"", 8, 4).await.is_empty());
    }

    #[tokio::test]
    async fn counts_consumed_bytes() {
        let mut split = LineSplitter::new(BufReader::with_capacity(4, &b"ab\ncdef\ng"[..]), 3);
        while split.next().await.unwrap().is_some() {}
        assert_eq!(split.bytes_consumed(), 9);
    }

    #[tokio::test]
    async fn the_accumulator_never_grows() {
        let mut input = Vec::new();
        for i in 0..10_000 {
            input.extend_from_slice(format!("line {i}\n").as_bytes());
        }
        input.extend_from_slice(&[b'x'; 100]);
        input.push(b'\n');
        let mut split = LineSplitter::new(BufReader::with_capacity(7, &input[..]), 32);
        assert_eq!(split.accumulator_capacity(), 32);
        while split.next().await.unwrap().is_some() {
            assert_eq!(split.accumulator_capacity(), 32);
        }
    }

    fn line(step: &str, n: usize) -> String {
        format!(r#"{{"step":"{step}","line":"{n}"}}"#)
    }

    #[tokio::test]
    async fn filtered_stream_keeps_matching_lines_in_several_chunks() {
        let mut input = String::new();
        let mut expected = String::new();
        for i in 0..10_000 {
            let l = line(if i % 2 == 0 { "a" } else { "b" }, i);
            if i % 2 == 0 {
                expected.push_str(&l);
                expected.push('\n');
            }
            input.push_str(&l);
            input.push('\n');
        }
        let reader = BufReader::new(std::io::Cursor::new(input.into_bytes()));
        let chunks: Vec<Bytes> = filtered_stream(reader, "a".into(), 1024, false)
            .try_collect()
            .await
            .unwrap();
        assert!(chunks.len() > 1, "a body over CHUNK must arrive in several chunks");
        assert!(chunks.iter().all(|c| c.len() <= CHUNK + 1024 + 1));
        assert_eq!(chunks.concat(), expected.into_bytes());
    }

    #[tokio::test]
    async fn filtered_stream_keeps_an_unterminated_line_only_when_asked() {
        let input = format!("{}\n{}", line("a", 1), line("a", 2));
        for (keep, want) in [
            (true, format!("{}\n{}\n", line("a", 1), line("a", 2))),
            (false, format!("{}\n", line("a", 1))),
        ] {
            let reader = BufReader::new(std::io::Cursor::new(input.clone().into_bytes()));
            let chunks: Vec<Bytes> = filtered_stream(reader, "a".into(), 1024, keep)
                .try_collect()
                .await
                .unwrap();
            assert_eq!(String::from_utf8(chunks.concat()).unwrap(), want);
        }
    }

    /// Returns `Pending` before every byte.
    struct Trickle {
        data: Vec<u8>,
        pos: usize,
        pend: bool,
    }

    impl AsyncRead for Trickle {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if std::mem::replace(&mut self.pend, !self.pend) {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            if self.pos < self.data.len() {
                let b = self.data[self.pos];
                buf.put_slice(&[b]);
                self.pos += 1;
            }
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn filtered_stream_survives_pending_in_the_middle_of_a_line() {
        let input = format!("{}\n{}\n{}\n", line("a", 1), line("b", 2), line("a", 3));
        let reader = BufReader::with_capacity(
            4,
            Trickle { data: input.into_bytes(), pos: 0, pend: true },
        );
        let chunks: Vec<Bytes> = filtered_stream(reader, "a".into(), 1024, false)
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(chunks.concat()).unwrap(),
            format!("{}\n{}\n", line("a", 1), line("a", 3))
        );
    }
}
```

Add `pub(crate) mod splitter;` to `log_read/mod.rs`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p stroem-server --lib log_read::splitter`
Expected: compile error — `cannot find type LineSplitter`.

- [ ] **Step 3: Implement the splitter and the filtered stream**

Insert above the tests module:

```rust
use super::{matcher, CHUNK};
use bytes::Bytes;
use futures_core::stream::BoxStream;
use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// One item from [`LineSplitter::next`], borrowed until the next call.
#[derive(Debug, PartialEq, Eq)]
pub enum LineRef<'a> {
    /// A line that ended with `\n` (the newline is not included).
    Line(&'a [u8]),
    /// The input's final line, which had no `\n`.
    Unterminated(&'a [u8]),
    /// A line longer than `max_line`, discarded without being buffered.
    Skipped { bytes: u64 },
}

/// Lends each line of `reader` from one accumulator of `max_line` bytes,
/// allocated once. Not `FramedRead`: that grows its buffer by doubling and
/// ends the stream after any decoder error, so an oversize line would end
/// the read (`tokio-util/src/codec/framed_impl.rs`).
pub struct LineSplitter<R> {
    reader: R,
    acc: Vec<u8>,
    max_line: usize,
    /// `Some(bytes so far)` while an oversize line is being discarded.
    discarding: Option<u64>,
    /// `acc` holds a line lent by the previous `next` call.
    lent: bool,
    consumed: u64,
}

impl<R: AsyncBufRead + Unpin> LineSplitter<R> {
    pub fn new(reader: R, max_line: usize) -> Self {
        Self {
            reader,
            acc: Vec::with_capacity(max_line),
            max_line,
            discarding: None,
            lent: false,
            consumed: 0,
        }
    }

    pub fn max_line(&self) -> usize {
        self.max_line
    }

    /// Bytes read from the underlying reader so far.
    pub fn bytes_consumed(&self) -> u64 {
        self.consumed
    }

    #[cfg(test)]
    pub fn accumulator_capacity(&self) -> usize {
        self.acc.capacity()
    }

    /// The next line. Cancel-safe: bytes are consumed from the reader only
    /// after they are copied, and a partial line stays in the accumulator.
    pub async fn next(&mut self) -> io::Result<Option<LineRef<'_>>> {
        if self.lent {
            self.acc.clear();
            self.lent = false;
        }
        loop {
            let buf = self.reader.fill_buf().await?;
            if buf.is_empty() {
                if let Some(bytes) = self.discarding.take() {
                    return Ok(Some(LineRef::Skipped { bytes }));
                }
                if self.acc.is_empty() {
                    return Ok(None);
                }
                self.lent = true;
                return Ok(Some(LineRef::Unterminated(&self.acc)));
            }
            let newline = buf.iter().position(|&b| b == b'\n');
            let take = newline.unwrap_or(buf.len());
            let consume = take + usize::from(newline.is_some());
            if let Some(discarded) = self.discarding.as_mut() {
                *discarded += take as u64;
                self.reader.consume(consume);
                self.consumed += consume as u64;
                if newline.is_some() {
                    let bytes = self.discarding.take().unwrap_or_default();
                    return Ok(Some(LineRef::Skipped { bytes }));
                }
                continue;
            }
            if self.acc.len() + take > self.max_line {
                // Discard from here on; the branch above handles these same
                // bytes on the next iteration.
                self.discarding = Some(self.acc.len() as u64);
                self.acc.clear();
                continue;
            }
            self.acc.extend_from_slice(&buf[..take]);
            self.reader.consume(consume);
            self.consumed += consume as u64;
            if newline.is_some() {
                self.lent = true;
                return Ok(Some(LineRef::Line(&self.acc)));
            }
        }
    }
}

/// Stream the lines of `reader` whose step is `step`, each followed by
/// `\n`, in chunks of at least `CHUNK` bytes (the last may be shorter).
/// Output buffers are `CHUNK + max_line + 1` bytes, so a match always fits
/// and no buffer grows. Oversize lines are skipped with a warning; an
/// unterminated final line is kept only when `keep_unterminated` (legacy
/// `.log` files, which have no line contract).
pub fn filtered_stream<R>(
    reader: R,
    step: String,
    max_line: usize,
    keep_unterminated: bool,
) -> BoxStream<'static, io::Result<Bytes>>
where
    R: AsyncBufRead + Unpin + Send + 'static,
{
    struct State<R> {
        split: LineSplitter<R>,
        step: String,
        keep_unterminated: bool,
        done: bool,
    }
    enum Next {
        More,
        Eof,
        Fail(io::Error),
    }

    let state = State {
        split: LineSplitter::new(reader, max_line),
        step,
        keep_unterminated,
        done: false,
    };
    Box::pin(futures_util::stream::unfold(state, |mut st| async move {
        if st.done {
            return None;
        }
        let mut out: Vec<u8> = Vec::with_capacity(CHUNK + st.split.max_line() + 1);
        loop {
            let next = match st.split.next().await {
                Err(e) => Next::Fail(e),
                Ok(None) => Next::Eof,
                Ok(Some(LineRef::Skipped { bytes })) => {
                    tracing::warn!(bytes, "log line over max_line_bytes skipped in a filtered read");
                    Next::More
                }
                Ok(Some(LineRef::Unterminated(line))) => {
                    if st.keep_unterminated && matcher::matches_bytes(line, &st.step) {
                        out.extend_from_slice(line);
                        out.push(b'\n');
                    }
                    Next::More
                }
                Ok(Some(LineRef::Line(line))) => {
                    if matcher::matches_bytes(line, &st.step) {
                        out.extend_from_slice(line);
                        out.push(b'\n');
                    }
                    Next::More
                }
            };
            match next {
                Next::More if out.len() < CHUNK => continue,
                Next::More => return Some((Ok(Bytes::from(out)), st)),
                Next::Eof => {
                    st.done = true;
                    return (!out.is_empty()).then(|| (Ok(Bytes::from(out)), st));
                }
                Next::Fail(e) => {
                    st.done = true;
                    return Some((Err(e), st));
                }
            }
        }
    }))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p stroem-server --lib log_read::splitter`
Expected: 10 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/log_read
git commit -m "feat(logs): lending line splitter and filtered log stream"
```

---

### Task 5: Local file reads

**Files:**
- Create: `crates/stroem-server/src/log_read/local.rs`
- Modify: `crates/stroem-server/src/log_read/mod.rs` (add `pub(crate) mod local;`)
- Modify: `crates/stroem-server/Cargo.toml` (`tokio-util = { workspace = true, features = ["io"] }`)

**Interfaces:**
- Consumes: `matcher::matches_bytes`, `splitter::filtered_stream`, `CHUNK`, `READ_SLACK`.
- Produces: `pub(crate) enum LocalKind { Jsonl, Legacy }`; `pub(crate) struct LocalFile { pub file: tokio::fs::File, pub kind: LocalKind, pub len: u64 }`; `pub(crate) struct LocalTail { pub logs: String, pub truncated: bool, pub len: u64 }`; `open_local(jsonl: &Path, legacy: &Path) -> io::Result<Option<LocalFile>>`; `read_range_exact(file: &mut File, start: u64, end: u64, buf: &mut Vec<u8>) -> io::Result<()>`; `tail_unfiltered(f: &mut LocalFile, t: u64) -> io::Result<LocalTail>`; `tail_step(f: &mut LocalFile, step: &str, t: u64, max_line: usize, scan_max: u64) -> io::Result<LocalTail>`; `full_stream(f: LocalFile, step: Option<String>, max_line: usize) -> io::Result<BoxStream<'static, io::Result<Bytes>>>`.

- [ ] **Step 1: Write the failing tests**

`crates/stroem-server/src/log_read/local.rs`:

```rust
//! Reads of a job's local log file: `{job}.jsonl`, else the legacy `{job}.log`.

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::TryStreamExt;
    use tempfile::TempDir;

    fn jl(step: &str, msg: &str) -> String {
        format!(r#"{{"ts":"2026-09-22T00:00:00Z","stream":"stdout","step":"{step}","line":"{msg}"}}"#)
    }

    /// A JSONL line of exactly `len` bytes, newline excluded.
    fn sized(step: &str, len: usize) -> String {
        let base = format!(r#"{{"step":"{step}","line":""}}"#).len();
        assert!(len >= base, "line too short for step {step}");
        format!(r#"{{"step":"{step}","line":"{}"}}"#, "x".repeat(len - base))
    }

    async fn open(dir: &TempDir, kind: LocalKind, content: &str) -> LocalFile {
        let (jsonl, legacy) = (dir.path().join("j.jsonl"), dir.path().join("j.log"));
        let path = if kind == LocalKind::Jsonl { &jsonl } else { &legacy };
        tokio::fs::write(path, content).await.unwrap();
        open_local(&jsonl, &legacy).await.unwrap().unwrap()
    }

    async fn full(f: LocalFile, step: Option<&str>) -> String {
        let chunks: Vec<Bytes> = full_stream(f, step.map(str::to_owned), 1024)
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        String::from_utf8(chunks.concat()).unwrap()
    }

    #[tokio::test]
    async fn open_local_prefers_jsonl_then_legacy_then_none() {
        let dir = TempDir::new().unwrap();
        let (jsonl, legacy) = (dir.path().join("j.jsonl"), dir.path().join("j.log"));
        assert!(open_local(&jsonl, &legacy).await.unwrap().is_none());
        tokio::fs::write(&legacy, "old\n").await.unwrap();
        assert_eq!(open_local(&jsonl, &legacy).await.unwrap().unwrap().kind, LocalKind::Legacy);
        tokio::fs::write(&jsonl, "").await.unwrap();
        let f = open_local(&jsonl, &legacy).await.unwrap().unwrap();
        assert_eq!((f.kind, f.len), (LocalKind::Jsonl, 0));
    }

    #[tokio::test]
    async fn unfiltered_tail_returns_whole_lines_from_the_end() {
        let dir = TempDir::new().unwrap();
        let lines: Vec<String> = (0..10).map(|i| jl("s", &format!("line-{i}"))).collect();
        let content: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let per = lines[0].len() as u64 + 1;
        let mut f = open(&dir, LocalKind::Jsonl, &content).await;
        let tail = tail_unfiltered(&mut f, 3 * per + 5).await.unwrap();
        assert_eq!(tail.logs, format!("{}\n{}\n{}\n", lines[7], lines[8], lines[9]));
        assert!(tail.truncated);
        assert_eq!(tail.len, content.len() as u64);
    }

    #[tokio::test]
    async fn unfiltered_tail_keeps_a_first_line_that_starts_on_the_boundary() {
        let dir = TempDir::new().unwrap();
        let lines: Vec<String> = (0..5).map(|i| jl("s", &format!("line-{i}"))).collect();
        let content: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let per = lines[0].len() as u64 + 1;
        let mut f = open(&dir, LocalKind::Jsonl, &content).await;
        let tail = tail_unfiltered(&mut f, 3 * per).await.unwrap();
        assert_eq!(tail.logs, format!("{}\n{}\n{}\n", lines[2], lines[3], lines[4]));
    }

    #[tokio::test]
    async fn unfiltered_tail_edge_cases() {
        let dir = TempDir::new().unwrap();
        let mut f = open(&dir, LocalKind::Jsonl, "a\nb\n").await;
        let whole = tail_unfiltered(&mut f, 1024).await.unwrap();
        assert_eq!((whole.logs.as_str(), whole.truncated), ("a\nb\n", false));

        let mut empty = open(&dir, LocalKind::Jsonl, "").await;
        let t = tail_unfiltered(&mut empty, 1024).await.unwrap();
        assert_eq!((t.logs.as_str(), t.truncated), ("", false));

        let mut long = open(&dir, LocalKind::Jsonl, &format!("{}\n", "x".repeat(100))).await;
        let t = tail_unfiltered(&mut long, 10).await.unwrap();
        assert_eq!((t.logs.as_str(), t.truncated), ("", true));
    }

    #[tokio::test]
    async fn unfiltered_tail_reads_exactly_the_snapshot() {
        let dir = TempDir::new().unwrap();
        let mut f = open(&dir, LocalKind::Jsonl, "a\nb\n").await;
        {
            use std::io::Write;
            let mut w = std::fs::OpenOptions::new()
                .append(true)
                .open(dir.path().join("j.jsonl"))
                .unwrap();
            w.write_all(b"appended-after-snapshot\n").unwrap();
        }
        let t = tail_unfiltered(&mut f, 1024).await.unwrap();
        assert_eq!(t.logs, "a\nb\n");
        assert!(t.logs.len() as u64 <= t.len);
    }

    #[tokio::test]
    async fn torn_last_line_is_dropped_from_jsonl_and_kept_in_legacy() {
        let dir = TempDir::new().unwrap();
        let mut j = open(&dir, LocalKind::Jsonl, "a-line\nhalf").await;
        let t = tail_unfiltered(&mut j, 1024).await.unwrap();
        assert_eq!((t.logs.as_str(), t.truncated), ("a-line\n", true));

        let dir2 = TempDir::new().unwrap();
        let mut l = open(&dir2, LocalKind::Legacy, "legacy content").await;
        let t = tail_unfiltered(&mut l, 1024).await.unwrap();
        assert_eq!((t.logs.as_str(), t.truncated), ("legacy content", false));
    }

    #[tokio::test]
    async fn step_tail_assembles_lines_across_windows() {
        let dir = TempDir::new().unwrap();
        let (a, b, c) = (sized("a", 39), sized("b", 39), sized("a", 39));
        let mut f = open(&dir, LocalKind::Jsonl, &format!("{a}\n{b}\n{c}\n")).await;
        let t = tail_step(&mut f, "a", 100, 1024, u64::MAX).await.unwrap();
        assert_eq!(t.logs, format!("{a}\n{c}\n"));
        assert!(!t.truncated, "scan reached the start with nothing excluded");
    }

    #[tokio::test]
    async fn step_tail_finds_a_quiet_step_behind_a_chatty_one() {
        let dir = TempDir::new().unwrap();
        let quiet = sized("quiet", 40);
        let mut content = format!("{quiet}\n");
        for _ in 0..20 {
            content.push_str(&sized("chatty", 40));
            content.push('\n');
        }
        let mut f = open(&dir, LocalKind::Jsonl, &content).await;
        let t = tail_step(&mut f, "quiet", 128, 1024, u64::MAX).await.unwrap();
        assert_eq!((t.logs, t.truncated), (format!("{quiet}\n"), false));
    }

    #[tokio::test]
    async fn step_tail_does_not_take_a_match_that_would_cross_the_budget() {
        let dir = TempDir::new().unwrap();
        let lines: Vec<String> = (0..3).map(|_| sized("a", 59)).collect();
        let content: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let mut f = open(&dir, LocalKind::Jsonl, &content).await;
        let t = tail_step(&mut f, "a", 100, 1024, u64::MAX).await.unwrap();
        assert_eq!(t.logs, format!("{}\n", lines[2]));
        assert!(t.truncated);
    }

    #[tokio::test]
    async fn step_tail_stops_at_the_scan_cap() {
        let dir = TempDir::new().unwrap();
        let mut content = format!("{}\n", sized("quiet", 40));
        for _ in 0..100 {
            content.push_str(&sized("chatty", 40));
            content.push('\n');
        }
        let mut f = open(&dir, LocalKind::Jsonl, &content).await;
        let t = tail_step(&mut f, "quiet", 256, 1024, 1024).await.unwrap();
        assert_eq!((t.logs.as_str(), t.truncated), ("", true));
    }

    #[tokio::test]
    async fn step_tail_matches_exactly() {
        let dir = TempDir::new().unwrap();
        let (b, n) = (jl("build", "compiling"), jl("build-notify", "build done"));
        let mut f = open(&dir, LocalKind::Jsonl, &format!("{b}\n{n}\n")).await;
        let t = tail_step(&mut f, "build", 4096, 1024, u64::MAX).await.unwrap();
        assert_eq!(t.logs, format!("{b}\n"));
    }

    #[tokio::test]
    async fn step_tail_skips_lines_over_the_line_cap() {
        let dir = TempDir::new().unwrap();
        let (long, short) = (sized("a", 80), sized("a", 30));
        // Whole oversize line inside one window.
        let mut f = open(&dir, LocalKind::Jsonl, &format!("{long}\n{short}\n")).await;
        let t = tail_step(&mut f, "a", 4096, 50, u64::MAX).await.unwrap();
        assert_eq!((t.logs, t.truncated), (format!("{short}\n"), true));
        // Oversize line assembled across windows.
        let dir2 = TempDir::new().unwrap();
        let (long2, short2) = (sized("a", 100), sized("a", 20));
        let mut g = open(&dir2, LocalKind::Jsonl, &format!("{long2}\n{short2}\n")).await;
        let t = tail_step(&mut g, "a", 32, 40, u64::MAX).await.unwrap();
        assert_eq!((t.logs, t.truncated), (format!("{short2}\n"), true));
    }

    #[tokio::test]
    async fn step_tail_torn_newest_line() {
        let dir = TempDir::new().unwrap();
        let first = jl("a", "1");
        let mut f = open(&dir, LocalKind::Jsonl, &format!("{first}\n{{\"step\":\"a\",\"li")).await;
        let t = tail_step(&mut f, "a", 4096, 1024, u64::MAX).await.unwrap();
        assert_eq!((t.logs, t.truncated), (format!("{first}\n"), true));

        let dir2 = TempDir::new().unwrap();
        let second = jl("a", "2");
        let mut g = open(&dir2, LocalKind::Legacy, &format!("{first}\n{second}")).await;
        let t = tail_step(&mut g, "a", 4096, 1024, u64::MAX).await.unwrap();
        assert_eq!((t.logs, t.truncated), (format!("{first}\n{second}\n"), false));
    }

    #[tokio::test]
    async fn full_stream_stops_at_the_last_newline_of_a_jsonl_file() {
        let dir = TempDir::new().unwrap();
        let f = open(&dir, LocalKind::Jsonl, "a\nb\nhalf").await;
        assert_eq!(full(f, None).await, "a\nb\n");
        let dir2 = TempDir::new().unwrap();
        let g = open(&dir2, LocalKind::Legacy, "a\nb\nhalf").await;
        assert_eq!(full(g, None).await, "a\nb\nhalf");
    }

    #[tokio::test]
    async fn full_stream_filters_and_chunks() {
        let dir = TempDir::new().unwrap();
        let (a, b) = (jl("a", "1"), jl("b", "2"));
        let f = open(&dir, LocalKind::Jsonl, &format!("{a}\n{b}\n{a}\n")).await;
        assert_eq!(full(f, Some("a")).await, format!("{a}\n{a}\n"));

        let dir2 = TempDir::new().unwrap();
        let big: String = (0..4000).map(|i| format!("{}\n", jl("s", &i.to_string()))).collect();
        let g = open(&dir2, LocalKind::Jsonl, &big).await;
        let chunks: Vec<Bytes> = full_stream(g, None, 1024).await.unwrap().try_collect().await.unwrap();
        assert!(chunks.len() > 1);
        assert_eq!(chunks.concat(), big.into_bytes());
    }
}
```

Add `pub(crate) mod local;` to `log_read/mod.rs`, and in `crates/stroem-server/Cargo.toml` replace `tokio-util.workspace = true` with `tokio-util = { workspace = true, features = ["io"] }`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p stroem-server --lib log_read::local`
Expected: compile error — `cannot find function open_local`.

- [ ] **Step 3: Implement the local reads**

Insert above the tests module:

```rust
use super::splitter::filtered_stream;
use super::{matcher, CHUNK, READ_SLACK};
use bytes::Bytes;
use futures_core::stream::BoxStream;
use std::io::{self, SeekFrom};
use std::path::Path;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, BufReader};
use tokio_util::io::ReaderStream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalKind {
    /// Written by `append_log`: every line ends with `\n`.
    Jsonl,
    /// Pre-JSONL plain text; no line contract.
    Legacy,
}

pub(crate) struct LocalFile {
    pub file: File,
    pub kind: LocalKind,
    /// Length when opened — the snapshot every read of this file respects.
    pub len: u64,
}

pub(crate) struct LocalTail {
    pub logs: String,
    pub truncated: bool,
    pub len: u64,
}

fn to_usize(n: u64) -> io::Result<usize> {
    usize::try_from(n).map_err(io::Error::other)
}

fn utf8(buf: Vec<u8>) -> io::Result<String> {
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// `.jsonl` first, then legacy `.log`. `NotFound` on both is `Ok(None)`:
/// retention can delete a file mid-request, which must not be a 500.
pub(crate) async fn open_local(jsonl: &Path, legacy: &Path) -> io::Result<Option<LocalFile>> {
    for (path, kind) in [(jsonl, LocalKind::Jsonl), (legacy, LocalKind::Legacy)] {
        match File::open(path).await {
            Ok(file) => {
                let len = file.metadata().await?.len();
                return Ok(Some(LocalFile { file, kind, len }));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// Replace `buf` with the bytes `[start, end)`. `buf` must have spare
/// capacity for them plus `READ_SLACK` so tokio never grows it.
pub(crate) async fn read_range_exact(
    file: &mut File,
    start: u64,
    end: u64,
    buf: &mut Vec<u8>,
) -> io::Result<()> {
    file.seek(SeekFrom::Start(start)).await?;
    buf.clear();
    let n = (&mut *file).take(end - start).read_to_end(buf).await?;
    if (n as u64) < end - start {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "log file shrank while being read",
        ));
    }
    Ok(())
}

/// The last `t` bytes of the snapshot, cut to whole lines.
pub(crate) async fn tail_unfiltered(f: &mut LocalFile, t: u64) -> io::Result<LocalTail> {
    let len = f.len;
    let start = len.saturating_sub(t);
    // One byte before the window tells whether its first line is complete.
    let from = start.saturating_sub(1);
    let mut buf = Vec::with_capacity(to_usize(len - from)? + READ_SLACK);
    read_range_exact(&mut f.file, from, len, &mut buf).await?;
    let head = if start == 0 {
        0
    } else if buf[0] == b'\n' {
        1
    } else {
        buf.iter().position(|&b| b == b'\n').map_or(buf.len(), |p| p + 1)
    };
    let mut end = buf.len();
    let mut torn = false;
    if f.kind == LocalKind::Jsonl && end > head && buf[end - 1] != b'\n' {
        end = buf[head..end]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(head, |p| head + p + 1);
        torn = true;
    }
    buf.truncate(end);
    buf.drain(..head);
    Ok(LocalTail {
        logs: utf8(buf)?,
        truncated: start > 0 || torn,
        len,
    })
}

/// Collects matching lines from the end of a result buffer backwards.
struct StepScan<'a> {
    step: &'a str,
    max_line: usize,
    out: &'a mut [u8],
    /// `out[w..]` holds the collected lines.
    w: usize,
    /// The next completed line is the text after the file's last `\n`.
    first_line: bool,
    jsonl: bool,
    truncated: bool,
    stop: bool,
}

impl StepScan<'_> {
    fn oversize(&mut self) {
        self.first_line = false;
        self.truncated = true;
    }

    fn complete(&mut self, line: &[u8]) {
        if std::mem::take(&mut self.first_line) && !line.is_empty() && self.jsonl {
            // A torn record: the writer finishes it later.
            self.truncated = true;
            return;
        }
        if line.is_empty() {
            return;
        }
        if line.len() > self.max_line {
            self.truncated = true;
            return;
        }
        if !matcher::matches_bytes(line, self.step) {
            return;
        }
        let need = line.len() + 1;
        if need > self.w {
            self.truncated = true;
            self.stop = true;
            return;
        }
        self.w -= need;
        self.out[self.w..self.w + line.len()].copy_from_slice(line);
        self.out[self.w + line.len()] = b'\n';
    }
}

fn prepend(pending: &mut Vec<u8>, oversize: &mut bool, seg: &[u8], max_line: usize) {
    if *oversize {
        return;
    }
    if pending.len() + seg.len() > max_line {
        *oversize = true;
        pending.clear();
        return;
    }
    pending.splice(0..0, seg.iter().copied());
}

/// The newest lines of `step`, at most `t` bytes, scanning backwards in
/// windows of `t` bytes. `truncated` is exact when `false`.
pub(crate) async fn tail_step(
    f: &mut LocalFile,
    step: &str,
    t: u64,
    max_line: usize,
    scan_max: u64,
) -> io::Result<LocalTail> {
    let len = f.len;
    let window_len = t.min(len).max(1);
    let mut window = Vec::with_capacity(to_usize(window_len)? + READ_SLACK);
    let cap = to_usize(t.min(len))?;
    let mut out = vec![0u8; cap];
    let mut pending: Vec<u8> = Vec::with_capacity(max_line.min(to_usize(len)?));
    let mut pending_oversize = false;
    let mut scan = StepScan {
        step,
        max_line,
        out: &mut out,
        w: cap,
        first_line: true,
        jsonl: f.kind == LocalKind::Jsonl,
        truncated: false,
        stop: false,
    };
    let mut pos = len;
    let mut scanned = 0u64;
    while pos > 0 && !scan.stop {
        if scanned >= scan_max {
            scan.truncated = true;
            break;
        }
        let start = pos.saturating_sub(window_len);
        read_range_exact(&mut f.file, start, pos, &mut window).await?;
        scanned += pos - start;
        let mut i = window.len();
        while !scan.stop {
            match window[..i].iter().rposition(|&b| b == b'\n') {
                Some(k) => {
                    let seg = &window[k + 1..i];
                    if pending.is_empty() && !pending_oversize {
                        scan.complete(seg);
                    } else {
                        prepend(&mut pending, &mut pending_oversize, seg, max_line);
                        if pending_oversize {
                            scan.oversize();
                        } else {
                            scan.complete(&pending);
                        }
                        pending.clear();
                        pending_oversize = false;
                    }
                    i = k;
                }
                None => {
                    prepend(&mut pending, &mut pending_oversize, &window[..i], max_line);
                    break;
                }
            }
        }
        pos = start;
    }
    if pos == 0 && !scan.stop {
        // The file's first line has no newline before it.
        if pending_oversize {
            scan.oversize();
        } else {
            scan.complete(&pending);
        }
    }
    let (w, truncated) = (scan.w, scan.truncated || pos > 0);
    out.drain(..w);
    Ok(LocalTail {
        logs: utf8(out)?,
        truncated,
        len,
    })
}

/// Offset just past the last `\n` before `len` (0 if none).
async fn last_line_end(file: &mut File, len: u64) -> io::Result<u64> {
    let mut buf = Vec::with_capacity(CHUNK + READ_SLACK);
    let mut pos = len;
    while pos > 0 {
        let start = pos.saturating_sub(CHUNK as u64);
        read_range_exact(file, start, pos, &mut buf).await?;
        if let Some(i) = buf.iter().rposition(|&b| b == b'\n') {
            return Ok(start + i as u64 + 1);
        }
        pos = start;
    }
    Ok(0)
}

/// The whole snapshot as a stream; a `.jsonl` file stops at its last
/// newline (torn-line rule). Filtered reads go through the line splitter.
pub(crate) async fn full_stream(
    mut f: LocalFile,
    step: Option<String>,
    max_line: usize,
) -> io::Result<BoxStream<'static, io::Result<Bytes>>> {
    let end = match f.kind {
        LocalKind::Jsonl => last_line_end(&mut f.file, f.len).await?,
        LocalKind::Legacy => f.len,
    };
    f.file.seek(SeekFrom::Start(0)).await?;
    let body = f.file.take(end);
    Ok(match step {
        None => Box::pin(ReaderStream::with_capacity(body, CHUNK)),
        Some(step) => filtered_stream(
            BufReader::with_capacity(CHUNK, body),
            step,
            max_line,
            f.kind == LocalKind::Legacy,
        ),
    })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p stroem-server --lib log_read::local`
Expected: 15 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/Cargo.toml crates/stroem-server/src/log_read
git commit -m "feat(logs): bounded local tails and streamed full reads"
```

---

### Task 6: Versioned archive range reads

**Files:**
- Modify: `crates/stroem-server/src/blob_storage.rs` (types, trait defaults, local and S3 overrides, tests)
- Create: `crates/stroem-server/src/log_read/archive.rs` (`ArchiveRangeReader`, `gzip_isize`)
- Modify: `crates/stroem-server/src/log_read/mod.rs` (add `pub(crate) mod archive;`)
- Modify: `crates/stroem-server/tests/s3_integration_test.rs` (new test)

**Interfaces:**
- Produces: `pub struct ArchiveObject { pub key: String, pub size: u64, pub version: ArchiveVersion }`; `pub enum ArchiveVersion { Snapshot(Bytes), File(tokio::sync::Mutex<tokio::fs::File>), ETag(Option<String>) }`; trait methods `async fn open(&self, key: &str) -> Result<Option<ArchiveObject>>` and `async fn read_range(&self, obj: &ArchiveObject, offset: u64, len: u64, into: &mut Vec<u8>) -> Result<()>`; `pub(crate) struct ArchiveRangeReader` (`new(Arc<dyn BlobArchive>, Arc<ArchiveObject>)`, `with_range(.., range: u64)`, implements `AsyncBufRead`); `pub(crate) async fn gzip_isize(&dyn BlobArchive, &ArchiveObject) -> anyhow::Result<u64>`.

- [ ] **Step 1: Write the failing blob-storage tests**

Append to the tests module of `crates/stroem-server/src/blob_storage.rs`:

```rust
    #[tokio::test]
    async fn default_open_and_read_range_slice_a_snapshot() {
        let store = InMemoryBlob::new();
        assert!(store.open("missing").await.unwrap().is_none());
        store.put("k", "t", Bytes::from_static(b"0123456789")).await.unwrap();
        let obj = store.open("k").await.unwrap().unwrap();
        assert_eq!((obj.key.as_str(), obj.size), ("k", 10));
        let mut v = Vec::new();
        store.read_range(&obj, 2, 3, &mut v).await.unwrap();
        assert_eq!(v, b"234");
        v.clear();
        store.read_range(&obj, 8, 10, &mut v).await.unwrap();
        assert_eq!(v, b"89");
        v.clear();
        store.read_range(&obj, 10, 5, &mut v).await.unwrap();
        assert!(v.is_empty());
    }

    #[tokio::test]
    async fn local_open_and_read_range() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());
        assert!(store.open("a/missing.gz").await.unwrap().is_none());
        store.put("a/obj.gz", "application/gzip", Bytes::from_static(b"0123456789")).await.unwrap();
        let obj = store.open("a/obj.gz").await.unwrap().unwrap();
        assert_eq!(obj.size, 10);
        let mut v = Vec::with_capacity(64);
        store.read_range(&obj, 2, 3, &mut v).await.unwrap();
        assert_eq!(v, b"234");
        v.clear();
        store.read_range(&obj, 8, 10, &mut v).await.unwrap();
        assert_eq!(v, b"89");
        v.clear();
        store.read_range(&obj, 12, 5, &mut v).await.unwrap();
        assert!(v.is_empty());
    }

    #[tokio::test]
    async fn local_read_range_serves_the_opened_version_after_the_path_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());
        store.put("a/obj.gz", "t", Bytes::from_static(b"old-content")).await.unwrap();
        let obj = store.open("a/obj.gz").await.unwrap().unwrap();
        store.put("a/obj.gz", "t", Bytes::from_static(b"NEW-CONTENT!!")).await.unwrap();
        let mut v = Vec::new();
        store.read_range(&obj, 0, 100, &mut v).await.unwrap();
        assert_eq!(v, b"old-content");
    }

    #[tokio::test]
    async fn read_range_rejects_an_object_opened_by_another_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());
        let foreign = ArchiveObject {
            key: "k".into(),
            size: 3,
            version: ArchiveVersion::Snapshot(Bytes::from_static(b"abc")),
        };
        assert!(store.read_range(&foreign, 0, 3, &mut Vec::new()).await.is_err());
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p stroem-server --lib blob_storage::tests`
Expected: compile error — `no method named open`.

- [ ] **Step 3: Add the types, trait methods and overrides**

In `crates/stroem-server/src/blob_storage.rs` change the tokio import to `use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};` and add after `struct Blob`:

```rust
/// An archive object opened for ranged reads, pinned to the version seen
/// at open: a replaced object is an error, never a silent mix.
#[derive(Debug)]
pub struct ArchiveObject {
    pub key: String,
    pub size: u64,
    pub version: ArchiveVersion,
}

#[derive(Debug)]
pub enum ArchiveVersion {
    /// The whole object, held by the default `open` (test and in-memory
    /// backends only).
    Snapshot(Bytes),
    /// An open descriptor: reads see the opened file even after the path
    /// is atomically replaced.
    File(tokio::sync::Mutex<fs::File>),
    /// An S3 ETag, sent as `If-Match` on every ranged GET.
    ETag(Option<String>),
}
```

Add to `trait BlobArchive`, after `get_stream`:

```rust
    /// Open `key` for ranged reads; `None` when it does not exist. The
    /// default holds the whole object in memory, which is acceptable only
    /// for test and in-memory backends; the local and S3 backends override.
    async fn open(&self, key: &str) -> Result<Option<ArchiveObject>> {
        Ok(self.get(key).await?.map(|blob| ArchiveObject {
            key: key.to_string(),
            size: blob.bytes.len() as u64,
            version: ArchiveVersion::Snapshot(blob.bytes),
        }))
    }

    /// Append `[offset, min(offset + len, size))` of the opened object to
    /// `into`: never more than `len` bytes, nothing when `offset >= size`.
    /// Callers preallocate `into`; implementations must not grow it beyond
    /// `len` bytes of new content.
    async fn read_range(
        &self,
        obj: &ArchiveObject,
        offset: u64,
        len: u64,
        into: &mut Vec<u8>,
    ) -> Result<()> {
        let ArchiveVersion::Snapshot(bytes) = &obj.version else {
            anyhow::bail!("read_range: object {} was not opened by this backend", obj.key);
        };
        let size = bytes.len() as u64;
        if offset >= size {
            return Ok(());
        }
        let end = offset.saturating_add(len).min(size);
        into.extend_from_slice(&bytes[offset as usize..end as usize]);
        Ok(())
    }
```

Add to `impl BlobArchive for LocalBlobArchive`:

```rust
    async fn open(&self, key: &str) -> Result<Option<ArchiveObject>> {
        let path = self.path_for(key)?;
        let file = match fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
        };
        let size = file
            .metadata()
            .await
            .with_context(|| format!("stat {}", path.display()))?
            .len();
        Ok(Some(ArchiveObject {
            key: key.to_string(),
            size,
            version: ArchiveVersion::File(tokio::sync::Mutex::new(file)),
        }))
    }

    async fn read_range(
        &self,
        obj: &ArchiveObject,
        offset: u64,
        len: u64,
        into: &mut Vec<u8>,
    ) -> Result<()> {
        let ArchiveVersion::File(file) = &obj.version else {
            anyhow::bail!("LocalBlobArchive::read_range: {} was not opened by this backend", obj.key);
        };
        if offset >= obj.size {
            return Ok(());
        }
        let len = len.min(obj.size - offset);
        let mut file = file.lock().await;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        (&mut *file)
            .take(len)
            .read_to_end(into)
            .await
            .with_context(|| format!("read {} bytes of {} at {offset}", len, obj.key))?;
        Ok(())
    }
```

Add to the `s3_blob_archive` module's imports `use aws_sdk_s3::operation::head_object::HeadObjectError;`, and to `impl BlobArchive for S3BlobArchive`:

```rust
        async fn open(&self, key: &str) -> Result<Option<ArchiveObject>> {
            match self.client.head_object().bucket(&self.bucket).key(key).send().await {
                Ok(out) => Ok(Some(ArchiveObject {
                    key: key.to_string(),
                    size: u64::try_from(out.content_length().unwrap_or(0)).unwrap_or(0),
                    version: ArchiveVersion::ETag(out.e_tag().map(str::to_string)),
                })),
                Err(e) => {
                    if matches!(e.as_service_error(), Some(HeadObjectError::NotFound(_))) {
                        return Ok(None);
                    }
                    Err(anyhow::anyhow!(e)).context(format!("S3 HEAD {key}"))
                }
            }
        }

        async fn read_range(
            &self,
            obj: &ArchiveObject,
            offset: u64,
            len: u64,
            into: &mut Vec<u8>,
        ) -> Result<()> {
            let ArchiveVersion::ETag(etag) = &obj.version else {
                anyhow::bail!("S3BlobArchive::read_range: {} was not opened by this backend", obj.key);
            };
            if offset >= obj.size || len == 0 {
                return Ok(());
            }
            let want = len.min(obj.size - offset);
            let mut req = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(&obj.key)
                .range(format!("bytes={offset}-{}", offset + want - 1));
            if let Some(etag) = etag {
                req = req.if_match(etag);
            }
            let out = match req.send().await {
                Ok(out) => out,
                Err(e) => {
                    match e.raw_response().map(|r| r.status().as_u16()) {
                        Some(416) => return Ok(()),
                        Some(412) => anyhow::bail!(
                            "S3 object {} changed since it was opened (If-Match failed)",
                            obj.key
                        ),
                        _ => {}
                    }
                    return Err(anyhow::anyhow!(e)).context(format!("S3 ranged GET {}", obj.key));
                }
            };
            // Read through the SDK's async adapter into the caller's buffer:
            // no `collect()`, so no segment list and no contiguous copy.
            let reader = tokio::io::AsyncReadExt::take(out.body.into_async_read(), want);
            let mut reader = std::pin::pin!(reader);
            tokio::io::AsyncReadExt::read_to_end(&mut reader, into)
                .await
                .with_context(|| format!("S3 read range of {}", obj.key))?;
            Ok(())
        }
```

- [ ] **Step 4: Write the failing range-reader tests**

`crates/stroem-server/src/log_read/archive.rs`:

```rust
//! Terminal-job archive reads: ranged gzip decoding, tails, streams, merge.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_storage::LocalBlobArchive;
    use tokio::io::AsyncReadExt;

    pub(crate) fn gzip(data: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    pub(crate) async fn put(
        dir: &tempfile::TempDir,
        bytes: Vec<u8>,
    ) -> (Arc<dyn BlobArchive>, Arc<ArchiveObject>) {
        let store: Arc<dyn BlobArchive> = Arc::new(LocalBlobArchive::new(dir.path().to_path_buf()));
        store.put("ws/t/obj.jsonl.gz", "application/gzip", bytes.into()).await.unwrap();
        let obj = store.open("ws/t/obj.jsonl.gz").await.unwrap().unwrap();
        (store, Arc::new(obj))
    }

    #[tokio::test]
    async fn range_reader_reads_the_whole_object_in_ranges() {
        for data in [&b"0123456789"[..], &b"012345678"[..], &b""[..]] {
            let dir = tempfile::TempDir::new().unwrap();
            let (store, obj) = put(&dir, data.to_vec()).await;
            let mut reader = ArchiveRangeReader::with_range(store, obj, 3);
            let mut out = Vec::new();
            reader.read_to_end(&mut out).await.unwrap();
            assert_eq!(out, data);
            assert_eq!(reader.range_buffer_capacity(), 3 + READ_SLACK, "range buffer must be reused");
        }
    }

    #[tokio::test]
    async fn gzip_isize_reads_the_trailer() {
        let dir = tempfile::TempDir::new().unwrap();
        let (store, obj) = put(&dir, gzip(b"hello world")).await;
        assert_eq!(gzip_isize(store.as_ref(), &obj).await.unwrap(), 11);

        let dir2 = tempfile::TempDir::new().unwrap();
        let (store2, obj2) = put(&dir2, b"short".to_vec()).await;
        assert!(gzip_isize(store2.as_ref(), &obj2).await.is_err());
    }
}
```

Add `pub(crate) mod archive;` to `log_read/mod.rs`.

- [ ] **Step 5: Implement the range reader and `gzip_isize`**

Insert above the tests module in `archive.rs`:

```rust
use super::{RANGE, READ_SLACK};
use crate::blob_storage::{ArchiveObject, BlobArchive};
use anyhow::Context as _;
use futures_util::future::BoxFuture;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};

/// `AsyncBufRead` over an opened archive object, fetched in ranges of
/// `range` bytes into ONE buffer that moves into each fetch and back, so a
/// finished range never coexists with the next.
pub(crate) struct ArchiveRangeReader {
    archive: Arc<dyn BlobArchive>,
    obj: Arc<ArchiveObject>,
    range: u64,
    pos: u64,
    buf: Vec<u8>,
    consumed: usize,
    fetch: Option<BoxFuture<'static, anyhow::Result<Vec<u8>>>>,
}

impl ArchiveRangeReader {
    pub(crate) fn new(archive: Arc<dyn BlobArchive>, obj: Arc<ArchiveObject>) -> Self {
        Self::with_range(archive, obj, RANGE)
    }

    pub(crate) fn with_range(archive: Arc<dyn BlobArchive>, obj: Arc<ArchiveObject>, range: u64) -> Self {
        Self {
            archive,
            obj,
            range,
            pos: 0,
            buf: Vec::with_capacity(range as usize + READ_SLACK),
            consumed: 0,
            fetch: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn range_buffer_capacity(&self) -> usize {
        self.buf.capacity()
    }
}

impl AsyncBufRead for ArchiveRangeReader {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        if this.consumed < this.buf.len() {
            return Poll::Ready(Ok(&this.buf[this.consumed..]));
        }
        if this.fetch.is_none() {
            if this.pos >= this.obj.size {
                return Poll::Ready(Ok(&[]));
            }
            let mut buf = std::mem::take(&mut this.buf);
            buf.clear();
            this.consumed = 0;
            let (archive, obj, pos, range) =
                (Arc::clone(&this.archive), Arc::clone(&this.obj), this.pos, this.range);
            this.fetch = Some(Box::pin(async move {
                archive.read_range(&obj, pos, range, &mut buf).await?;
                Ok(buf)
            }));
        }
        let fetch = this.fetch.as_mut().expect("fetch was just set");
        let result = ready!(fetch.as_mut().poll(cx));
        this.fetch = None;
        match result {
            Ok(buf) if buf.is_empty() => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("archive object {} is shorter than its size", this.obj.key),
            ))),
            Ok(buf) => {
                this.pos += buf.len() as u64;
                this.buf = buf;
                Poll::Ready(Ok(&this.buf[..]))
            }
            Err(e) => Poll::Ready(Err(io::Error::other(e))),
        }
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.get_mut();
        this.consumed = (this.consumed + amt).min(this.buf.len());
    }
}

impl AsyncRead for ArchiveRangeReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let avail = ready!(Pin::new(&mut *this).poll_fill_buf(cx))?;
        let n = avail.len().min(out.remaining());
        out.put_slice(&avail[..n]);
        this.consumed += n;
        Poll::Ready(Ok(()))
    }
}

/// The decompressed length from a gzip member's trailer (ISIZE, mod 2^32).
/// Our uploads are one member (`GzEncoder` … `finish()`), so this is exact
/// under 4 GiB; callers still bound every decompression with `take`.
pub(crate) async fn gzip_isize(archive: &dyn BlobArchive, obj: &ArchiveObject) -> anyhow::Result<u64> {
    anyhow::ensure!(
        obj.size >= 18,
        "archive object {} is {} bytes, too short to be gzip",
        obj.key,
        obj.size
    );
    let mut trailer = Vec::with_capacity(4 + READ_SLACK);
    archive.read_range(obj, obj.size - 4, 4, &mut trailer).await?;
    let bytes: [u8; 4] = trailer
        .as_slice()
        .try_into()
        .with_context(|| format!("short gzip trailer read from {}", obj.key))?;
    Ok(u64::from(u32::from_le_bytes(bytes)))
}
```

- [ ] **Step 6: Add the S3 test**

Append to `crates/stroem-server/tests/s3_integration_test.rs`:

```rust
#[tokio::test]
async fn s3_open_and_read_range_are_bounded_and_version_pinned() -> Result<()> {
    let (_container, endpoint) = setup_minio().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = test_s3_client(&endpoint);
    create_bucket(&client, &bucket).await?;
    let archive = S3BlobArchive::from_client(client.clone(), bucket.clone());

    assert!(archive.open("missing").await?.is_none());
    archive.put("k", "text/plain", Bytes::from_static(b"0123456789")).await?;
    let obj = archive.open("k").await?.unwrap();
    assert_eq!(obj.size, 10);

    let mut v = Vec::with_capacity(64);
    archive.read_range(&obj, 2, 3, &mut v).await?;
    assert_eq!(v, b"234");
    v.clear();
    archive.read_range(&obj, 8, 10, &mut v).await?;
    assert_eq!(v, b"89");
    v.clear();
    archive.read_range(&obj, 10, 5, &mut v).await?;
    assert!(v.is_empty());

    archive.put("k", "text/plain", Bytes::from_static(b"replaced!!")).await?;
    let err = archive.read_range(&obj, 0, 5, &mut Vec::new()).await.unwrap_err();
    assert!(format!("{err:#}").contains("changed since it was opened"), "{err:#}");
    Ok(())
}
```

- [ ] **Step 7: Run all the new tests**

Run: `cargo test -p stroem-server --lib blob_storage::tests && cargo test -p stroem-server --lib log_read::archive && cargo test -p stroem-server --test s3_integration_test s3_open_and_read_range`
Expected: PASS (the S3 test needs Docker).

- [ ] **Step 8: Commit**

```bash
git add crates/stroem-server/src/blob_storage.rs crates/stroem-server/src/log_read crates/stroem-server/tests/s3_integration_test.rs
git commit -m "feat(logs): version-pinned ranged archive reads and gzip trailer size"
```

---

### Task 7: Archive tails, archive stream and the bounded merge

**Files:**
- Modify: `crates/stroem-server/src/log_read/archive.rs`
- Modify: `crates/stroem-server/Cargo.toml` (add `async-compression = { version = "0.4", features = ["tokio", "gzip"] }`)

**Interfaces:**
- Consumes: `ArchiveRangeReader`, `gzip_isize` (Task 6); `LineSplitter`, `LineRef`, `filtered_stream` (Task 4); `LocalFile`, `LocalKind`, `read_range_exact` (Task 5); `count_lines`, `trim_torn`, `retain_matching_lines` (Task 3); `crate::log_storage::merge_jsonl_logs`; `LogReadConfig` (Task 1).
- Produces: `pub(crate) struct ArchiveTail { pub logs: String, pub truncated: bool, pub decompressed: u64 }`; `pub(crate) async fn tail(Arc<dyn BlobArchive>, Arc<ArchiveObject>, filter: Option<&str>, t: u64, max_line: usize) -> anyhow::Result<ArchiveTail>`; `pub(crate) fn full_stream(Arc<dyn BlobArchive>, Arc<ArchiveObject>, step: Option<String>, max_line: usize) -> BoxStream<'static, io::Result<Bytes>>`; `pub(crate) async fn read_merged_full(local: &mut LocalFile, Arc<dyn BlobArchive>, Arc<ArchiveObject>, filter: Option<&str>, cfg: &LogReadConfig) -> anyhow::Result<Option<String>>`.

- [ ] **Step 1: Write the failing tests**

Append inside the tests module of `archive.rs`:

```rust
    use crate::config::LogReadConfig;
    use crate::log_read::local::{open_local, LocalKind};
    use futures_util::TryStreamExt;

    fn jl(step: &str, msg: &str) -> String {
        format!(r#"{{"ts":"2026-09-22T00:00:0{}Z","step":"{step}","line":"{msg}"}}"#, msg.len() % 10)
    }

    fn lines(step: &str, n: usize) -> String {
        (0..n).map(|i| format!("{}\n", jl(step, &format!("m{i:04}")))).collect()
    }

    #[tokio::test]
    async fn unfiltered_tail_keeps_the_newest_whole_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let content = lines("s", 10);
        let (store, obj) = put(&dir, gzip(content.as_bytes())).await;
        let per = content.lines().next().unwrap().len() as u64 + 1;
        let t = tail(store.clone(), obj.clone(), None, 3 * per + 5, 1024).await.unwrap();
        let expected: String = content.lines().skip(7).map(|l| format!("{l}\n")).collect();
        assert_eq!(t.logs, expected);
        assert!(t.truncated);
        assert_eq!(t.decompressed, content.len() as u64);

        let whole = tail(store.clone(), obj.clone(), None, 1 << 20, 1024).await.unwrap();
        assert_eq!((whole.logs, whole.truncated), (content.clone(), false));

        let exact = tail(store, obj, None, 3 * per, 1024).await.unwrap();
        assert_eq!(exact.logs, expected, "a window that starts on a line boundary keeps that line");
    }

    #[tokio::test]
    async fn unfiltered_tail_keeps_a_long_line_the_filtered_cap_would_drop() {
        let dir = tempfile::TempDir::new().unwrap();
        let long = format!("{}\n", "x".repeat(2 << 20));
        let (store, obj) = put(&dir, gzip(long.as_bytes())).await;
        let t = tail(store, obj, None, 4 << 20, 1 << 20).await.unwrap();
        assert_eq!((t.logs.len(), t.truncated), (long.len(), false));
    }

    #[tokio::test]
    async fn a_torn_archive_line_is_dropped_and_flagged() {
        let dir = tempfile::TempDir::new().unwrap();
        let (store, obj) = put(&dir, gzip(b"a\nb\nhalf")).await;
        let t = tail(store, obj, None, 1 << 20, 1024).await.unwrap();
        assert_eq!((t.logs.as_str(), t.truncated), ("a\nb\n", true));
    }

    #[tokio::test]
    async fn filtered_tail_keeps_the_newest_matching_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut content = String::new();
        for i in 0..20 {
            content.push_str(&jl(if i % 2 == 0 { "a" } else { "b" }, &format!("m{i:04}")));
            content.push('\n');
        }
        let (store, obj) = put(&dir, gzip(content.as_bytes())).await;
        let a_lines: Vec<&str> = content.lines().filter(|l| l.contains(r#""step":"a""#)).collect();
        let per = a_lines[0].len() as u64 + 1;
        let t = tail(store.clone(), obj.clone(), Some("a"), 2 * per, 1024).await.unwrap();
        assert_eq!(t.logs, format!("{}\n{}\n", a_lines[8], a_lines[9]));
        assert!(t.truncated, "older matches were evicted");

        let all = tail(store, obj, Some("a"), 1 << 20, 1024).await.unwrap();
        assert_eq!(all.logs.lines().count(), 10);
        assert!(!all.truncated);
    }

    #[tokio::test]
    async fn filtered_tail_flags_skipped_and_torn_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let content = format!("{}\n{}\n{}", jl("a", &"y".repeat(200)), jl("a", "ok"), r#"{"step":"a","li"#);
        let (store, obj) = put(&dir, gzip(content.as_bytes())).await;
        let t = tail(store, obj, Some("a"), 1 << 20, 100).await.unwrap();
        assert_eq!(t.logs, format!("{}\n", jl("a", "ok")));
        assert!(t.truncated);
    }

    #[tokio::test]
    async fn a_corrupt_object_is_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let (store, obj) = put(&dir, b"this is not gzip data at all".to_vec()).await;
        assert!(tail(store, obj, None, 1024, 1024).await.is_err());
    }

    #[tokio::test]
    async fn full_stream_decompresses_and_filters() {
        let dir = tempfile::TempDir::new().unwrap();
        let content = format!("{}{}", lines("a", 3), lines("b", 2));
        let (store, obj) = put(&dir, gzip(content.as_bytes())).await;
        let all: Vec<Bytes> = full_stream(store.clone(), obj.clone(), None, 1024).try_collect().await.unwrap();
        assert_eq!(all.concat(), content.as_bytes());
        let a: Vec<Bytes> = full_stream(store, obj, Some("a".into()), 1024).try_collect().await.unwrap();
        assert_eq!(String::from_utf8(a.concat()).unwrap(), lines("a", 3));
    }

    async fn local_with(dir: &tempfile::TempDir, kind: LocalKind, content: &str) -> LocalFile {
        let (jsonl, legacy) = (dir.path().join("l.jsonl"), dir.path().join("l.log"));
        tokio::fs::write(if kind == LocalKind::Jsonl { &jsonl } else { &legacy }, content).await.unwrap();
        open_local(&jsonl, &legacy).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn merged_full_unions_both_sources_under_the_caps() {
        let (d1, d2) = (tempfile::TempDir::new().unwrap(), tempfile::TempDir::new().unwrap());
        let (only_local, shared, only_archive) = (jl("s", "local"), jl("s", "shared"), jl("s", "archive"));
        let mut local = local_with(&d1, LocalKind::Jsonl, &format!("{only_local}\n{shared}\n")).await;
        let (store, obj) = put(&d2, gzip(format!("{shared}\n{only_archive}\n").as_bytes())).await;
        let merged = read_merged_full(&mut local, store, obj, None, &LogReadConfig::default())
            .await
            .unwrap()
            .expect("under both caps");
        for l in [&only_local, &shared, &only_archive] {
            assert_eq!(merged.matches(l.as_str()).count(), 1, "{l}");
        }
    }

    #[tokio::test]
    async fn merged_full_filters_by_step() {
        let (d1, d2) = (tempfile::TempDir::new().unwrap(), tempfile::TempDir::new().unwrap());
        let mut local = local_with(&d1, LocalKind::Jsonl, &format!("{}\n{}\n", jl("a", "1"), jl("b", "2"))).await;
        let (store, obj) = put(&d2, gzip(format!("{}\n", jl("a", "3")).as_bytes())).await;
        let merged = read_merged_full(&mut local, store, obj, Some("a"), &LogReadConfig::default())
            .await
            .unwrap()
            .unwrap();
        assert!(merged.contains(r#""line":"1""#) && merged.contains(r#""line":"3""#));
        assert!(!merged.contains(r#""step":"b""#));
    }

    #[tokio::test]
    async fn merged_full_is_abandoned_over_either_cap_or_on_a_lying_trailer() {
        let content = lines("s", 50);
        let cfg = LogReadConfig::default();
        let over_bytes = LogReadConfig { merge_max_bytes: 100, max_line_bytes: 100, ..cfg };
        let over_lines = LogReadConfig { merge_max_lines: 10, ..cfg };
        for c in [over_bytes, over_lines] {
            let (d1, d2) = (tempfile::TempDir::new().unwrap(), tempfile::TempDir::new().unwrap());
            let mut local = local_with(&d1, LocalKind::Jsonl, &content).await;
            let (store, obj) = put(&d2, gzip(content.as_bytes())).await;
            assert!(read_merged_full(&mut local, store, obj, None, &c).await.unwrap().is_none());
        }
        // A trailer claiming 3 bytes for a 50-line body.
        let (d1, d2) = (tempfile::TempDir::new().unwrap(), tempfile::TempDir::new().unwrap());
        let mut local = local_with(&d1, LocalKind::Jsonl, "x\n").await;
        let mut lying = gzip(content.as_bytes());
        let n = lying.len();
        lying[n - 4..].copy_from_slice(&3u32.to_le_bytes());
        let (store, obj) = put(&d2, lying).await;
        assert!(read_merged_full(&mut local, store, obj, None, &cfg).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn merged_full_keeps_a_legacy_unterminated_line_and_trims_torn_jsonl() {
        let (d1, d2) = (tempfile::TempDir::new().unwrap(), tempfile::TempDir::new().unwrap());
        let mut local = local_with(&d1, LocalKind::Legacy, "legacy tail").await;
        let (store, obj) = put(&d2, gzip(format!("{}\nhalf", jl("s", "arch")).as_bytes())).await;
        let merged = read_merged_full(&mut local, store, obj, None, &LogReadConfig::default())
            .await
            .unwrap()
            .unwrap();
        assert!(merged.contains("legacy tail\n"));
        assert!(!merged.contains("half"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p stroem-server --lib log_read::archive`
Expected: compile errors — `cannot find function tail`.

- [ ] **Step 3: Implement**

Add the dependency to `crates/stroem-server/Cargo.toml` under `[dependencies]`:

```toml
async-compression = { version = "0.4", features = ["tokio", "gzip"] }
```

Extend the imports at the top of `archive.rs`:

```rust
use super::local::{read_range_exact, LocalFile, LocalKind};
use super::splitter::{filtered_stream, LineRef, LineSplitter};
use super::{count_lines, matcher, retain_matching_lines, trim_torn, CHUNK};
use crate::config::LogReadConfig;
use crate::log_storage::merge_jsonl_logs;
use async_compression::tokio::bufread::GzipDecoder;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use std::collections::VecDeque;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio_util::io::ReaderStream;
```

and insert below `gzip_isize`:

```rust
pub(crate) struct ArchiveTail {
    pub logs: String,
    pub truncated: bool,
    /// Decompressed bytes counted while streaming (never the trailer).
    pub decompressed: u64,
}

fn decoder(archive: Arc<dyn BlobArchive>, obj: Arc<ArchiveObject>) -> GzipDecoder<ArchiveRangeReader> {
    GzipDecoder::new(ArchiveRangeReader::new(archive, obj))
}

/// The newest `t` bytes of the archived log (whole lines), or of its lines
/// for one step. Computed completely before the caller answers, so any
/// archive error can still fall back to local.
pub(crate) async fn tail(
    archive: Arc<dyn BlobArchive>,
    obj: Arc<ArchiveObject>,
    filter: Option<&str>,
    t: u64,
    max_line: usize,
) -> anyhow::Result<ArchiveTail> {
    let reader = BufReader::with_capacity(CHUNK, decoder(archive, obj));
    Ok(match filter {
        None => byte_ring_tail(reader, t).await?,
        Some(step) => line_ring_tail(reader, step, t, max_line).await?,
    })
}

/// A ring of the last `t` decompressed bytes. No line splitter, so a line
/// of any length survives exactly as it does from the local file.
async fn byte_ring_tail<R: tokio::io::AsyncBufRead + Unpin>(mut reader: R, t: u64) -> io::Result<ArchiveTail> {
    let cap = usize::try_from(t).map_err(io::Error::other)?;
    let mut ring: VecDeque<u8> = VecDeque::with_capacity(cap);
    let mut total = 0u64;
    // The byte just before the ring's first byte, once anything was evicted.
    let mut before_ring: Option<u8> = None;
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            break;
        }
        let n = buf.len();
        total += n as u64;
        if n >= cap {
            before_ring = if n > cap { Some(buf[n - cap - 1]) } else { ring.back().copied().or(before_ring) };
            ring.clear();
            ring.extend(buf[n - cap..].iter().copied());
        } else {
            let overflow = (ring.len() + n).saturating_sub(cap);
            if overflow > 0 {
                before_ring = ring.get(overflow - 1).copied();
                ring.drain(..overflow);
            }
            ring.extend(buf.iter().copied());
        }
        reader.consume(n);
    }
    let evicted = total > ring.len() as u64;
    let mut v = Vec::from(ring); // never reallocates
    let head = if evicted && before_ring != Some(b'\n') {
        v.iter().position(|&b| b == b'\n').map_or(v.len(), |p| p + 1)
    } else {
        0
    };
    v.drain(..head);
    let torn = trim_torn(&mut v);
    let logs = String::from_utf8(v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(ArchiveTail { logs, truncated: evicted || torn, decompressed: total })
}

/// A ring of whole matching lines, at most `t` bytes, oldest evicted first.
async fn line_ring_tail<R: tokio::io::AsyncBufRead + Unpin>(
    reader: R,
    step: &str,
    t: u64,
    max_line: usize,
) -> io::Result<ArchiveTail> {
    let cap = usize::try_from(t).map_err(io::Error::other)?;
    let mut ring: VecDeque<u8> = VecDeque::with_capacity(cap);
    let mut split = LineSplitter::new(reader, max_line);
    let mut truncated = false;
    loop {
        match split.next().await? {
            None => break,
            // Oversize, or a torn record at the end of a .jsonl snapshot.
            Some(LineRef::Skipped { .. }) | Some(LineRef::Unterminated(_)) => truncated = true,
            Some(LineRef::Line(line)) => {
                if line.is_empty() || !matcher::matches_bytes(line, step) {
                    continue;
                }
                let need = line.len() + 1;
                if need > cap {
                    truncated = true;
                    continue;
                }
                if ring.len() + need > cap {
                    let must_free = ring.len() + need - cap;
                    let cut = ring
                        .iter()
                        .skip(must_free - 1)
                        .position(|&b| b == b'\n')
                        .map_or(ring.len(), |p| must_free + p);
                    ring.drain(..cut);
                    truncated = true;
                }
                ring.extend(line.iter().copied());
                ring.push_back(b'\n');
            }
        }
    }
    let decompressed = split.bytes_consumed();
    let logs = String::from_utf8(Vec::from(ring)).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(ArchiveTail { logs, truncated, decompressed })
}

/// The archived log as a stream, served as stored (no torn-line trim:
/// the end is only known after it was sent).
pub(crate) fn full_stream(
    archive: Arc<dyn BlobArchive>,
    obj: Arc<ArchiveObject>,
    step: Option<String>,
    max_line: usize,
) -> BoxStream<'static, io::Result<Bytes>> {
    let decoder = decoder(archive, obj);
    match step {
        None => Box::pin(ReaderStream::with_capacity(decoder, CHUNK)),
        Some(step) => filtered_stream(BufReader::with_capacity(CHUNK, decoder), step, max_line, false),
    }
}

/// The union of the local snapshot and the archive, when both fit the caps.
/// `Ok(None)` means "over a cap, or the trailer disagrees with the body":
/// the caller streams a single source instead. Nothing is sent before this
/// returns, so every failure here can still fall back.
pub(crate) async fn read_merged_full(
    local: &mut LocalFile,
    archive: Arc<dyn BlobArchive>,
    obj: Arc<ArchiveObject>,
    filter: Option<&str>,
    cfg: &LogReadConfig,
) -> anyhow::Result<Option<String>> {
    let isize = gzip_isize(archive.as_ref(), &obj).await?;
    if local.len.saturating_add(isize) > cfg.merge_max_bytes {
        return Ok(None);
    }
    let mut l = Vec::with_capacity(usize::try_from(local.len)? + READ_SLACK);
    read_range_exact(&mut local.file, 0, local.len, &mut l)
        .await
        .context("read local log for merge")?;
    let mut a = Vec::with_capacity(usize::try_from(isize)? + 1 + READ_SLACK);
    let limited = decoder(archive, obj).take(isize + 1);
    let mut limited = std::pin::pin!(limited);
    let n = limited.read_to_end(&mut a).await.context("decompress archive for merge")?;
    if n as u64 != isize {
        return Ok(None); // wrong, wrapped or stale trailer
    }
    if local.kind == LocalKind::Jsonl {
        trim_torn(&mut l);
    }
    trim_torn(&mut a);
    if let Some(step) = filter {
        retain_matching_lines(&mut l, step, cfg.max_line_bytes);
        retain_matching_lines(&mut a, step, cfg.max_line_bytes);
    }
    if count_lines(&l) + count_lines(&a) > cfg.merge_max_lines {
        return Ok(None);
    }
    let l = String::from_utf8(l).context("local log is not UTF-8")?;
    let a = String::from_utf8(a).context("archived log is not UTF-8")?;
    Ok(Some(merge_jsonl_logs(&l, &a)))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p stroem-server --lib log_read::archive`
Expected: 13 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.lock crates/stroem-server/Cargo.toml crates/stroem-server/src/log_read/archive.rs
git commit -m "feat(logs): bounded archive tails, archive stream and capped merge"
```

### Task 8: `LogStorage::read_tail` and `stream_full`

**Files:**
- Modify: `crates/stroem-server/src/log_storage.rs` (new methods, `combine_tails`, tests)

**Interfaces:**
- Consumes: everything in `crate::log_read` from Tasks 2–7.
- Produces: `pub async fn read_tail(&self, job_id: Uuid, meta: &JobLogMeta, is_terminal: bool, filter: StepFilter<'_>, tail_bytes: u64) -> anyhow::Result<Tail>`; `pub async fn stream_full(&self, job_id: Uuid, meta: &JobLogMeta, is_terminal: bool, filter: StepFilter<'_>) -> anyhow::Result<(LogSource, BoxStream<'static, std::io::Result<Bytes>>)>`.

- [ ] **Step 1: Write the failing tests**

Append to the tests module of `crates/stroem-server/src/log_storage.rs`:

```rust
    // ─── read_tail / stream_full (spec § 3.2–3.3) ────────────────────────

    use crate::log_read::{LogSource, StepFilter};

    const ALL: u64 = 16 * 1024 * 1024;

    async fn full_text(
        storage: &LogStorage,
        job: Uuid,
        meta: &JobLogMeta,
        terminal: bool,
        filter: StepFilter<'_>,
    ) -> (LogSource, String) {
        use futures_util::TryStreamExt;
        let (source, stream) = storage.stream_full(job, meta, terminal, filter).await.unwrap();
        let chunks: Vec<Bytes> = stream.try_collect().await.unwrap();
        (source, String::from_utf8(chunks.concat()).unwrap())
    }

    fn mocked(tmp: &TempDir) -> (Arc<MockArchive>, LogStorage) {
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(tmp.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn BlobArchive>, String::new());
        (mock, storage)
    }

    async fn seed_archive(mock: &MockArchive, job: Uuid, meta: &JobLogMeta, content: &str) {
        mock.put(&archive_key("", job, meta), "application/gzip", Bytes::from(gzip(content)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn read_tail_non_terminal_never_touches_the_archive() {
        let tmp = TempDir::new().unwrap();
        let (mock, storage) = mocked(&tmp);
        let (job, meta) = (Uuid::new_v4(), test_meta());
        let local = format!("{}\n", ts_line("2026-06-10T05:00:01Z", "s", "local"));
        storage.append_log(job, &local).await.unwrap();
        seed_archive(&mock, job, &meta, &format!("{}\n", ts_line("2026-06-10T05:00:02Z", "s", "arch"))).await;
        let tail = storage.read_tail(job, &meta, false, StepFilter::All, ALL).await.unwrap();
        assert_eq!((tail.logs, tail.source), (local, LogSource::Local));
        assert_eq!(mock.get_call_count().await, 0);
    }

    #[tokio::test]
    async fn read_tail_terminal_merges_both_tails() {
        let tmp = TempDir::new().unwrap();
        let (mock, storage) = mocked(&tmp);
        let (job, meta) = (Uuid::new_v4(), test_meta());
        let local = format!("{}\n", ts_line("2026-06-10T05:00:01Z", "s", "from-local"));
        let arch = format!("{}\n", ts_line("2026-06-10T05:00:02Z", "s", "from-archive"));
        storage.append_log(job, &local).await.unwrap();
        seed_archive(&mock, job, &meta, &arch).await;
        let tail = storage.read_tail(job, &meta, true, StepFilter::All, ALL).await.unwrap();
        assert_eq!(tail.logs, format!("{local}{arch}"));
        assert_eq!(tail.source, LogSource::Merged);
        assert!(!tail.truncated);
        assert_eq!(tail.total_bytes, (local.len() + arch.len() + 2) as u64);
        assert!(tail.returned_bytes() <= tail.total_bytes);
        assert_eq!(mock.get_call_count().await, 1, "one archive fetch");
    }

    #[tokio::test]
    async fn read_tail_terminal_cuts_the_union_back_to_the_budget() {
        let tmp = TempDir::new().unwrap();
        let (mock, storage) = mocked(&tmp);
        let (job, meta) = (Uuid::new_v4(), test_meta());
        let (a, b) = (
            ts_line("2026-06-10T05:00:01Z", "s", "older"),
            ts_line("2026-06-10T05:00:02Z", "s", "newer"),
        );
        storage.append_log(job, &format!("{a}\n")).await.unwrap();
        seed_archive(&mock, job, &meta, &format!("{b}\n")).await;
        let tail = storage
            .read_tail(job, &meta, true, StepFilter::All, b.len() as u64 + 1)
            .await
            .unwrap();
        assert_eq!((tail.logs, tail.truncated), (format!("{b}\n"), true));
    }

    #[tokio::test]
    async fn read_tail_terminal_degrades_and_falls_back() {
        // Archive error → local.
        let tmp = TempDir::new().unwrap();
        let storage = LogStorage::new(tmp.path())
            .with_archive(Arc::new(ErroringArchive) as Arc<dyn BlobArchive>, String::new());
        let (job, meta) = (Uuid::new_v4(), test_meta());
        storage.append_log(job, "a\n").await.unwrap();
        let tail = storage.read_tail(job, &meta, true, StepFilter::All, ALL).await.unwrap();
        assert_eq!((tail.logs.as_str(), tail.source), ("a\n", LogSource::Local));

        // Local missing → archive; neither → empty.
        let tmp2 = TempDir::new().unwrap();
        let (mock, storage2) = mocked(&tmp2);
        seed_archive(&mock, job, &meta, "b\n").await;
        let tail = storage2.read_tail(job, &meta, true, StepFilter::All, ALL).await.unwrap();
        assert_eq!((tail.logs.as_str(), tail.source), ("b\n", LogSource::Archive));
        let none = storage2.read_tail(Uuid::new_v4(), &meta, true, StepFilter::All, ALL).await.unwrap();
        assert_eq!(none, crate::log_read::Tail::empty());
    }

    #[tokio::test]
    async fn read_tail_serves_local_when_the_union_exceeds_the_line_cap() {
        let tmp = TempDir::new().unwrap();
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(tmp.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn BlobArchive>, String::new())
            .with_read_config(crate::config::LogReadConfig { merge_max_lines: 1, ..Default::default() });
        let (job, meta) = (Uuid::new_v4(), test_meta());
        storage.append_log(job, "a\n").await.unwrap();
        seed_archive(&mock, job, &meta, "b\n").await;
        let tail = storage.read_tail(job, &meta, true, StepFilter::All, ALL).await.unwrap();
        assert_eq!((tail.logs.as_str(), tail.source, tail.truncated), ("a\n", LogSource::Local, true));
        assert_eq!(tail.total_bytes, 2 + 2 + 2);
    }

    #[tokio::test]
    async fn stream_full_chooses_merged_local_archive_or_none() {
        let meta = test_meta();
        let (a, b) = (
            ts_line("2026-06-10T05:00:01Z", "s", "local"),
            ts_line("2026-06-10T05:00:02Z", "s", "arch"),
        );
        // Under the caps → merged.
        let tmp = TempDir::new().unwrap();
        let (mock, storage) = mocked(&tmp);
        let job = Uuid::new_v4();
        storage.append_log(job, &format!("{a}\n")).await.unwrap();
        seed_archive(&mock, job, &meta, &format!("{b}\n")).await;
        assert_eq!(
            full_text(&storage, job, &meta, true, StepFilter::All).await,
            (LogSource::Merged, format!("{a}\n{b}\n"))
        );
        // Over the byte cap with local present → local.
        let small = storage.clone().with_read_config(crate::config::LogReadConfig {
            merge_max_bytes: 8,
            max_line_bytes: 8,
            ..Default::default()
        });
        assert_eq!(
            full_text(&small, job, &meta, true, StepFilter::All).await,
            (LogSource::Local, format!("{a}\n"))
        );
        // Over the cap, local missing → archive.
        tokio::fs::remove_file(storage.log_path(job)).await.unwrap();
        assert_eq!(
            full_text(&small, job, &meta, true, StepFilter::All).await,
            (LogSource::Archive, format!("{b}\n"))
        );
        // Neither → none.
        assert_eq!(
            full_text(&storage, Uuid::new_v4(), &meta, true, StepFilter::All).await,
            (LogSource::None, String::new())
        );
        // Non-terminal never merges.
        let job2 = Uuid::new_v4();
        storage.append_log(job2, &format!("{a}\n")).await.unwrap();
        seed_archive(&mock, job2, &meta, &format!("{b}\n")).await;
        assert_eq!(
            full_text(&storage, job2, &meta, false, StepFilter::All).await,
            (LogSource::Local, format!("{a}\n"))
        );
    }

    #[tokio::test]
    async fn stream_full_archive_error_falls_back_to_local() {
        let tmp = TempDir::new().unwrap();
        let storage = LogStorage::new(tmp.path())
            .with_archive(Arc::new(ErroringArchive) as Arc<dyn BlobArchive>, String::new());
        let (job, meta) = (Uuid::new_v4(), test_meta());
        storage.append_log(job, "a\n").await.unwrap();
        assert_eq!(
            full_text(&storage, job, &meta, true, StepFilter::All).await,
            (LogSource::Local, "a\n".to_string())
        );
    }

    #[tokio::test]
    async fn step_filter_applies_to_both_modes() {
        let tmp = TempDir::new().unwrap();
        let (_mock, storage) = mocked(&tmp);
        let (job, meta) = (Uuid::new_v4(), test_meta());
        let (b, t) = (jsonl_line("build", "stdout", "b"), jsonl_line("test", "stdout", "t"));
        storage.append_log(job, &format!("{b}\n{t}\n")).await.unwrap();
        let tail = storage.read_tail(job, &meta, false, StepFilter::Step("build"), ALL).await.unwrap();
        assert_eq!(tail.logs, format!("{b}\n"));
        assert_eq!(
            full_text(&storage, job, &meta, false, StepFilter::Step("test")).await.1,
            format!("{t}\n")
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p stroem-server --lib log_storage::tests`
Expected: compile error — `no method named read_tail`.

- [ ] **Step 3: Implement**

Add to the imports of `log_storage.rs`:

```rust
use crate::log_read::archive::{self as archive_read, ArchiveTail};
use crate::log_read::local::{self as local_read, LocalTail};
use crate::log_read::{bytes_stream, count_lines, cut_front_to_lines, LogSource, StepFilter, Tail};
use futures_core::stream::BoxStream;
```

Add to `impl LogStorage`:

```rust
    /// A bounded read of the log's end (spec § 3.2–3.3). The local tail
    /// always; for a terminal job also the archive tail, merged with the
    /// local one when the union fits `merge_max_lines`. Archive errors
    /// degrade to local: the archive tail is complete before this returns.
    pub async fn read_tail(
        &self,
        job_id: Uuid,
        meta: &JobLogMeta,
        is_terminal: bool,
        filter: StepFilter<'_>,
        tail_bytes: u64,
    ) -> Result<Tail> {
        let read = self.read;
        let local = match local_read::open_local(&self.log_path(job_id), &self.legacy_log_path(job_id))
            .await
            .context("open local log")?
        {
            None => None,
            Some(mut f) => Some(
                match filter {
                    StepFilter::All => local_read::tail_unfiltered(&mut f, tail_bytes).await,
                    StepFilter::Step(step) => {
                        local_read::tail_step(
                            &mut f,
                            step,
                            tail_bytes,
                            read.max_line_bytes,
                            read.tail_scan_max_bytes,
                        )
                        .await
                    }
                }
                .context("read local log tail")?,
            ),
        };
        let archive = if is_terminal {
            self.archive_tail(job_id, meta, filter, tail_bytes).await
        } else {
            None
        };
        Ok(combine_tails(local, archive, tail_bytes, read.merge_max_lines))
    }

    async fn archive_tail(
        &self,
        job_id: Uuid,
        meta: &JobLogMeta,
        filter: StepFilter<'_>,
        tail_bytes: u64,
    ) -> Option<ArchiveTail> {
        let archive = self.archive.as_ref()?;
        let key = archive_key(&self.archive_prefix, job_id, meta);
        let result: Result<Option<ArchiveTail>> = async {
            let Some(obj) = archive.open(&key).await? else {
                return Ok(None);
            };
            let tail = archive_read::tail(
                Arc::clone(archive),
                Arc::new(obj),
                filter.step(),
                tail_bytes,
                self.read.max_line_bytes,
            )
            .await?;
            Ok(Some(tail))
        }
        .await;
        result.unwrap_or_else(|e| {
            tracing::warn!("archive read failed for terminal job {job_id}, serving local only: {e:#}");
            None
        })
    }

    /// The whole log as a stream (spec § 3.2–3.3). For a terminal job: the
    /// in-memory union when both sources fit the caps, else ONE source —
    /// local first, the archive only when there is no local file.
    pub async fn stream_full(
        &self,
        job_id: Uuid,
        meta: &JobLogMeta,
        is_terminal: bool,
        filter: StepFilter<'_>,
    ) -> Result<(LogSource, BoxStream<'static, std::io::Result<Bytes>>)> {
        let read = self.read;
        let step = filter.step().map(str::to_owned);
        let mut local = local_read::open_local(&self.log_path(job_id), &self.legacy_log_path(job_id))
            .await
            .context("open local log")?;
        if let (true, Some(archive)) = (is_terminal, self.archive.as_ref()) {
            let key = archive_key(&self.archive_prefix, job_id, meta);
            match archive.open(&key).await {
                Err(e) => tracing::warn!(
                    "archive open failed for terminal job {job_id}, serving local only: {e:#}"
                ),
                Ok(None) => {}
                Ok(Some(obj)) => {
                    let obj = Arc::new(obj);
                    match local.as_mut() {
                        None => {
                            let stream = archive_read::full_stream(
                                Arc::clone(archive),
                                obj,
                                step,
                                read.max_line_bytes,
                            );
                            return Ok((LogSource::Archive, stream));
                        }
                        Some(lf) => match archive_read::read_merged_full(
                            lf,
                            Arc::clone(archive),
                            obj,
                            filter.step(),
                            &read,
                        )
                        .await
                        {
                            Ok(Some(merged)) => return Ok((LogSource::Merged, bytes_stream(merged))),
                            Ok(None) => {}
                            Err(e) => tracing::warn!(
                                "merging the archive of terminal job {job_id} failed, serving local only: {e:#}"
                            ),
                        },
                    }
                }
            }
        }
        match local {
            Some(f) => Ok((
                LogSource::Local,
                local_read::full_stream(f, step, read.max_line_bytes)
                    .await
                    .context("stream local log")?,
            )),
            None => Ok((
                LogSource::None,
                Box::pin(futures_util::stream::empty::<std::io::Result<Bytes>>()),
            )),
        }
    }
```

And a free function below `archive_key`:

```rust
/// Spec § 3.3: the union of a local and an archive tail when it fits
/// `max_lines`, cut back to `t`; otherwise the single source there is.
/// `total_bytes` is an upper bound: a union is at most the sum of its
/// inputs, plus the newline each input's last line may gain.
fn combine_tails(
    local: Option<LocalTail>,
    archive: Option<ArchiveTail>,
    t: u64,
    max_lines: usize,
) -> Tail {
    match (local, archive) {
        (None, None) => Tail::empty(),
        (Some(l), None) => Tail {
            logs: l.logs,
            truncated: l.truncated,
            total_bytes: l.len,
            source: LogSource::Local,
        },
        (None, Some(a)) => Tail {
            logs: a.logs,
            truncated: a.truncated,
            total_bytes: a.decompressed,
            source: LogSource::Archive,
        },
        (Some(l), Some(a)) => {
            let total_bytes = l.len + a.decompressed + 2;
            if count_lines(l.logs.as_bytes()) + count_lines(a.logs.as_bytes()) > max_lines {
                return Tail {
                    logs: l.logs,
                    truncated: true,
                    total_bytes,
                    source: LogSource::Local,
                };
            }
            let merged = merge_jsonl_logs(&l.logs, &a.logs);
            let (logs, cut) = cut_front_to_lines(merged, t);
            Tail {
                logs,
                truncated: l.truncated || a.truncated || cut,
                total_bytes,
                source: LogSource::Merged,
            }
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p stroem-server --lib log_storage::tests`
Expected: all PASS, old and new.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/log_storage.rs
git commit -m "feat(logs): LogStorage::read_tail and stream_full with the terminal merge rules"
```

---

### Task 9: REST handlers, WebSocket backfill and MCP on the bounded reads

**Files:**
- Create: `crates/stroem-server/src/web/api/logs.rs`
- Modify: `crates/stroem-server/src/web/api/mod.rs:1-15` (`pub mod logs;`), `:315-316` (routes)
- Modify: `crates/stroem-server/src/web/api/jobs.rs:559-629` (delete the two handlers)
- Modify: `crates/stroem-server/src/web/api/ws.rs:137-173`
- Modify: `crates/stroem-server/src/mcp/tools.rs:63-71`, `:602-646`
- Test: `crates/stroem-server/tests/integration_test.rs`, `crates/stroem-server/tests/mcp_test.rs`

**Interfaces:**
- Consumes: `LogStorage::{read_tail, stream_full}` (Task 8); `tail_body`, `human_bytes`, `LOG_SOURCE_HEADER`, `StepFilter` (Task 3); `check_job_acl` (`web/api/jobs.rs:1161`, `pub(crate)`).
- Produces: `pub struct LogQuery { pub tail_bytes: Option<String>, pub full: Option<String> }`; `pub enum LogReadMode { Tail(u64), Full }`; `pub fn parse_log_mode(&LogQuery, &LogReadConfig) -> Result<LogReadMode, AppError>`; MCP param `tail_bytes: Option<u64>`.

- [ ] **Step 1: Write the failing integration tests**

Add near `body_json` in `crates/stroem-server/tests/integration_test.rs`:

```rust
async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    response.into_body().collect().await.unwrap().to_bytes().to_vec()
}

fn jsonl_entry(step: &str, msg: &str) -> String {
    format!(r#"{{"ts":"2026-09-22T00:00:00Z","stream":"stdout","step":"{step}","line":"{msg}"}}"#)
}

/// A pending job whose local log file holds `content`.
async fn job_with_log(pool: &PgPool, tmp: &TempDir, content: &str) -> Result<Uuid> {
    let job_id = JobRepo::create(pool, "default", "hello-world", "distributed", None, "api", None, None, None)
        .await?;
    std::fs::write(tmp.path().join("logs").join(format!("{job_id}.jsonl")), content)?;
    Ok(job_id)
}

fn big_log(lines: usize) -> String {
    (0..lines).map(|i| format!("{}\n", jsonl_entry("build", &format!("line {i:06}")))).collect()
}
```

and these tests at the end of the file:

```rust
#[tokio::test]
async fn test_log_tail_envelope_and_source_header() -> Result<()> {
    let (router, pool, tmp, _container) = setup().await?;
    let content = format!("{}\n{}\n", jsonl_entry("build", "one"), jsonl_entry("build", "two"));
    let job_id = job_with_log(&pool, &tmp, &content).await?;
    let response = router.oneshot(api_get(&format!("/api/jobs/{job_id}/logs"))).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-stroem-log-source"], "local");
    let body = body_json(response).await;
    assert_eq!(body["logs"], content);
    assert_eq!(body["truncated"], false);
    assert_eq!(body["total_bytes"], content.len());
    assert_eq!(body["returned_bytes"], content.len());
    Ok(())
}

#[tokio::test]
async fn test_log_tail_is_bounded_and_flags_truncation() -> Result<()> {
    let (router, pool, tmp, _container) = setup().await?;
    let content = big_log(20_000);
    let job_id = job_with_log(&pool, &tmp, &content).await?;
    let body = body_json(router.clone().oneshot(api_get(&format!("/api/jobs/{job_id}/logs"))).await?).await;
    let logs = body["logs"].as_str().unwrap();
    assert_eq!(body["truncated"], true);
    assert!(logs.len() <= 262_144, "default tail is 256 KiB, got {}", logs.len());
    assert!(content.ends_with(logs) && logs.starts_with('{'), "whole lines from the end");
    assert_eq!(body["total_bytes"], content.len());

    let small = body_json(
        router
            .oneshot(api_get(&format!("/api/jobs/{job_id}/logs?tail_bytes=1024")))
            .await?,
    )
    .await;
    assert!(small["returned_bytes"].as_u64().unwrap() <= 1024);
    Ok(())
}

#[tokio::test]
async fn test_log_full_streams_ndjson_equal_to_the_file() -> Result<()> {
    let (router, pool, tmp, _container) = setup().await?;
    let content = big_log(20_000);
    let job_id = job_with_log(&pool, &tmp, &content).await?;
    let response = router.oneshot(api_get(&format!("/api/jobs/{job_id}/logs?full=true"))).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()["content-type"].to_str()?.starts_with("application/x-ndjson"));
    assert_eq!(response.headers()["x-stroem-log-source"], "local");
    assert_eq!(body_bytes(response).await, content.into_bytes());
    Ok(())
}

#[tokio::test]
async fn test_log_query_validation() -> Result<()> {
    let (router, pool, tmp, _container) = setup().await?;
    let job_id = job_with_log(&pool, &tmp, "x\n").await?;
    for q in ["tail_bytes=0", "tail_bytes=4194305", "tail_bytes=abc", "tail_bytes=10&full=true", "full=yes"] {
        for path in [format!("/api/jobs/{job_id}/logs?{q}"), format!("/api/jobs/{job_id}/steps/build/logs?{q}")] {
            let response = router.clone().oneshot(api_get(&path)).await?;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            assert!(body_json(response).await["error"].is_string(), "{path}: JSON error body");
        }
    }
    Ok(())
}

#[tokio::test]
async fn test_step_logs_are_filtered_in_both_modes() -> Result<()> {
    let (router, pool, tmp, _container) = setup().await?;
    let (b, t, s) = (jsonl_entry("build", "b"), jsonl_entry("test", "t"), jsonl_entry("_server", "hook failed"));
    let job_id = job_with_log(&pool, &tmp, &format!("{b}\n{t}\n{s}\n")).await?;
    let tail = body_json(router.clone().oneshot(api_get(&format!("/api/jobs/{job_id}/steps/build/logs"))).await?).await;
    assert_eq!(tail["logs"], format!("{b}\n"));
    let full = router.clone().oneshot(api_get(&format!("/api/jobs/{job_id}/steps/test/logs?full=true"))).await?;
    assert_eq!(String::from_utf8(body_bytes(full).await)?, format!("{t}\n"));
    let server = body_json(router.oneshot(api_get(&format!("/api/jobs/{job_id}/steps/_server/logs"))).await?).await;
    assert_eq!(server["logs"], format!("{s}\n"));
    Ok(())
}

#[tokio::test]
async fn test_ws_backfill_is_a_tail() -> Result<()> {
    let (router, pool, tmp, _container) = setup().await?;
    let content = big_log(20_000);
    let job_id = job_with_log(&pool, &tmp, &content).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let url = format!("ws://127.0.0.1:{}/api/jobs/{}/logs/stream", addr.port(), job_id);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");
    use futures_util::StreamExt;
    let text = ws.next().await.unwrap()?.into_text()?;
    assert!(text.len() <= 262_144, "backfill is the default tail, got {}", text.len());
    assert!(content.ends_with(text.as_str()));
    drop(ws);
    server.abort();
    Ok(())
}
```

Add to `crates/stroem-server/tests/mcp_test.rs` (import `JobRepo` from `stroem_db` if not already imported):

```rust
#[tokio::test]
async fn test_mcp_get_job_logs_tail_bytes_and_truncation_trailer() -> Result<()> {
    let (router, pool, tmp, _container) = setup_with_mcp().await?;
    let job_id = JobRepo::create(&pool, "default", "hello-world", "distributed", None, "api", None, None, None)
        .await?;
    let content: String = (0..2_000)
        .map(|i| format!(r#"{{"ts":"2026-09-22T00:00:00Z","stream":"stdout","step":"build","line":"line {i:05}"}}"#) + "\n")
        .collect();
    std::fs::write(tmp.path().join("logs").join(format!("{job_id}.jsonl")), &content)?;
    let (router, session_id) = mcp_initialize(router).await;
    let call = |id: u64, args: Value| {
        json!({"jsonrpc": "2.0", "method": "tools/call", "id": id,
               "params": {"name": "get_job_logs", "arguments": args}})
    };

    let resp = body_json(router.clone().oneshot(mcp_request(session_id.as_deref(), call(2, json!({"job_id": job_id.to_string()})))).await?).await;
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("line 01999") && text.contains("line 00000"));
    assert!(!text.contains("[truncated:"), "under the default tail there is no trailer");

    let resp = body_json(router.clone().oneshot(mcp_request(session_id.as_deref(), call(3, json!({"job_id": job_id.to_string(), "tail_bytes": 1000})))).await?).await;
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("line 01999") && !text.contains("line 00000"));
    assert!(text.trim_end().ends_with("earlier lines omitted]"), "{text}");

    let resp = body_json(router.oneshot(mcp_request(session_id.as_deref(), call(4, json!({"job_id": job_id.to_string(), "tail_bytes": 0})))).await?).await;
    assert!(resp.get("error").is_some(), "tail_bytes 0 is invalid params: {resp}");
    Ok(())
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p stroem-server --test integration_test test_log_ -- --test-threads=4`
Expected: FAIL — the envelope has no `truncated` and no source header; `?tail_bytes=0` answers 200.

- [ ] **Step 3: Create `web/api/logs.rs`**

```rust
//! `GET /api/jobs/{id}/logs` and `GET /api/jobs/{id}/steps/{step}/logs`:
//! a bounded tail by default, the whole log streamed with `?full=true`
//! (spec § 3.1).

use crate::acl::TaskPermission;
use crate::config::LogReadConfig;
use crate::log_read::{tail_body, StepFilter, LOG_SOURCE_HEADER};
use crate::log_storage::JobLogMeta;
use crate::state::AppState;
use crate::web::api::jobs::check_job_acl;
use crate::web::api::middleware::AuthUser;
use crate::web::api::parse_uuid_param;
use crate::web::error::AppError;
use anyhow::Context;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderName, HeaderValue};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::Arc;
use stroem_db::JobRepo;

/// Raw query values, so malformed input becomes our JSON 400 rather than
/// axum's plain-text rejection.
#[derive(Debug, Default, Deserialize)]
pub struct LogQuery {
    pub tail_bytes: Option<String>,
    pub full: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LogReadMode {
    Tail(u64),
    Full,
}

pub fn parse_log_mode(q: &LogQuery, cfg: &LogReadConfig) -> Result<LogReadMode, AppError> {
    let full = match q.full.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "full must be true or false, got '{other}'"
            )))
        }
    };
    match (full, q.tail_bytes.as_deref()) {
        (true, Some(_)) => Err(AppError::BadRequest(
            "tail_bytes and full=true are mutually exclusive".into(),
        )),
        (true, None) => Ok(LogReadMode::Full),
        (false, None) => Ok(LogReadMode::Tail(cfg.tail_default_bytes)),
        (false, Some(raw)) => match raw.parse::<u64>() {
            Ok(n) if (1..=cfg.tail_max_bytes).contains(&n) => Ok(LogReadMode::Tail(n)),
            _ => Err(AppError::BadRequest(format!(
                "tail_bytes must be an integer between 1 and {}",
                cfg.tail_max_bytes
            ))),
        },
    }
}

fn source_header(source: &'static str) -> (HeaderName, HeaderValue) {
    (HeaderName::from_static(LOG_SOURCE_HEADER), HeaderValue::from_static(source))
}

async fn read_logs(
    state: Arc<AppState>,
    auth_user: Option<AuthUser>,
    id: String,
    step: Option<String>,
    query: LogQuery,
) -> Result<Response, AppError> {
    let job_id = parse_uuid_param(&id, "job")?;
    let mode = parse_log_mode(&query, &state.config.log_storage.read)?;
    let job = JobRepo::get(&state.pool, job_id)
        .await
        .context("get job")?
        .ok_or_else(|| AppError::not_found("Job"))?;
    let perm = check_job_acl(&state, &auth_user, &job.workspace, &job.task_name).await?;
    if matches!(perm, TaskPermission::Deny) {
        return Err(AppError::not_found("Job"));
    }
    let is_terminal = stroem_common::models::job::is_terminal_status(&job.status);
    let meta = JobLogMeta {
        workspace: job.workspace,
        task_name: job.task_name,
        created_at: job.created_at,
    };
    let filter = step.as_deref().map_or(StepFilter::All, StepFilter::Step);
    match mode {
        LogReadMode::Tail(n) => {
            let tail = state
                .log_storage
                .read_tail(job_id, &meta, is_terminal, filter, n)
                .await
                .context("read log tail")?;
            let body = tail_body(&tail).context("serialize log tail")?;
            let headers = [
                (header::CONTENT_TYPE, HeaderValue::from_static("application/json")),
                source_header(tail.source.as_str()),
            ];
            Ok((headers, body).into_response())
        }
        LogReadMode::Full => {
            let (source, stream) = state
                .log_storage
                .stream_full(job_id, &meta, is_terminal, filter)
                .await
                .context("stream log")?;
            let headers = [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/x-ndjson; charset=utf-8"),
                ),
                source_header(source.as_str()),
            ];
            Ok((headers, Body::from_stream(stream)).into_response())
        }
    }
}

/// GET /api/jobs/{id}/logs
#[tracing::instrument(skip(state))]
pub async fn get_job_logs(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> Result<Response, AppError> {
    read_logs(state, auth_user, id, None, query).await
}

/// GET /api/jobs/{id}/steps/{step}/logs — `_server` is a pseudo-step.
#[tracing::instrument(skip(state))]
pub async fn get_step_logs(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path((id, step_name)): Path<(String, String)>,
    Query(query): Query<LogQuery>,
) -> Result<Response, AppError> {
    read_logs(state, auth_user, id, Some(step_name), query).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(tail: Option<&str>, full: Option<&str>) -> LogQuery {
        LogQuery { tail_bytes: tail.map(Into::into), full: full.map(Into::into) }
    }

    #[test]
    fn parse_log_mode_defaults_bounds_and_exclusivity() {
        let cfg = LogReadConfig::default();
        assert_eq!(parse_log_mode(&q(None, None), &cfg).unwrap(), LogReadMode::Tail(262_144));
        assert_eq!(parse_log_mode(&q(Some("1024"), None), &cfg).unwrap(), LogReadMode::Tail(1024));
        assert_eq!(parse_log_mode(&q(Some("4194304"), None), &cfg).unwrap(), LogReadMode::Tail(4_194_304));
        assert_eq!(parse_log_mode(&q(None, Some("true")), &cfg).unwrap(), LogReadMode::Full);
        assert_eq!(parse_log_mode(&q(None, Some("false")), &cfg).unwrap(), LogReadMode::Tail(262_144));
        for (tail, full) in [
            (Some("0"), None),
            (Some("4194305"), None),
            (Some("-1"), None),
            (Some("abc"), None),
            (Some("10"), Some("true")),
            (None, Some("yes")),
        ] {
            assert!(matches!(parse_log_mode(&q(tail, full), &cfg), Err(AppError::BadRequest(_))), "{tail:?} {full:?}");
        }
    }
}
```

In `web/api/mod.rs` add `pub mod logs;` and change the two routes to `get(logs::get_job_logs)` and `get(logs::get_step_logs)`. Delete `get_step_logs` and `get_job_logs` from `web/api/jobs.rs` (lines 559–629 at plan time) and any import that clippy then reports unused.

- [ ] **Step 4: Switch the WebSocket backfill**

In `web/api/ws.rs`, import `use crate::log_read::StepFilter;` and replace the `if let Ok(existing) = state.log_storage.get_log(...)` block (`:164-172`) with:

```rust
        let tail_bytes = state.config.log_storage.read.tail_default_bytes;
        if let Ok(tail) = state
            .log_storage
            .read_tail(job_id, &meta, is_terminal, StepFilter::All, tail_bytes)
            .await
        {
            if !tail.logs.is_empty()
                && socket
                    .send(axum::extract::ws::Message::Text(tail.logs.into()))
                    .await
                    .is_err()
            {
                return;
            }
        }
```

- [ ] **Step 5: Switch the MCP tool**

In `mcp/tools.rs`, add to `GetJobLogsParams`:

```rust
    /// Bytes of the log's end to return. Defaults to 256 KiB; at most
    /// `log_storage.read.tail_max_bytes` (4 MiB by default).
    #[serde(default)]
    pub tail_bytes: Option<u64>,
```

change the tool description to `"Get the end of a job's log (the last 256 KiB unless tail_bytes is given). Optionally filter by step name."`, and replace the body after the `meta` construction with:

```rust
        let read = self.state.config.log_storage.read;
        let tail_bytes = match params.tail_bytes {
            None => read.tail_default_bytes,
            Some(n) if (1..=read.tail_max_bytes).contains(&n) => n,
            Some(_) => {
                return Err(rmcp::ErrorData::invalid_params(
                    format!("tail_bytes must be between 1 and {}", read.tail_max_bytes),
                    None,
                ))
            }
        };
        let filter = params
            .step
            .as_deref()
            .map_or(crate::log_read::StepFilter::All, crate::log_read::StepFilter::Step);
        let tail = self
            .state
            .log_storage
            .read_tail(job_id, &meta, is_terminal, filter, tail_bytes)
            .await
            .map_err(|e| internal_err(format!("Failed to get logs: {e}")))?;

        let mut formatted = format_logs(&tail.logs);
        if tail.truncated {
            if !formatted.ends_with('\n') {
                formatted.push('\n');
            }
            formatted.push_str(&truncation_trailer(&tail));
        }
        Ok(text_result(formatted))
```

Add next to `format_logs`:

```rust
/// Last line of a truncated `get_job_logs` result.
fn truncation_trailer(tail: &crate::log_read::Tail) -> String {
    format!(
        "[truncated: showing the last {} of up to {} — earlier lines omitted]",
        crate::log_read::human_bytes(tail.returned_bytes()),
        crate::log_read::human_bytes(tail.total_bytes)
    )
}
```

and a unit test in the tools test module:

```rust
    #[test]
    fn truncation_trailer_names_both_sizes() {
        let tail = crate::log_read::Tail {
            logs: "x".repeat(262_144),
            truncated: true,
            total_bytes: 87_325_871,
            source: crate::log_read::LogSource::Local,
        };
        assert_eq!(
            truncation_trailer(&tail),
            "[truncated: showing the last 256.0 KiB of up to 83.3 MiB — earlier lines omitted]"
        );
    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p stroem-server --lib web::api::logs && cargo test -p stroem-server --lib mcp::tools && cargo test -p stroem-server --test integration_test -- test_log_ test_step_logs_ test_ws_ && cargo test -p stroem-server --test mcp_test test_mcp_get_job_logs`
Expected: PASS, including the existing `test_ws_backfill_existing_logs`, `test_log_append_and_retrieve` and the other WS tests.

- [ ] **Step 7: Commit**

```bash
git add crates/stroem-server/src/web crates/stroem-server/src/mcp/tools.rs crates/stroem-server/tests/integration_test.rs crates/stroem-server/tests/mcp_test.rs
git commit -m "feat(logs): tail by default on REST, WebSocket and MCP; ?full=true streams NDJSON"
```

---

### Task 10: Remove the unbounded readers and migrate their callers

**Files:**
- Modify: `crates/stroem-server/src/log_storage.rs` (delete old readers; migrate unit tests)
- Modify: `crates/stroem-server/src/state.rs` (tests at `:396-580`)
- Modify: `crates/stroem-server/tests/ha_test.rs` (`:319`, `:384`, `:535`), `crates/stroem-server/tests/integration_test.rs` (`get_log` at about `:29241` and `:29559`), `crates/stroem-server/tests/s3_integration_test.rs` (`:143`, `:188`, `:256`, `:259`)
- Modify: `crates/stroem-server/src/log_read/mod.rs` (drop `#![allow(dead_code)]`)

**Interfaces:**
- Removes: `LogStorage::{get_log, get_step_log, read_local_log, read_file_if_present, read_local_step_log, filter_step_from_file_if_present, line_matches_step, get_log_from_archive, get_step_log_from_archive}`.

- [ ] **Step 1: Delete the old readers**

In `log_storage.rs` delete the nine methods above (all of `get_log` through `get_step_log_from_archive`, lines 300–555 at plan time, keeping `upload_to_archive` and `get_log_path`). Delete `#![allow(dead_code)]` and its comment from `log_read/mod.rs`.

- [ ] **Step 2: Build to list every caller**

Run: `cargo build -p stroem-server --all-targets 2>&1 | grep -E '^error|-->' | head -60`
Expected: `no method named get_log` / `get_step_log` errors at the test call sites listed under **Files**, and nowhere in production code.

- [ ] **Step 3: Migrate the unit tests**

Add to the tests module of `log_storage.rs` (next to `ALL` from Task 8):

```rust
    /// The whole log in one read — test helper standing in for the removed
    /// `get_log` / `get_step_log`.
    async fn read_all(
        storage: &LogStorage,
        job: Uuid,
        meta: &JobLogMeta,
        terminal: bool,
        filter: StepFilter<'_>,
    ) -> String {
        storage.read_tail(job, meta, terminal, filter, ALL).await.unwrap().logs
    }
```

Rewrite each call: `storage.get_log(J, M, T).await.unwrap()` (or `.expect(..)`) → `read_all(&storage, J, M, T, StepFilter::All).await`; `storage.get_step_log(J, "S", M, T).await.unwrap()` → `read_all(&storage, J, M, T, StepFilter::Step("S")).await`. Keep every assertion unchanged. Do the same in the `state.rs` tests with a copy of `read_all` taking `&state.log_storage` and `use crate::log_read::StepFilter;`.

- [ ] **Step 4: Migrate the integration tests**

In `ha_test.rs`, `integration_test.rs` and `s3_integration_test.rs` add `use stroem_server::log_read::StepFilter;` and `const ALL: u64 = 16 * 1024 * 1024;`, then rewrite:

```rust
// before
let logs = state_b.log_storage.get_log(job, &meta, false).await?;
// after
let logs = state_b.log_storage.read_tail(job, &meta, false, StepFilter::All, ALL).await?.logs;

// before
let build_logs = storage.get_step_log(job_id, "build", &meta, true).await?;
// after
let build_logs = storage.read_tail(job_id, &meta, true, StepFilter::Step("build"), ALL).await?.logs;
```

In `integration_test.rs` the loop at about `:29241` uses `if let Ok(text) = state.log_storage.get_log(..)` — rewrite it to `if let Ok(tail) = state.log_storage.read_tail(job.job_id, &meta, false, StepFilter::All, ALL).await` and use `tail.logs`. Keep every assertion unchanged.

- [ ] **Step 5: Run everything that reads logs**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo test -p stroem-server --lib && cargo test -p stroem-server --test ha_test && cargo test -p stroem-server --test s3_integration_test && cargo test -p stroem-server --test integration_test -- log hook_chain dispatch_error`
Expected: clippy clean (remove any now-unused import it reports, e.g. `AsyncReadExt` in `log_storage.rs`); all PASS.

- [ ] **Step 6: Commit**

```bash
git add -A crates
git commit -m "refactor(logs): remove the unbounded String log readers; migrate callers"
```

---

### Task 11: Peak-allocation test binary

**Files:**
- Create: `crates/stroem-server/tests/log_peak_alloc_test.rs`
- Modify: `crates/stroem-server/Cargo.toml` (a `[[test]]` entry)

**Interfaces:**
- Consumes: `LogStorage::{read_tail, stream_full, with_archive, with_read_config}`, `log_read::{tail_body, StepFilter, LogSource}`, `log_storage::archive_key`, `blob_storage::{LocalBlobArchive, S3BlobArchive}`.

- [ ] **Step 1: Register the binary**

Append to `crates/stroem-server/Cargo.toml`:

```toml
# Counting global allocator, one read at a time: needs its own main.
[[test]]
name = "log_peak_alloc_test"
harness = false
```

- [ ] **Step 2: Write the test**

`crates/stroem-server/tests/log_peak_alloc_test.rs`:

```rust
//! Peak-allocation regression test for log reads — spec § 3.4 and § 5.
//!
//! Its own binary (`harness = false`) so it can install a counting global
//! allocator and run one read at a time. It counts the REQUESTED sizes of
//! alloc/realloc/dealloc on the Rust heap: not allocator metadata, not
//! stack, not RSS. Fixtures live on disk, so only the read allocates. Each
//! case asserts `peak − baseline <= formula × 1.5`; the 1.5 covers what the
//! formulas do not model (allocator rounding, the transient copy inside a
//! `realloc`). `STROEM_PEAK_ALLOC_LARGE=1` grows the incident-shaped fixture
//! from 8 MiB to 100 MiB (release checklist).

use futures_util::StreamExt;
use std::alloc::{GlobalAlloc, Layout, System};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;
use stroem_server::blob_storage::{BlobArchive, LocalBlobArchive};
use stroem_server::config::LogReadConfig;
use stroem_server::log_read::{tail_body, LogSource, StepFilter};
use stroem_server::log_storage::{archive_key, JobLogMeta, LogStorage};
use tempfile::TempDir;
use uuid::Uuid;

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(n: usize) {
    let now = CURRENT.fetch_add(n, SeqCst) + n;
    PEAK.fetch_max(now, SeqCst);
}

fn shrink(n: usize) {
    CURRENT.fetch_sub(n, SeqCst);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        shrink(layout.size());
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, new_size) };
        if !q.is_null() {
            if new_size > layout.size() {
                grow(new_size - layout.size());
            } else {
                shrink(layout.size() - new_size);
            }
        }
        q
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Run `f`; return its output and the peak heap growth above the level at
/// its start.
async fn peak_of<T>(f: impl Future<Output = T>) -> (T, usize) {
    let baseline = CURRENT.load(SeqCst);
    PEAK.store(baseline, SeqCst);
    let out = f.await;
    (out, PEAK.load(SeqCst).saturating_sub(baseline))
}

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const K: usize = 64 * KIB;
const R: usize = MIB;
const H: usize = 408 * KIB;
const D: usize = 64 * KIB;

// § 3.4 formulas (with the plan's amendments 3 and 5).
fn envelope(t: usize) -> usize { 6 * t + 256 }
fn tail_local(t: usize) -> usize { 2 * t + envelope(t) }
fn tail_step(t: usize, l: usize) -> usize { 3 * t + 3 * l + envelope(t) }
fn tail_merged(t: usize, l: usize, n: usize) -> usize {
    11 * t + 3 * l + R + H + D + K + (2 * t + 96 * n) + 256
}
fn full_local() -> usize { 3 * K }
fn full_local_filtered(l: usize) -> usize { 5 * l + 4 * K }
fn full_archive(l: usize, filtered: bool) -> usize {
    R + H + D + 3 * K + if filtered { 5 * l } else { 0 }
}
fn full_merged(c: usize, n: usize) -> usize { 3 * c + 96 * n + R + H + D + K }

#[derive(Default)]
struct Report {
    over: Vec<String>,
}

impl Report {
    fn check(&mut self, name: &str, peak: usize, formula: usize) {
        let bound = formula + formula / 2;
        let verdict = if peak <= bound { "ok" } else { "OVER" };
        println!("{name:<48} peak {peak:>11}  bound {bound:>11}  {verdict}");
        if peak > bound {
            self.over.push(format!("{name}: {peak} > {bound}"));
        }
    }
}

fn meta() -> JobLogMeta {
    JobLogMeta {
        workspace: "ws".into(),
        task_name: "task".into(),
        created_at: chrono::DateTime::parse_from_rfc3339("2026-09-22T03:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    }
}

fn incident_line(i: usize, step: &str, tag: &str) -> String {
    format!(
        r#"{{"ts":"2026-09-22T06:01:04.{:06}Z","stream":"stdout","step":"{step}","line":"2026-09-22 06:01:04,312 - INFO -   {tag} frame bf3a02f470e74c38b71746a0a15f13fd (243 items) #{i}"}}"#,
        i % 1_000_000
    )
}

/// ~190-byte lines like the incident's; one `quiet` line first.
fn incident_log(bytes: usize, tag: &str) -> String {
    let mut s = String::with_capacity(bytes + 512);
    s.push_str(&incident_line(0, "quiet", tag));
    s.push('\n');
    let mut i = 1;
    while s.len() < bytes {
        s.push_str(&incident_line(i, "sync", tag));
        s.push('\n');
        i += 1;
    }
    s
}

/// Distinct ~64-byte lines.
fn short_lines(n: usize, tag: &str) -> String {
    (0..n)
        .map(|i| format!(r#"{{"ts":"2026-09-22T00:00:00Z","step":"s","line":"{tag}{i:08}"}}"#) + "\n")
        .collect()
}

fn gzip(data: &[u8]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

struct Env {
    _live: TempDir,
    _arch: TempDir,
    live: PathBuf,
    archive: Arc<dyn BlobArchive>,
    storage: LogStorage,
}

fn env_with(cfg: LogReadConfig) -> Env {
    let live = TempDir::new().unwrap();
    let arch = TempDir::new().unwrap();
    let archive: Arc<dyn BlobArchive> = Arc::new(LocalBlobArchive::new(arch.path().to_path_buf()));
    let storage = LogStorage::new(live.path())
        .with_archive(Arc::clone(&archive), String::new())
        .with_read_config(cfg);
    Env { live: live.path().to_path_buf(), _live: live, _arch: arch, archive, storage }
}

impl Env {
    async fn local(&self, job: Uuid, ext: &str, content: &[u8]) {
        tokio::fs::write(self.live.join(format!("{job}.{ext}")), content).await.unwrap();
    }

    async fn archived(&self, job: Uuid, content: &[u8]) {
        self.archive
            .put(&archive_key("", job, &meta()), "application/gzip", gzip(content).into())
            .await
            .unwrap();
    }

    async fn tail(&self, job: Uuid, terminal: bool, filter: StepFilter<'_>, t: usize) -> LogSource {
        let tail = self.storage.read_tail(job, &meta(), terminal, filter, t as u64).await.unwrap();
        drop(tail_body(&tail).unwrap());
        tail.source
    }

    async fn drain(&self, job: Uuid, terminal: bool, filter: StepFilter<'_>) -> LogSource {
        let (source, mut stream) = self.storage.stream_full(job, &meta(), terminal, filter).await.unwrap();
        while let Some(chunk) = stream.next().await {
            drop(chunk.unwrap());
        }
        source
    }
}

fn main() {
    let large = std::env::var_os("STROEM_PEAK_ALLOC_LARGE").is_some();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run(large));
}

async fn run(large: bool) {
    let cfg = LogReadConfig::default();
    let (t, l) = (cfg.tail_default_bytes as usize, cfg.max_line_bytes);
    let (c, n) = (cfg.merge_max_bytes as usize, cfg.merge_max_lines);
    let mut report = Report::default();

    // (a) Incident-shaped log, local and archived.
    let e = env_with(cfg);
    let job = Uuid::new_v4();
    {
        // 9 MiB, not 8: local + archive must exceed merge_max_bytes (16 MiB)
        // so the "over the cap" case below is really over it.
        let log = incident_log(if large { 100 * MIB } else { 9 * MIB }, "Uploading");
        e.local(job, "jsonl", log.as_bytes()).await;
        e.archived(job, log.as_bytes()).await;
    }
    // Warm up the blocking pool and lazy statics before any measurement.
    e.tail(job, false, StepFilter::All, 1024).await;

    let (s, p) = peak_of(e.tail(job, false, StepFilter::All, t)).await;
    assert_eq!(s, LogSource::Local);
    report.check("tail, unfiltered, local", p, tail_local(t));
    let (_, p) = peak_of(e.tail(job, false, StepFilter::Step("sync"), t)).await;
    report.check("tail, step (chatty), local", p, tail_step(t, l));
    let (_, p) = peak_of(e.tail(job, false, StepFilter::Step("quiet"), t)).await;
    report.check("tail, step (quiet: scans to the cap), local", p, tail_step(t, l));
    let (_, p) = peak_of(e.drain(job, false, StepFilter::All)).await;
    report.check("full, unfiltered, local", p, full_local());
    let (_, p) = peak_of(e.drain(job, false, StepFilter::Step("sync"))).await;
    report.check("full, filtered, local", p, full_local_filtered(l));
    let (s, p) = peak_of(e.tail(job, true, StepFilter::All, t)).await;
    assert_eq!(s, LogSource::Merged);
    report.check("tail, terminal, merged", p, tail_merged(t, l, n));
    let (_, p) = peak_of(e.tail(job, true, StepFilter::Step("sync"), t)).await;
    report.check("tail, terminal step, merged", p, tail_merged(t, l, n));
    let (s, p) = peak_of(e.drain(job, true, StepFilter::All)).await;
    assert_eq!(s, LogSource::Local, "the incident log is over merge_max_bytes");
    report.check("full, terminal over the cap, local", p, full_local());
    tokio::fs::remove_file(e.live.join(format!("{job}.jsonl"))).await.unwrap();
    let (s, p) = peak_of(e.drain(job, true, StepFilter::All)).await;
    assert_eq!(s, LogSource::Archive);
    report.check("full, archive, single source", p, full_archive(l, false));
    let (_, p) = peak_of(e.drain(job, true, StepFilter::Step("sync"))).await;
    report.check("full, archive filtered, single source", p, full_archive(l, true));

    // Full merged under both caps: 10 MiB local + 4 MiB archive, disjoint.
    {
        let e = env_with(cfg);
        let job = Uuid::new_v4();
        e.local(job, "jsonl", incident_log(10 * MIB, "Uploading").as_bytes()).await;
        e.archived(job, incident_log(4 * MIB, "Archived").as_bytes()).await;
        let (s, p) = peak_of(e.drain(job, true, StepFilter::All)).await;
        assert_eq!(s, LogSource::Merged);
        report.check("full, terminal, merged", p, full_merged(c, n));
    }

    // (b) Distinct short lines filling a 4 MiB tail on each side, just
    // under the merge line cap.
    {
        let e = env_with(cfg);
        let job = Uuid::new_v4();
        let t4 = 4 * MIB;
        // 60-byte lines: 60 000 per side is 3.4 MiB (under the 4 MiB tail)
        // and 120 000 in the union (under merge_max_lines = 131 072).
        e.local(job, "jsonl", short_lines(60_000, "L").as_bytes()).await;
        e.archived(job, short_lines(60_000, "A").as_bytes()).await;
        let (s, p) = peak_of(e.tail(job, true, StepFilter::All, t4)).await;
        assert_eq!(s, LogSource::Merged, "the two tails must fit merge_max_lines");
        report.check("tail 4 MiB, terminal, merged, short lines", p, tail_merged(t4, l, n));
    }

    // (c) Worst-case envelope escapes: a legacy file of raw control bytes.
    {
        let e = env_with(cfg);
        let job = Uuid::new_v4();
        let line = format!("{}\n", "\u{1}".repeat(200));
        e.local(job, "log", line.repeat(MIB / line.len()).as_bytes()).await;
        let (s, p) = peak_of(e.tail(job, false, StepFilter::All, t)).await;
        assert_eq!(s, LogSource::Local);
        report.check("tail, control bytes (6x envelope)", p, tail_local(t));
    }

    // (d) Matcher scratch: the step value carries an escape, and the name
    // also appears literally so the fast guard lets the line through.
    {
        let e = env_with(cfg);
        let job = Uuid::new_v4();
        let body = "s".repeat((l - 200) / 2);
        let step = format!("{body}A");
        let hit = format!(r#"{{"step":"{body}A","line":"{body}A"}}"#);
        let miss = r#"{"step":"other","line":"x"}"#;
        let content: String = (0..5).map(|_| format!("{hit}\n{miss}\n")).collect();
        e.local(job, "jsonl", content.as_bytes()).await;
        let (_, p) = peak_of(e.drain(job, false, StepFilter::Step(&step))).await;
        report.check("full, filtered, escaped step values", p, full_local_filtered(l));
    }

    // (e) One 15 MiB line under merge_max_lines = 2: the merger's
    // byte-based reservation, not its line count, dominates.
    {
        let cfg2 = LogReadConfig { merge_max_lines: 2, ..cfg };
        let e = env_with(cfg2);
        let job = Uuid::new_v4();
        let huge = format!(r#"{{"ts":"2026-09-22T00:00:00Z","step":"s","line":"{}"}}"#, "x".repeat(15 * MIB)) + "\n";
        e.local(job, "jsonl", huge.as_bytes()).await;
        e.archived(job, short_lines(1, "A").as_bytes()).await;
        let (s, p) = peak_of(e.drain(job, true, StepFilter::All)).await;
        assert_eq!(s, LogSource::Merged);
        report.check("full, terminal, merged, one 15 MiB line", p, full_merged(c, 2));
    }

    #[cfg(feature = "s3")]
    s3_cases(&mut report, cfg).await;

    assert!(report.over.is_empty(), "reads over their memory bound: {:#?}", report.over);
}

#[cfg(feature = "s3")]
async fn s3_cases(report: &mut Report, cfg: LogReadConfig) {
    use stroem_server::blob_storage::S3BlobArchive;
    use testcontainers::runners::AsyncRunner;
    use testcontainers::ImageExt;
    use testcontainers_modules::minio::MinIO;

    let container = MinIO::default().with_name("quay.io/minio/minio").start().await.unwrap();
    let port = container.get_host_port_ipv4(9000).await.unwrap();
    let creds = aws_sdk_s3::config::Credentials::new("minioadmin", "minioadmin", None, None, "test");
    let client = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .endpoint_url(format!("http://127.0.0.1:{port}"))
            .credentials_provider(creds)
            .force_path_style(true)
            .build(),
    );
    client.create_bucket().bucket("peak").send().await.unwrap();
    let archive: Arc<dyn BlobArchive> = Arc::new(S3BlobArchive::from_client(client, "peak".into()));
    let live = TempDir::new().unwrap();
    let storage = LogStorage::new(live.path())
        .with_archive(Arc::clone(&archive), String::new())
        .with_read_config(cfg);
    let job = Uuid::new_v4();
    let key = archive_key("", job, &meta());
    {
        let log = incident_log(8 * MIB, "Uploading");
        tokio::fs::write(live.path().join(format!("{job}.jsonl")), &log).await.unwrap();
        archive.put(&key, "application/gzip", gzip(log.as_bytes()).into()).await.unwrap();
    }
    // Warm the SDK's connection pool, credentials and endpoint caches.
    let obj = archive.open(&key).await.unwrap().unwrap();
    archive.read_range(&obj, 0, 16, &mut Vec::new()).await.unwrap();
    drop(obj);

    let (t, l, n) = (cfg.tail_default_bytes as usize, cfg.max_line_bytes, cfg.merge_max_lines);
    let (_, p) = peak_of(async {
        let tail = storage.read_tail(job, &meta(), true, StepFilter::All, t as u64).await.unwrap();
        assert_eq!(tail.source, LogSource::Merged);
        drop(tail_body(&tail).unwrap());
    })
    .await;
    report.check("S3: tail, terminal, merged", p, tail_merged(t, l, n));

    tokio::fs::remove_file(live.path().join(format!("{job}.jsonl"))).await.unwrap();
    let (_, p) = peak_of(async {
        let (source, mut stream) = storage.stream_full(job, &meta(), true, StepFilter::All).await.unwrap();
        assert_eq!(source, LogSource::Archive);
        while let Some(chunk) = stream.next().await {
            drop(chunk.unwrap());
        }
    })
    .await;
    report.check("S3: full, archive, single source", p, full_archive(l, false));
}
```

- [ ] **Step 3: Run it**

Run: `cargo test -p stroem-server --test log_peak_alloc_test`
Expected: a table of 15 cases, all `ok` (Docker is needed for the S3 cases). If a case prints `OVER`, do not raise the bound: find the allocation with a heap profiler (`heaptrack` or `valgrind --tool=dhat`) and fix the code, or — only if the formula missed a real buffer — amend the formula in both this file and spec § 3.4 and say so in the commit message.

- [ ] **Step 4: Run the large variant once**

Run: `STROEM_PEAK_ALLOC_LARGE=1 cargo test -p stroem-server --release --test log_peak_alloc_test`
Expected: all `ok`; the bounds do not depend on the log's size.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/Cargo.toml crates/stroem-server/tests/log_peak_alloc_test.rs
git commit -m "test(logs): counting-allocator peak test for every read mode"
```

### Task 12: CLI `stroem-api logs --full / --tail-bytes`

**Files:**
- Modify: `crates/stroem-cli/src/remote/logs.rs` (rewrite)
- Modify: `crates/stroem-cli/src/remote/mod.rs:33-37` (flags), `:144-146` (dispatch), tests module

**Interfaces:**
- Produces: `pub fn logs_url(server: &str, job_id: &str, full: bool, tail_bytes: Option<u64>) -> String`; `pub fn truncation_note(returned: u64, total: u64) -> String`; `pub async fn write_logs<O: std::io::Write, E: std::io::Write>(client: &Client, url: &str, out: &mut O, err: &mut E) -> Result<()>`; `pub async fn cmd_logs(client: &Client, server: &str, job_id: &str, full: bool, tail_bytes: Option<u64>) -> Result<()>`.

- [ ] **Step 1: Write the failing tests**

Replace `crates/stroem-cli/src/remote/logs.rs` with the tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn logs_url_variants() {
        assert_eq!(logs_url("http://s", "j1", false, None), "http://s/api/jobs/j1/logs");
        assert_eq!(logs_url("http://s", "j1", false, Some(1024)), "http://s/api/jobs/j1/logs?tail_bytes=1024");
        assert_eq!(logs_url("http://s", "j1", true, None), "http://s/api/jobs/j1/logs?full=true");
    }

    #[test]
    fn truncation_note_names_both_sizes() {
        assert_eq!(
            truncation_note(262_144, 87_325_871),
            "note: showing the last 256.0 KiB of up to 83.3 MiB; use --full for the whole log"
        );
    }

    async fn run(server: &MockServer, full: bool, tail: Option<u64>) -> (Result<()>, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let url = logs_url(&server.uri(), "j1", full, tail);
        let result = write_logs(&Client::new(), &url, &mut out, &mut err).await;
        (result, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[tokio::test]
    async fn a_truncated_tail_prints_the_logs_and_a_note_on_stderr() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/j1/logs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "logs": "a\nb\n", "truncated": true, "total_bytes": 87_325_871u64, "returned_bytes": 262_144u64
            })))
            .mount(&server)
            .await;
        let (result, out, err) = run(&server, false, None).await;
        result.unwrap();
        assert_eq!(out, "a\nb\n");
        assert!(err.contains("256.0 KiB of up to 83.3 MiB"), "{err}");
    }

    #[tokio::test]
    async fn a_complete_tail_prints_no_note() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/j1/logs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "logs": "a\n", "truncated": false, "total_bytes": 2, "returned_bytes": 2
            })))
            .mount(&server)
            .await;
        let (result, out, err) = run(&server, false, None).await;
        result.unwrap();
        assert_eq!((out.as_str(), err.as_str()), ("a\n", ""));
    }

    #[tokio::test]
    async fn full_streams_the_ndjson_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/j1/logs"))
            .and(query_param("full", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(b"x\ny\n".to_vec(), "application/x-ndjson"))
            .mount(&server)
            .await;
        let (result, out, _) = run(&server, true, None).await;
        result.unwrap();
        assert_eq!(out, "x\ny\n");
    }

    #[tokio::test]
    async fn full_against_an_old_server_prints_the_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/j1/logs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"logs": "old\n"})))
            .mount(&server)
            .await;
        let (result, out, err) = run(&server, true, None).await;
        result.unwrap();
        assert_eq!((out.as_str(), err.as_str()), ("old\n", ""));
    }

    #[tokio::test]
    async fn tail_bytes_is_forwarded() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/j1/logs"))
            .and(query_param("tail_bytes", "1024"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"logs": "t\n"})))
            .mount(&server)
            .await;
        let (result, out, _) = run(&server, false, Some(1024)).await;
        result.unwrap();
        assert_eq!(out, "t\n");
    }

    #[tokio::test]
    async fn server_errors_surface_their_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/j1/logs"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "Job not found"})))
            .mount(&server)
            .await;
        let (result, _, _) = run(&server, false, None).await;
        assert!(format!("{:#}", result.unwrap_err()).contains("Job not found"));
    }
}
```

Add to the tests module of `remote/mod.rs`:

```rust
    #[test]
    fn logs_subcommand_flags() {
        match parse(&["stroem-api", "logs", "j1"]).unwrap().command {
            super::Commands::Logs { job_id, full, tail_bytes } => {
                assert_eq!(job_id, "j1");
                assert!(!full);
                assert!(tail_bytes.is_none());
            }
            _ => panic!("unexpected command variant"),
        }
        match parse(&["stroem-api", "logs", "j1", "--full"]).unwrap().command {
            super::Commands::Logs { full, .. } => assert!(full),
            _ => panic!("unexpected command variant"),
        }
        match parse(&["stroem-api", "logs", "j1", "--tail-bytes", "1024"]).unwrap().command {
            super::Commands::Logs { tail_bytes, .. } => assert_eq!(tail_bytes, Some(1024)),
            _ => panic!("unexpected command variant"),
        }
        assert!(parse(&["stroem-api", "logs", "j1", "--full", "--tail-bytes", "5"]).is_err());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p stroem-cli remote::`
Expected: compile errors — `cannot find function logs_url`, `no field full on Commands::Logs`.

- [ ] **Step 3: Implement**

Insert above the tests module in `logs.rs`:

```rust
use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::io::Write;

use super::client::check_response;

/// The default tail, a tail of `tail_bytes`, or the full stream.
pub fn logs_url(server: &str, job_id: &str, full: bool, tail_bytes: Option<u64>) -> String {
    let base = format!("{server}/api/jobs/{job_id}/logs");
    match (full, tail_bytes) {
        (true, _) => format!("{base}?full=true"),
        (false, Some(n)) => format!("{base}?tail_bytes={n}"),
        (false, None) => base,
    }
}

/// `262144` → `256.0 KiB`. Same format as the server's `log_read::human_bytes`
/// (the CLI does not depend on stroem-server).
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 3] = ["KiB", "MiB", "GiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

pub fn truncation_note(returned: u64, total: u64) -> String {
    format!(
        "note: showing the last {} of up to {}; use --full for the whole log",
        human_bytes(returned),
        human_bytes(total)
    )
}

/// Fetch `url` and print the log to `out`. An NDJSON body (`full=true` on a
/// current server) is copied as it arrives; a JSON envelope (a tail, or any
/// answer from an older server) prints `logs` and, when truncated, a note
/// to `err`.
pub async fn write_logs<O: Write, E: Write>(client: &Client, url: &str, out: &mut O, err: &mut E) -> Result<()> {
    let mut resp = client.get(url).send().await.context("Failed to connect to server")?;
    let status = resp.status();
    let ndjson = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/x-ndjson"));
    if status.is_success() && ndjson {
        while let Some(chunk) = resp.chunk().await.context("Failed to read the log stream")? {
            out.write_all(&chunk)?;
        }
        out.flush()?;
        return Ok(());
    }
    let body: Value = resp.json().await.context("Failed to parse response")?;
    check_response(&status, &body)?;
    let logs = body.get("logs").and_then(Value::as_str).unwrap_or("");
    out.write_all(logs.as_bytes())?;
    out.flush()?;
    if body.get("truncated").and_then(Value::as_bool) == Some(true) {
        let returned = body.get("returned_bytes").and_then(Value::as_u64).unwrap_or(logs.len() as u64);
        let total = body.get("total_bytes").and_then(Value::as_u64).unwrap_or(returned);
        writeln!(err, "{}", truncation_note(returned, total))?;
    }
    Ok(())
}

pub async fn cmd_logs(client: &Client, server: &str, job_id: &str, full: bool, tail_bytes: Option<u64>) -> Result<()> {
    let url = logs_url(server, job_id, full, tail_bytes);
    write_logs(client, &url, &mut std::io::stdout(), &mut std::io::stderr()).await
}
```

In `remote/mod.rs` change the variant:

```rust
    /// Get job logs (the last 256 KiB unless --full or --tail-bytes)
    Logs {
        /// Job ID
        job_id: String,
        /// Stream the whole log instead of its tail
        #[arg(long, conflicts_with = "tail_bytes")]
        full: bool,
        /// Bytes of the log's end to print (server default 256 KiB, max 4 MiB)
        #[arg(long)]
        tail_bytes: Option<u64>,
    },
```

and the dispatch arm:

```rust
        Commands::Logs { job_id, full, tail_bytes } => {
            logs::cmd_logs(&http_client, server, &job_id, full, tail_bytes).await?;
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p stroem-cli`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-cli/src/remote
git commit -m "feat(cli): stroem-api logs prints the tail by default; --full streams, --tail-bytes"
```

---

### Task 13: UI log helpers, `appendTail` and the API client

**Files:**
- Create: `ui/src/lib/log-lines.ts`, `ui/src/lib/log-tail.ts`, `ui/src/lib/download.ts`
- Create: `ui/src/lib/__tests__/log-lines.test.ts`, `ui/src/lib/__tests__/log-tail.test.ts`
- Modify: `ui/src/lib/api.ts:341-345` (`getJobLogs` type), `:472-479` (`getStepLogs`), new functions
- Modify: `ui/src/lib/__tests__/api.test.ts`

**Interfaces:**
- Produces: `LOG_GAP_MARKER: string`; `splitLogLines(body: string): string[]`; `estimateLineRows(raw: string, charsPerRow: number): number`; `formatBytesIEC(bytes: number): string`; `appendTail(displayed: readonly string[], tail: readonly string[]): { lines: string[]; gap: boolean }`; `saveBlob(blob: Blob, filename: string): void`; `interface LogTail { logs; truncated; total_bytes; returned_bytes }`; `getStepLogs(jobId, stepName): Promise<LogTail>`; `getStepLogsFull(jobId, stepName, onProgress?): Promise<string>`; `downloadStepLog(jobId, stepName): Promise<void>`.

- [ ] **Step 1: Write the failing tests**

`ui/src/lib/__tests__/log-tail.test.ts`:

```ts
import { describe, it, expect } from "vitest";
import { appendTail } from "../log-tail";
import { LOG_GAP_MARKER } from "../log-lines";

const plain = (lines: string[]) => lines.filter((l) => l !== LOG_GAP_MARKER);

describe("appendTail", () => {
  it("appends only what the view lacks on a plain overlap", () => {
    expect(appendTail(["a", "b", "c"], ["b", "c", "d"])).toEqual({ lines: ["a", "b", "c", "d"], gap: false });
  });

  it("is idempotent", () => {
    const once = appendTail(["a", "b"], ["b", "c"]).lines;
    expect(appendTail(once, ["b", "c"]).lines).toEqual(once);
  });

  it("does not duplicate under an arrival-ordered tail", () => {
    expect(appendTail(["B", "A", "C"], ["B", "A", "C"]).lines).toEqual(["B", "A", "C"]);
  });

  it("shows a line another replica recovered in the middle", () => {
    expect(appendTail(["A", "C"], ["A", "B", "C"]).lines).toEqual(["A", "B", "C"]);
  });

  it("appends a stale straggler once", () => {
    const once = appendTail(["a", "b", "c"], ["c", "s"]).lines;
    expect(once).toEqual(["a", "b", "c", "s"]);
    expect(appendTail(once, ["c", "s"]).lines).toEqual(once);
  });

  it("marks a gap only when no tail line was displayed", () => {
    expect(appendTail(["a", "b"], ["x", "y"])).toEqual({ lines: ["a", "b", LOG_GAP_MARKER, "x", "y"], gap: true });
    expect(appendTail([], ["x"])).toEqual({ lines: ["x"], gap: false });
  });

  it("keeps observed multiplicity", () => {
    expect(appendTail(["X", "X"], ["X"]).lines).toEqual(["X", "X"]);
    expect(plain(appendTail(["A"], ["B", "B"]).lines)).toEqual(["A", "B", "B"]);
    expect(plain(appendTail(["A", "A"], ["B"]).lines)).toEqual(["A", "A", "B"]);
  });

  it("keeps kept lines in their order and keeps gap markers", () => {
    expect(appendTail(["a", "b", "c", "d"], ["b", "d"]).lines).toEqual(["a", "c", "b", "d"]);
    expect(appendTail(["a", LOG_GAP_MARKER, "b"], ["b", "c"]).lines).toEqual(["a", LOG_GAP_MARKER, "b", "c"]);
  });

  it("returns the view unchanged for an empty tail", () => {
    expect(appendTail(["a"], [])).toEqual({ lines: ["a"], gap: false });
  });
});
```

`ui/src/lib/__tests__/log-lines.test.ts`:

```ts
import { describe, it, expect } from "vitest";
import { estimateLineRows, formatBytesIEC, splitLogLines } from "../log-lines";

describe("log line helpers", () => {
  it("splits a JSONL body into non-empty lines", () => {
    expect(splitLogLines("")).toEqual([]);
    expect(splitLogLines("a\n\nb\n")).toEqual(["a", "b"]);
    expect(splitLogLines("a\nb")).toEqual(["a", "b"]);
  });

  it("estimates wrapped rows, discounting the JSON envelope", () => {
    expect(estimateLineRows("short", 120)).toBe(1);
    expect(estimateLineRows("x".repeat(250), 100)).toBe(3);
    expect(estimateLineRows(`{"ts":"t","stream":"stdout","step":"s","line":"${"y".repeat(150)}"}`, 100)).toBe(2);
    expect(estimateLineRows("", 100)).toBe(1);
  });

  it("formats IEC sizes with one decimal", () => {
    expect(formatBytesIEC(512)).toBe("512 B");
    expect(formatBytesIEC(262_144)).toBe("256.0 KiB");
    expect(formatBytesIEC(87_325_871)).toBe("83.3 MiB");
  });
});
```

Append to `ui/src/lib/__tests__/api.test.ts` (add `getStepLogs, getStepLogsFull` to the import list):

```ts
describe("step log reads", () => {
  it("getStepLogs fills fields an older server does not send", async () => {
    setAccessToken("t");
    stubFetch(200, { logs: "a\n" });
    expect(await getStepLogs("j", "build")).toEqual({ logs: "a\n", truncated: false, total_bytes: 2, returned_bytes: 2 });
  });

  it("getStepLogsFull reads an NDJSON stream and reports progress", async () => {
    setAccessToken("t");
    const fetchMock = vi.fn().mockResolvedValue(
      new Response("a\nb\n", { status: 200, headers: { "content-type": "application/x-ndjson" } }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const progress: number[] = [];
    expect(await getStepLogsFull("j", "build", (n) => progress.push(n))).toBe("a\nb\n");
    expect(progress.at(-1)).toBe(4);
    expect(fetchMock.mock.calls[0][0]).toBe("/api/jobs/j/steps/build/logs?full=true");
  });

  it("getStepLogsFull reads the envelope of a server without full mode", async () => {
    setAccessToken("t");
    stubFetch(200, { logs: "old\n" }, { "content-type": "application/json" });
    expect(await getStepLogsFull("j", "build")).toBe("old\n");
  });
});
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd ui && bun run test src/lib/__tests__/log-tail.test.ts src/lib/__tests__/log-lines.test.ts src/lib/__tests__/api.test.ts`
Expected: FAIL — modules and exports missing.

- [ ] **Step 3: Implement**

`ui/src/lib/log-lines.ts`:

```ts
/** A row the viewer renders as a "lines missing" divider. NUL never
 * appears in JSONL, so it cannot collide with a real line. */
export const LOG_GAP_MARKER = "\u0000stroem-gap";

/** Split a JSONL body into its non-empty lines. */
export function splitLogLines(body: string): string[] {
  if (!body) return [];
  return body.split("\n").filter((line) => line.length > 0);
}

/** Bytes of JSON around the text of a typical line
 * (`{"ts":…,"stream":…,"step":…,"line":…}`). */
const JSON_OVERHEAD = 70;

/** Estimated wrapped rows of a raw line; the virtualiser measures the
 * real height once the row renders. */
export function estimateLineRows(raw: string, charsPerRow: number): number {
  const visible = raw.startsWith("{") ? raw.length - JSON_OVERHEAD : raw.length;
  return Math.max(1, Math.ceil(Math.max(visible, 1) / Math.max(charsPerRow, 1)));
}

/** `262144` → `256.0 KiB` (IEC units, one decimal). */
export function formatBytesIEC(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(1)} ${units[unit]}`;
}
```

`ui/src/lib/log-tail.ts`:

```ts
import { LOG_GAP_MARKER } from "./log-lines";

export interface AppendResult {
  lines: string[];
  gap: boolean;
}

/**
 * Stitch a freshly polled tail onto the lines the viewer shows in full mode
 * (spec § 3.6): remove from `displayed`, newest first, as many copies of
 * each line as the tail holds, then append the tail. Nothing about time —
 * a file window is not a time range. Idempotent, never duplicates under an
 * arrival-ordered tail, keeps the multiplicity observed so far. When no
 * tail line was displayed, a gap marker separates the view from the tail.
 */
export function appendTail(displayed: readonly string[], tail: readonly string[]): AppendResult {
  if (tail.length === 0) return { lines: [...displayed], gap: false };
  const counts = new Map<string, number>();
  for (const line of tail) counts.set(line, (counts.get(line) ?? 0) + 1);
  const keep = new Array<boolean>(displayed.length).fill(true);
  let remaining = tail.length;
  let removed = 0;
  for (let i = displayed.length - 1; i >= 0 && remaining > 0; i--) {
    const count = counts.get(displayed[i]);
    if (count) {
      counts.set(displayed[i], count - 1);
      keep[i] = false;
      removed += 1;
      remaining -= 1;
    }
  }
  const kept = displayed.filter((_, i) => keep[i]);
  const gap = displayed.length > 0 && removed === 0;
  return { lines: gap ? kept.concat([LOG_GAP_MARKER], tail) : kept.concat(tail), gap };
}
```

`ui/src/lib/download.ts`:

```ts
/** Save `blob` as `filename` through a temporary object URL. */
export function saveBlob(blob: Blob, filename: string): void {
  const url = URL.createObjectURL(blob);
  try {
    const a = document.createElement("a");
    a.href = url;
    a.download = filename;
    // Some browsers need the anchor in the DOM before .click().
    document.body.appendChild(a);
    a.click();
    a.remove();
  } finally {
    // Give the browser time to start the download before revoking.
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
  }
}
```

In `ui/src/lib/api.ts` add `import { saveBlob } from "./download";`, replace `getStepLogs` (and retype `getJobLogs` to `Promise<LogTail>` via `apiFetch<LogTail>`), and add:

```ts
/** A bounded read of a log's end (spec 2026-09-22-log-tail-streaming § 3.1). */
export interface LogTail {
  logs: string;
  /** Something that exists was left out; `false` is exact. */
  truncated: boolean;
  /** Upper bound on the size of the whole job log. */
  total_bytes: number;
  returned_bytes: number;
}

function stepLogsUrl(jobId: string, stepName: string): string {
  return `/api/jobs/${jobId}/steps/${encodeURIComponent(stepName)}/logs`;
}

/** The end of a step's log. Older servers answer `{logs}` only; the
 * missing fields then mean "complete". */
export async function getStepLogs(jobId: string, stepName: string): Promise<LogTail> {
  const data = await apiFetch<Partial<LogTail>>(stepLogsUrl(jobId, stepName));
  const logs = data.logs ?? "";
  return {
    logs,
    truncated: data.truncated ?? false,
    total_bytes: data.total_bytes ?? logs.length,
    returned_bytes: data.returned_bytes ?? logs.length,
  };
}

/** The whole step log, streamed; `onProgress` gets the bytes received. */
export async function getStepLogsFull(
  jobId: string,
  stepName: string,
  onProgress?: (bytes: number) => void,
): Promise<string> {
  const res = await apiFetchRaw(`${stepLogsUrl(jobId, stepName)}?full=true`);
  if ((res.headers.get("content-type") ?? "").includes("application/json")) {
    // A server without `full=true` answers with the tail envelope.
    const body = (await res.json()) as { logs?: string };
    return body.logs ?? "";
  }
  if (!res.body) return res.text();
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  const parts: string[] = [];
  let received = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    received += value.byteLength;
    parts.push(decoder.decode(value, { stream: true }));
    onProgress?.(received);
  }
  parts.push(decoder.decode());
  return parts.join("");
}

/** Save the whole step log as `<job8>-<step>.jsonl`. */
export async function downloadStepLog(jobId: string, stepName: string): Promise<void> {
  const res = await apiFetchRaw(`${stepLogsUrl(jobId, stepName)}?full=true`);
  saveBlob(await res.blob(), `${jobId.slice(0, 8)}-${stepName}.jsonl`);
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd ui && bun run test src/lib && bunx tsc -b && bun run lint`
Expected: PASS; typecheck and lint clean (the components still compile: `data.logs` exists on `LogTail`).

- [ ] **Step 5: Commit**

```bash
git add ui/src/lib
git commit -m "feat(ui): log tail client, full-log stream, appendTail and line helpers"
```

---

### Task 14: Virtualised log viewer and the tail banner

**Files:**
- Modify: `ui/package.json`, `ui/bun.lock` (add `@tanstack/react-virtual`)
- Modify: `ui/src/components/log-viewer.tsx` (rewrite)
- Create: `ui/src/components/log-tail-banner.tsx`
- Create: `ui/src/components/__tests__/log-viewer.test.tsx`, `ui/src/components/__tests__/log-tail-banner.test.tsx`
- Modify: `ui/vitest.setup.ts` (`Element.prototype.scrollTo` polyfill)

**Interfaces:**
- Consumes: `LOG_GAP_MARKER`, `splitLogLines`, `estimateLineRows`, `formatBytesIEC` (Task 13).
- Produces: `LogViewer({ logs: string | readonly string[]; isStreaming: boolean; header?: ReactNode })`; `export type FullLogState = "idle" | "loading" | "loaded" | "error"`; `LogTailBanner({ lineCount, returnedBytes, totalBytes, fullState, progressBytes, onLoadFull, onDownload })`.

- [ ] **Step 1: Add the dependency and the jsdom polyfill**

Run: `cd ui && bun add @tanstack/react-virtual@^3.14`

In `ui/vitest.setup.ts`, inside the existing `if (typeof Element !== "undefined")` block, add:

```ts
  if (!Element.prototype.scrollTo) {
    Element.prototype.scrollTo = () => {};
  }
```

- [ ] **Step 2: Write the failing tests**

`ui/src/components/__tests__/log-viewer.test.tsx`:

```tsx
import { describe, it, expect, beforeAll, afterAll, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { LogViewer } from "../log-viewer";
import { LOG_GAP_MARKER } from "@/lib/log-lines";

// jsdom has no layout: give every element a 20px box so the virtualiser
// can measure rows (the TanStack Virtual testing advice).
beforeAll(() => {
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(
    () => ({ x: 0, y: 0, top: 0, left: 0, bottom: 20, right: 800, width: 800, height: 20, toJSON: () => ({}) }) as DOMRect,
  );
});
afterAll(() => vi.restoreAllMocks());

const jsonl = (i: number, stream = "stdout") =>
  JSON.stringify({ ts: "2026-09-22T06:01:04.123Z", stream, step: "s", line: `line ${i}` });

describe("LogViewer", () => {
  it("shows a placeholder for an empty log", () => {
    render(<LogViewer logs="" isStreaming={false} />);
    expect(screen.getByText("Waiting for logs...")).toBeInTheDocument();
  });

  it("renders only a window of a long log", () => {
    const lines = Array.from({ length: 10_000 }, (_, i) => jsonl(i));
    const { container } = render(<LogViewer logs={lines} isStreaming={false} />);
    const rows = container.querySelectorAll("[data-index]");
    expect(rows.length).toBeGreaterThan(0);
    expect(rows.length).toBeLessThan(200);
  });

  it("parses JSONL rows from a raw body, with stderr styling", () => {
    render(<LogViewer logs={`${jsonl(1)}\n${jsonl(2, "stderr")}\n`} isStreaming={false} />);
    expect(screen.getByText("line 1")).toBeInTheDocument();
    expect(screen.getByText("line 2")).toHaveAttribute("data-stream", "stderr");
  });

  it("renders legacy plain-text lines as they are", () => {
    render(<LogViewer logs={"plain legacy line\n"} isStreaming={false} />);
    expect(screen.getByText("plain legacy line")).toBeInTheDocument();
  });

  it("renders the gap marker as a separator", () => {
    render(<LogViewer logs={[jsonl(1), LOG_GAP_MARKER, jsonl(2)]} isStreaming={false} />);
    expect(screen.getByTestId("log-gap")).toBeInTheDocument();
  });

  it("renders a header and the live badge", () => {
    render(<LogViewer logs="" isStreaming header={<div>banner here</div>} />);
    expect(screen.getByText("banner here")).toBeInTheDocument();
    expect(screen.getByText("Log streaming is active")).toBeInTheDocument();
  });
});
```

`ui/src/components/__tests__/log-tail-banner.test.tsx`:

```tsx
import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { LogTailBanner } from "../log-tail-banner";

const base = {
  lineCount: 2100,
  returnedBytes: 262_144,
  totalBytes: 87_325_871,
  fullState: "idle" as const,
  progressBytes: 0,
  onLoadFull: vi.fn(),
  onDownload: vi.fn(),
};

describe("LogTailBanner", () => {
  it("says how much of the log is shown", () => {
    render(<LogTailBanner {...base} />);
    expect(screen.getByTestId("log-tail-banner")).toHaveTextContent(
      "Showing the last ~2,100 lines (256.0 KiB of up to 83.3 MiB).",
    );
  });

  it("offers load and download", () => {
    const onLoadFull = vi.fn();
    const onDownload = vi.fn();
    render(<LogTailBanner {...base} onLoadFull={onLoadFull} onDownload={onDownload} />);
    fireEvent.click(screen.getByRole("button", { name: "Load full log" }));
    fireEvent.click(screen.getByRole("button", { name: /Download/ }));
    expect(onLoadFull).toHaveBeenCalledOnce();
    expect(onDownload).toHaveBeenCalledOnce();
  });

  it("shows progress while loading and an error after a failure", () => {
    const { rerender } = render(<LogTailBanner {...base} fullState="loading" progressBytes={3 * 1024 * 1024} />);
    expect(screen.getByRole("button", { name: "Loading… 3.0 MiB" })).toBeDisabled();
    rerender(<LogTailBanner {...base} fullState="error" />);
    expect(screen.getByText("Could not load the full log.")).toBeInTheDocument();
  });
});
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cd ui && bun run test src/components/__tests__/log-viewer.test.tsx src/components/__tests__/log-tail-banner.test.tsx`
Expected: FAIL — no `LogTailBanner`; the long-log test renders 10 000 rows.

- [ ] **Step 4: Implement**

`ui/src/components/log-tail-banner.tsx`:

```tsx
import { Download } from "lucide-react";
import { Button } from "@/components/ui/button";
import { formatBytesIEC } from "@/lib/log-lines";

export type FullLogState = "idle" | "loading" | "loaded" | "error";

interface LogTailBannerProps {
  lineCount: number;
  returnedBytes: number;
  totalBytes: number;
  fullState: FullLogState;
  progressBytes: number;
  onLoadFull: () => void;
  onDownload: () => void;
}

export function LogTailBanner({
  lineCount,
  returnedBytes,
  totalBytes,
  fullState,
  progressBytes,
  onLoadFull,
  onDownload,
}: LogTailBannerProps) {
  const loading = fullState === "loading";
  return (
    <div
      data-testid="log-tail-banner"
      role="status"
      className="mb-2 flex flex-wrap items-center gap-2 rounded-md border border-amber-300 bg-amber-50 px-3 py-1.5 text-xs text-amber-900 dark:border-amber-700 dark:bg-amber-950 dark:text-amber-100"
    >
      <span className="grow">
        Showing the last ~{lineCount.toLocaleString("en-US")} lines ({formatBytesIEC(returnedBytes)} of up to{" "}
        {formatBytesIEC(totalBytes)}).
      </span>
      {fullState === "error" && (
        <span className="text-red-600 dark:text-red-400">Could not load the full log.</span>
      )}
      <Button type="button" size="sm" variant="outline" disabled={loading} onClick={onLoadFull}>
        {loading ? `Loading… ${formatBytesIEC(progressBytes)}` : "Load full log"}
      </Button>
      <Button type="button" size="sm" variant="ghost" onClick={onDownload}>
        <Download className="mr-1 h-3.5 w-3.5" aria-hidden="true" />
        Download
      </Button>
    </div>
  );
}
```

Replace `ui/src/components/log-viewer.tsx`:

```tsx
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { LOG_GAP_MARKER, estimateLineRows, splitLogLines } from "@/lib/log-lines";

interface ParsedLogLine {
  ts: string;
  stream: string;
  step: string;
  line: string;
}

interface LogViewerProps {
  /** A raw JSONL body, or lines already split (may contain LOG_GAP_MARKER). */
  logs: string | readonly string[];
  isStreaming: boolean;
  /** Rendered above the log, e.g. the tail banner. */
  header?: ReactNode;
}

/** text-xs (12px) × leading-relaxed (1.625). */
const LINE_HEIGHT_PX = 20;
const DEFAULT_CHARS_PER_ROW = 120;
const PROBE_CHARS = 20;
/** Timestamp column and horizontal padding, subtracted before chars/row. */
const RESERVED_PX = 140;

function parseLogLine(raw: string): ParsedLogLine | null {
  try {
    const parsed = JSON.parse(raw);
    if (parsed && typeof parsed.line === "string") {
      return {
        ts: parsed.ts || "",
        stream: parsed.stream || "stdout",
        step: parsed.step || "",
        line: parsed.line,
      };
    }
  } catch {
    // Not JSON — legacy plain text line
  }
  return null;
}

function formatTimestamp(ts: string): string {
  if (!ts) return "";
  try {
    return new Date(ts).toISOString().substring(11, 23); // HH:MM:SS.mmm
  } catch {
    return "";
  }
}

/** One row; parsed on render, so only visible rows cost a JSON.parse. */
function LogLine({ raw }: { raw: string }) {
  if (raw === LOG_GAP_MARKER) {
    return (
      <div
        role="separator"
        data-testid="log-gap"
        className="my-1 border-t border-dashed border-amber-500/60 pt-0.5 text-center text-[10px] uppercase tracking-wider text-amber-500"
      >
        Lines missing here — load the full log again to fill the gap
      </div>
    );
  }
  const parsed = parseLogLine(raw);
  if (!parsed) return <div className="text-zinc-300">{raw}</div>;
  const ts = formatTimestamp(parsed.ts);
  return (
    <div className="flex">
      {ts && <span className="mr-3 shrink-0 select-none text-zinc-600">{ts}</span>}
      <span
        className={parsed.stream === "stderr" ? "text-red-400" : "text-zinc-300"}
        data-stream={parsed.stream}
      >
        {parsed.line}
      </span>
    </div>
  );
}

export function LogViewer({ logs, isStreaming, header }: LogViewerProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const probeRef = useRef<HTMLSpanElement>(null);
  const autoScrollRef = useRef(true);
  const [charsPerRow, setCharsPerRow] = useState(DEFAULT_CHARS_PER_ROW);
  const lines = useMemo(() => (typeof logs === "string" ? splitLogLines(logs) : logs), [logs]);

  const virtualizer = useVirtualizer({
    count: lines.length,
    getScrollElement: () => containerRef.current,
    estimateSize: (i) => estimateLineRows(lines[i] ?? "", charsPerRow) * LINE_HEIGHT_PX,
    overscan: 20,
    // No layout before the first paint (and none in jsdom); the observed
    // size replaces this.
    initialRect: { width: 800, height: 500 },
  });

  // Characters per row from one measured character and the container width.
  useEffect(() => {
    const el = containerRef.current;
    const probe = probeRef.current;
    if (!el || !probe) return;
    const update = () => {
      const charWidth = probe.getBoundingClientRect().width / PROBE_CHARS;
      if (charWidth > 0 && el.clientWidth > 0) {
        setCharsPerRow(Math.max(20, Math.floor((el.clientWidth - RESERVED_PX) / charWidth)));
      }
    };
    update();
    const observer = new ResizeObserver(update);
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  // Follow the end while the user is at the bottom.
  useEffect(() => {
    if (autoScrollRef.current && lines.length > 0) {
      virtualizer.scrollToIndex(lines.length - 1, { align: "end" });
    }
  }, [lines, virtualizer]);

  function handleScroll() {
    const el = containerRef.current;
    if (!el) return;
    autoScrollRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
  }

  return (
    <div>
      {header}
      <div className="relative">
        {isStreaming && (
          <div className="absolute right-3 top-3 z-10 flex items-center gap-1.5">
            <span className="h-2 w-2 animate-pulse rounded-full bg-green-500" aria-hidden="true" />
            <span
              className="text-[10px] font-medium uppercase tracking-wider text-green-600 dark:text-green-400"
              aria-hidden="true"
            >
              Live
            </span>
            <span className="sr-only">Log streaming is active</span>
          </div>
        )}
        <div
          ref={containerRef}
          onScroll={handleScroll}
          role="log"
          aria-label="Job execution logs"
          aria-live="polite"
          className="relative max-h-[500px] min-h-[200px] overflow-auto rounded-lg bg-zinc-950 p-4 font-mono text-xs leading-relaxed"
        >
          <span ref={probeRef} aria-hidden="true" className="invisible absolute whitespace-pre">
            {"0".repeat(PROBE_CHARS)}
          </span>
          {lines.length === 0 ? (
            <span className="text-zinc-600">Waiting for logs...</span>
          ) : (
            <div className="relative w-full" style={{ height: virtualizer.getTotalSize() }}>
              {virtualizer.getVirtualItems().map((item) => (
                <div
                  key={item.key}
                  data-index={item.index}
                  ref={virtualizer.measureElement}
                  className="absolute left-0 top-0 w-full"
                  style={{ transform: `translateY(${item.start}px)` }}
                >
                  <LogLine raw={lines[item.index]} />
                </div>
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
```

- [ ] **Step 5: Run the tests, typecheck and lint**

Run: `cd ui && bun run test src/components && bunx tsc -b && bun run lint`
Expected: PASS (existing `step-detail`/`job-detail` tests still pass: `LogViewer` still accepts a string).

- [ ] **Step 6: Commit**

```bash
git add ui/package.json ui/bun.lock ui/vitest.setup.ts ui/src/components
git commit -m "feat(ui): virtualised log viewer and tail banner"
```

---

### Task 15: `useStepLog` and the step / server-event panels

**Files:**
- Create: `ui/src/hooks/use-step-log.ts`, `ui/src/hooks/__tests__/use-step-log.test.ts`
- Modify: `ui/src/components/step-detail.tsx:1-100`, `:198-206`
- Modify: `ui/src/components/server-events.tsx` (rewrite)
- Create: `ui/src/components/__tests__/server-events.test.tsx`
- Modify: `ui/src/components/__tests__/step-detail.test.tsx`

**Interfaces:**
- Consumes: `getStepLogs`, `getStepLogsFull`, `downloadStepLog`, `LogTail` (Task 13); `appendTail`, `splitLogLines`, `formatBytesIEC` (Task 13); `LogViewer`, `LogTailBanner`, `FullLogState` (Task 14).
- Produces: `useStepLog(jobId: string, stepName: string, opts: { enabled: boolean; pollMs: number | null }): StepLog`; `FULL_LOAD_CONFIRM_BYTES = 64 MiB`.

- [ ] **Step 1: Write the failing hook tests**

`ui/src/hooks/__tests__/use-step-log.test.ts`:

```ts
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { renderHook, act, waitFor } from "@testing-library/react";
import { useStepLog } from "../use-step-log";
import { LOG_GAP_MARKER } from "@/lib/log-lines";

const getStepLogs = vi.fn();
const getStepLogsFull = vi.fn();
const downloadStepLog = vi.fn();
vi.mock("@/lib/api", () => ({
  getStepLogs: (...a: unknown[]) => getStepLogs(...a),
  getStepLogsFull: (...a: unknown[]) => getStepLogsFull(...a),
  downloadStepLog: (...a: unknown[]) => downloadStepLog(...a),
}));

const tail = (logs: string, over: Partial<{ truncated: boolean; total_bytes: number }> = {}) => ({
  logs,
  truncated: false,
  total_bytes: logs.length,
  returned_bytes: logs.length,
  ...over,
});

beforeEach(() => {
  getStepLogs.mockReset();
  getStepLogsFull.mockReset();
  downloadStepLog.mockReset().mockResolvedValue(undefined);
});
afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("useStepLog", () => {
  it("fetches once without polling and exposes the tail metadata", async () => {
    getStepLogs.mockResolvedValue(tail("a\nb\n", { truncated: true, total_bytes: 999 }));
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: null }));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.lines).toEqual(["a", "b"]);
    expect(result.current.truncated).toBe(true);
    expect(result.current.totalBytes).toBe(999);
    expect(getStepLogs).toHaveBeenCalledTimes(1);
  });

  it("does nothing when disabled", async () => {
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: false, pollMs: 2000 }));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(getStepLogs).not.toHaveBeenCalled();
  });

  it("keeps the last non-empty body when a poll comes back empty", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs.mockResolvedValueOnce(tail("a\n")).mockResolvedValue(tail(""));
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));
    await waitFor(() => expect(result.current.lines).toEqual(["a"]));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });
    expect(getStepLogs.mock.calls.length).toBeGreaterThanOrEqual(2);
    expect(result.current.lines).toEqual(["a"]);
  });

  it("loads the full log, then stitches later polls onto it", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs
      .mockResolvedValueOnce(tail("c\nd\n", { truncated: true }))
      .mockResolvedValue(tail("d\ne\n", { truncated: true }));
    getStepLogsFull.mockResolvedValue("a\nb\nc\nd\n");
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));
    await waitFor(() => expect(result.current.truncated).toBe(true));
    await act(async () => {
      result.current.loadFull();
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d"]);
    expect(result.current.truncated).toBe(false);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });
    await waitFor(() => expect(result.current.lines).toEqual(["a", "b", "c", "d", "e"]));
  });

  it("marks a gap when a poll shares no line with the full view", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs.mockResolvedValueOnce(tail("b\n", { truncated: true })).mockResolvedValue(tail("y\nz\n", { truncated: true }));
    getStepLogsFull.mockResolvedValue("a\nb\n");
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));
    await waitFor(() => expect(result.current.truncated).toBe(true));
    await act(async () => {
      result.current.loadFull();
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });
    await waitFor(() => expect(result.current.lines).toEqual(["a", "b", LOG_GAP_MARKER, "y", "z"]));
  });

  it("asks before loading more than 64 MiB", async () => {
    getStepLogs.mockResolvedValue(tail("a\n", { truncated: true, total_bytes: 65 * 1024 * 1024 }));
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: null }));
    await waitFor(() => expect(result.current.truncated).toBe(true));
    await act(async () => {
      result.current.loadFull();
    });
    expect(confirm).toHaveBeenCalled();
    expect(getStepLogsFull).not.toHaveBeenCalled();
  });

  it("reports a failed full load", async () => {
    getStepLogs.mockResolvedValue(tail("a\n", { truncated: true }));
    getStepLogsFull.mockRejectedValue(new Error("boom"));
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: null }));
    await waitFor(() => expect(result.current.truncated).toBe(true));
    await act(async () => {
      result.current.loadFull();
    });
    await waitFor(() => expect(result.current.fullState).toBe("error"));
    expect(result.current.lines).toEqual(["a"]);
  });

  it("starts over when the step changes", async () => {
    getStepLogs.mockImplementation((_job: string, step: string) => Promise.resolve(tail(`${step}-line\n`)));
    const { result, rerender } = renderHook(
      ({ step }) => useStepLog("j", step, { enabled: true, pollMs: null }),
      { initialProps: { step: "a" } },
    );
    await waitFor(() => expect(result.current.lines).toEqual(["a-line"]));
    rerender({ step: "b" });
    await waitFor(() => expect(result.current.lines).toEqual(["b-line"]));
  });
});
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd ui && bun run test src/hooks/__tests__/use-step-log.test.ts`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement the hook**

`ui/src/hooks/use-step-log.ts`:

```ts
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { downloadStepLog, getStepLogs, getStepLogsFull, type LogTail } from "@/lib/api";
import { appendTail } from "@/lib/log-tail";
import { formatBytesIEC, splitLogLines } from "@/lib/log-lines";
import type { FullLogState } from "@/components/log-tail-banner";

/** Above this the browser would hold a very large string: ask first. */
export const FULL_LOAD_CONFIRM_BYTES = 64 * 1024 * 1024;

export interface StepLog {
  /** Tail lines, or the full log (with gap markers) once loaded. */
  lines: readonly string[];
  truncated: boolean;
  returnedBytes: number;
  totalBytes: number;
  loading: boolean;
  fullState: FullLogState;
  progressBytes: number;
  loadFull: () => void;
  download: () => void;
}

interface Options {
  enabled: boolean;
  /** Poll interval while the step is live; `null` fetches once. */
  pollMs: number | null;
}

/**
 * The log panel's data: polls the step's tail, keeps the last non-empty
 * body when a poll lands on a replica without this job's chunks, and — once
 * the user loads the full log — stitches every later tail onto it with
 * `appendTail`.
 */
export function useStepLog(jobId: string, stepName: string, { enabled, pollMs }: Options): StepLog {
  const [tail, setTail] = useState<LogTail | null>(null);
  const [fullLines, setFullLines] = useState<string[] | null>(null);
  const [fullState, setFullState] = useState<FullLogState>("idle");
  const [progressBytes, setProgressBytes] = useState(0);
  const [loading, setLoading] = useState(enabled);
  const hasLogsRef = useRef(false);
  const fullRef = useRef<string[] | null>(null);
  const generationRef = useRef(0);

  // A different step (or job) starts from scratch.
  useEffect(() => {
    generationRef.current += 1;
    hasLogsRef.current = false;
    fullRef.current = null;
    setTail(null);
    setFullLines(null);
    setFullState("idle");
    setProgressBytes(0);
    setLoading(enabled);
  }, [jobId, stepName, enabled]);

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    async function fetchTail() {
      try {
        const data = await getStepLogs(jobId, stepName);
        if (cancelled) return;
        // With multi-replica servers a poll can land on a replica that has
        // not received this job's chunks and return "". Keep what we have.
        if (data.logs) {
          hasLogsRef.current = true;
          setTail(data);
          if (fullRef.current) {
            const { lines } = appendTail(fullRef.current, splitLogLines(data.logs));
            fullRef.current = lines;
            setFullLines(lines);
          }
        } else if (!hasLogsRef.current) {
          setTail(data);
        }
      } catch {
        // Logs may not exist yet.
      } finally {
        if (!cancelled) setLoading(false);
      }
    }
    void fetchTail();
    if (pollMs == null) {
      return () => {
        cancelled = true;
      };
    }
    const id = setInterval(fetchTail, pollMs);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [jobId, stepName, enabled, pollMs]);

  const loadFull = useCallback(async () => {
    if (
      tail &&
      tail.total_bytes > FULL_LOAD_CONFIRM_BYTES &&
      !window.confirm(
        `This log is up to ${formatBytesIEC(tail.total_bytes)}. Loading all of it may slow this tab down; Download is lighter. Load it anyway?`,
      )
    ) {
      return;
    }
    const generation = generationRef.current;
    setFullState("loading");
    setProgressBytes(0);
    try {
      const text = await getStepLogsFull(jobId, stepName, (n) => {
        if (generationRef.current === generation) setProgressBytes(n);
      });
      if (generationRef.current !== generation) return;
      const lines = splitLogLines(text);
      fullRef.current = lines;
      setFullLines(lines);
      setFullState("loaded");
    } catch {
      if (generationRef.current === generation) setFullState("error");
    }
  }, [jobId, stepName, tail]);

  const download = useCallback(() => {
    void downloadStepLog(jobId, stepName).catch(() => {});
  }, [jobId, stepName]);

  const tailLines = useMemo(() => splitLogLines(tail?.logs ?? ""), [tail]);

  return {
    lines: fullLines ?? tailLines,
    truncated: fullLines ? false : (tail?.truncated ?? false),
    returnedBytes: tail?.returned_bytes ?? 0,
    totalBytes: tail?.total_bytes ?? 0,
    loading,
    fullState,
    progressBytes,
    loadFull: () => void loadFull(),
    download,
  };
}
```

If `bun run lint` flags the `setState` calls in the reset effect (`react-hooks/set-state-in-effect`), keep the effect and add a one-line `// eslint-disable-next-line` with the reason "resetting per-step state on identity change" — `step-detail.tsx` already resets state the same way.

- [ ] **Step 4: Run the hook tests**

Run: `cd ui && bun run test src/hooks/__tests__/use-step-log.test.ts`
Expected: 8 tests PASS.

- [ ] **Step 5: Wire the panels**

In `step-detail.tsx`: remove the `logs`/`loadingLogs` state, `hasLogsRef`, the fetch `useEffect` and the `getStepLogs` import; add imports for `useStepLog` and `LogTailBanner`; and use:

```tsx
  const isActive = step.status === "running" || step.status === "ready";
  const log = useStepLog(jobId, step.step_name, {
    enabled: !isCarriedOver && !isSkipped,
    pollMs: isActive ? 2000 : null,
  });
  const showBanner = log.truncated || log.fullState === "loading" || log.fullState === "error";
```

Replace the `loadingLogs ? … : <LogViewer … />` branch (`:198-206`) with:

```tsx
          ) : log.loading ? (
            <div className="flex items-center justify-center py-8">
              <div className="h-5 w-5 animate-spin rounded-full border-2 border-muted border-t-primary" />
            </div>
          ) : (
            <LogViewer
              logs={log.lines}
              isStreaming={isStreaming}
              header={
                showBanner ? (
                  <LogTailBanner
                    lineCount={log.lines.length}
                    returnedBytes={log.returnedBytes}
                    totalBytes={log.totalBytes}
                    fullState={log.fullState}
                    progressBytes={log.progressBytes}
                    onLoadFull={log.loadFull}
                    onDownload={log.download}
                  />
                ) : null
              }
            />
          )}
```

Replace `ui/src/components/server-events.tsx` with:

```tsx
import { AlertCircle } from "lucide-react";
import { LogViewer } from "@/components/log-viewer";
import { LogTailBanner } from "@/components/log-tail-banner";
import { useStepLog } from "@/hooks/use-step-log";

interface ServerEventsProps {
  jobId: string;
  jobStatus: string;
}

export function ServerEvents({ jobId, jobStatus }: ServerEventsProps) {
  // Poll while the job is active. The switch to terminal changes `pollMs`,
  // which runs one final fetch: hook errors are written after completion.
  const isActive = jobStatus === "pending" || jobStatus === "running";
  const log = useStepLog(jobId, "_server", { enabled: true, pollMs: isActive ? 3000 : null });
  const showBanner = log.truncated || log.fullState === "loading" || log.fullState === "error";

  // An empty but truncated tail (its only record was torn) still shows the
  // banner, so the full log stays reachable.
  if (log.lines.length === 0 && !showBanner) return null;

  return (
    <div className="rounded-lg border border-amber-300 bg-amber-50 dark:border-amber-700 dark:bg-amber-950">
      <div className="flex items-center gap-2 border-b border-amber-200 px-4 py-2.5 dark:border-amber-800">
        <AlertCircle className="h-4 w-4 text-amber-600 dark:text-amber-400" />
        <span className="text-sm font-medium text-amber-800 dark:text-amber-200">Server Events</span>
      </div>
      <div className="p-2">
        <LogViewer
          logs={log.lines}
          isStreaming={false}
          header={
            showBanner ? (
              <LogTailBanner
                lineCount={log.lines.length}
                returnedBytes={log.returnedBytes}
                totalBytes={log.totalBytes}
                fullState={log.fullState}
                progressBytes={log.progressBytes}
                onLoadFull={log.loadFull}
                onDownload={log.download}
              />
            ) : null
          }
        />
      </div>
    </div>
  );
}
```

- [ ] **Step 6: Write the panel tests**

`ui/src/components/__tests__/server-events.test.tsx`:

```tsx
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { ServerEvents } from "../server-events";

const getStepLogs = vi.fn();
vi.mock("@/lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/lib/api")>()),
  getStepLogs: (...a: unknown[]) => getStepLogs(...a),
}));

const serverLine = JSON.stringify({ ts: "2026-09-22T06:00:00Z", stream: "stderr", step: "_server", line: "hook failed" });

beforeEach(() => getStepLogs.mockReset());

describe("ServerEvents", () => {
  it("renders nothing without events", async () => {
    getStepLogs.mockResolvedValue({ logs: "", truncated: false, total_bytes: 0, returned_bytes: 0 });
    const { container } = render(<ServerEvents jobId="j" jobStatus="completed" />);
    await waitFor(() => expect(getStepLogs).toHaveBeenCalled());
    expect(container).toBeEmptyDOMElement();
  });

  it("renders the events", async () => {
    getStepLogs.mockResolvedValue({ logs: `${serverLine}\n`, truncated: false, total_bytes: 90, returned_bytes: 90 });
    render(<ServerEvents jobId="j" jobStatus="completed" />);
    expect(await screen.findByText("hook failed")).toBeInTheDocument();
    expect(screen.queryByTestId("log-tail-banner")).not.toBeInTheDocument();
  });

  it("shows the banner for an empty but truncated tail", async () => {
    getStepLogs.mockResolvedValue({ logs: "", truncated: true, total_bytes: 40, returned_bytes: 0 });
    render(<ServerEvents jobId="j" jobStatus="completed" />);
    expect(await screen.findByTestId("log-tail-banner")).toBeInTheDocument();
    expect(screen.getByText("Server Events")).toBeInTheDocument();
  });
});
```

In `step-detail.test.tsx`, add `getStepLogsFull` to the mocked module (`getStepLogsFull: (...args: unknown[]) => getStepLogsFull(...args)` with `const getStepLogsFull = vi.fn();`) and append:

```tsx
describe("StepDetail tail banner", () => {
  it("offers the full log when the tail is truncated", async () => {
    getStepLogs.mockResolvedValue({
      logs: JSON.stringify({ ts: "2026-09-22T06:00:00Z", stream: "stdout", step: "build", line: "last" }) + "\n",
      truncated: true,
      total_bytes: 87_325_871,
      returned_bytes: 70,
    });
    getStepLogsFull.mockResolvedValue(
      JSON.stringify({ ts: "2026-09-22T05:00:00Z", stream: "stdout", step: "build", line: "first" }) + "\n",
    );
    renderDetail(makeStep());
    const banner = await screen.findByTestId("log-tail-banner");
    expect(banner).toHaveTextContent("83.3 MiB");
    fireEvent.click(screen.getByRole("button", { name: "Load full log" }));
    await waitFor(() => expect(getStepLogsFull).toHaveBeenCalledWith("job-1", "build", expect.any(Function)));
    await waitFor(() => expect(screen.queryByTestId("log-tail-banner")).not.toBeInTheDocument());
  });
});
```

- [ ] **Step 7: Run the UI suite**

Run: `cd ui && bun run test && bunx tsc -b && bun run lint`
Expected: PASS — including the existing `step-detail` and `job-detail` tests.

- [ ] **Step 8: Commit**

```bash
git add ui/src
git commit -m "feat(ui): step and server-event panels show a tail with load-full and download"
```

---

### Task 16: End-to-end coverage

**Files:**
- Create: `workspace/.workflows/big-log.yaml`
- Modify: `ui/e2e/log-streaming.spec.ts`
- Modify: `tests/e2e.sh` (after the log checks, about `:175`)

- [ ] **Step 1: Add a task with a log larger than the default tail**

`workspace/.workflows/big-log.yaml`:

```yaml
actions:
  print-many-lines:
    type: script
    script: |
      i=1
      while [ $i -le 20000 ]; do
        echo "big log line $i padded so that the whole log is well over the default tail"
        i=$((i + 1))
      done

tasks:
  big-log:
    mode: distributed
    flow:
      print:
        action: print-many-lines
```

- [ ] **Step 2: Add the Playwright test**

Append inside `test.describe("Log Streaming", …)` in `ui/e2e/log-streaming.spec.ts`:

```ts
  test("a large log shows a tail banner and loads the full log on demand", async ({ page, baseURL }) => {
    test.setTimeout(120_000);
    await login(page);
    const jobId = await triggerJob(baseURL!, "big-log", {});
    await page.goto(`/jobs/${jobId}`);
    // Step details mount only when the step is expanded.
    await page.getByText("print", { exact: true }).first().click();

    const banner = page.getByTestId("log-tail-banner");
    await expect(banner).toBeVisible({ timeout: 90_000 });
    await banner.getByRole("button", { name: "Load full log" }).click();
    await expect(banner).toBeHidden({ timeout: 60_000 });

    const log = page.getByRole("log");
    await log.evaluate((el) => {
      el.scrollTop = 0;
    });
    await expect(log).toContainText("big log line 1 padded", { timeout: 10_000 });
  });
```

If the step row renders its name differently, adjust only the click selector (see `ui/src/components/step-timeline.tsx`).

- [ ] **Step 3: Extend `tests/e2e.sh`**

After the greet/shout log checks in section 6, add:

```bash
# Tail envelope and full stream (log-tail-streaming spec § 3.1)
if [ "$(echo "$LOGS_RESP" | jq -r '.truncated')" != "false" ]; then
    echo "$LOGS_RESP" | jq .
    fail "A small log must not be reported as truncated"
fi
pass "Log tail envelope reports truncated=false for a small log"

FULL_LOGS=$(acurl "$BASE_URL/api/jobs/$JOB_ID/logs?full=true")
if echo "$FULL_LOGS" | grep -q "Hello E2E Test"; then
    pass "Full log stream (?full=true) contains the greet output"
else
    echo "$FULL_LOGS"
    fail "Full log stream (?full=true) is missing the greet output"
fi
```

- [ ] **Step 4: Run them**

Run: `./tests/e2e.sh` and `docker compose -f docker-compose.yml -f docker-compose.test.yml up --build --abort-on-container-exit playwright`
Expected: both PASS (Docker required).

- [ ] **Step 5: Commit**

```bash
git add workspace/.workflows/big-log.yaml ui/e2e/log-streaming.spec.ts tests/e2e.sh
git commit -m "test(logs): e2e coverage for the tail banner, full load and ?full=true"
```

---

### Task 17: Documentation and the full CI suite

**Files:**
- Modify: `docs/src/content/docs/reference/api.md` ("Get Job Logs", "Get Step Logs", "Stream Job Logs")
- Modify: `docs/src/content/docs/operations/log-storage.md` (Fields, Read fallback, Server events recipe, WebSocket streaming, new "Reading logs" section)
- Modify: `docs/src/content/docs/guides/mcp.md:70` and the example near `:303`
- Modify: `CLAUDE.md` (§ Log Storage `:471-476`, § WebSocket Log Streaming `:478-479`, `:499`, `:513`)
- Modify: `CONTEXT.md`, `docs/internal/TODO.md`, `docs/public/llms.txt` (regenerated)

- [ ] **Step 1: API reference**

Replace the three sections in `reference/api.md` with:

````markdown
## Get Job Logs

```
GET /api/jobs/{id}/logs
GET /api/jobs/{id}/logs?tail_bytes=1048576
GET /api/jobs/{id}/logs?full=true
```

Returns the **end** of the job's log by default: the last 256 KiB, cut to whole JSONL lines. `tail_bytes` asks for a different tail (1 byte to `log_storage.read.tail_max_bytes`, 4 MiB by default). `full=true` streams the whole log instead. The two parameters are mutually exclusive; an invalid value is a 400.

**Tail response** (`application/json`):

```json
{
  "logs": "{\"ts\":\"...\",\"stream\":\"stdout\",\"step\":\"say-hello\",\"line\":\"Hello World\"}\n",
  "truncated": false,
  "total_bytes": 81,
  "returned_bytes": 81
}
```

- `truncated` — `true` when earlier lines may exist; `false` is exact.
- `total_bytes` — an upper bound on the size of the whole job log.
- `returned_bytes` — the length of `logs`.

**Full response** (`application/x-ndjson; charset=utf-8`): the raw JSONL stream, no envelope.

Every response carries `X-Stroem-Log-Source: local | archive | merged | none`, naming the source that answered (see [Log storage](/operations/log-storage/#reading-logs)).

## Get Step Logs

```
GET /api/jobs/{id}/steps/{step}/logs
```

Same parameters, envelope and header as **Get Job Logs**, filtered to one step. Use `_server` as the step name for server-side events.

## Stream Job Logs (WebSocket)

```
GET /api/jobs/{id}/logs/stream
```

Opens a WebSocket for live log streaming. On connect the server sends the last 256 KiB of the log (whole lines) as one frame, then streams new chunks. `?skip_backfill=true` skips that first frame.

```bash
websocat ws://localhost:8080/api/jobs/JOB_ID/logs/stream
```
````

- [ ] **Step 2: Operations page**

In `operations/log-storage.md`: add the `read` block to the Fields table/section with the six keys, defaults and meanings from spec § 3.5 (keys under `log_storage.read`, env `STROEM__LOG_STORAGE__READ__<KEY>`); keep the `curl …/steps/_server/logs | jq -r .logs` recipe and add below it:

````markdown
To fetch a whole log, ask for the stream — it has no JSON envelope, so do not pipe it through `jq .logs`:

```bash
curl -s -H "Authorization: Bearer $TOKEN" "$STROEM/api/jobs/$JOB/logs?full=true" > job.jsonl
```
````

and add a new section:

```markdown
## Reading logs

Every log read is bounded. A read returns either a **tail** — the newest whole lines, 256 KiB by default — or the **full** log as a stream. The UI, `stroem-api logs`, the MCP `get_job_logs` tool and the WebSocket backfill all start from the tail.

| Job | Tail | Full |
|---|---|---|
| running | local file | local file |
| finished, archive configured | union of the local and archived tails (`merged`) | union in memory while local + archive ≤ `merge_max_bytes` and ≤ `merge_max_lines`; otherwise the local file, or the archive when there is no local file |

Neither source is guaranteed complete on its own (mirroring gaps, lines written after the archive upload), which is why finished jobs merge them when the caps allow. `X-Stroem-Log-Source` says which source answered. A full stream that fails part-way ends early; there is no fallback once the response has started.

**Memory.** The limits above bound what one read can allocate. The largest read a client can trigger (a 4 MiB tail of a finished job with very short lines) is about 69 MiB; a normal UI poll is under 6 MiB. On a server with a 512 Mi memory limit set `log_storage.read.tail_max_bytes: 1048576`.

**Behaviour change.** Scripts reading `logs` from `/api/jobs/{id}/logs` now receive the tail; check `truncated`, or use `?full=true`. Older `stroem-api` binaries print the tail without a note. WebSocket clients receive a tail as the first frame.
```

Also update the "Read fallback" subsection to: "`.jsonl` → legacy `.log` → (finished jobs only) the archive; a missing file is never an error." and the WebSocket section's backfill sentence to match the API reference.

- [ ] **Step 3: MCP guide**

Change the `get_job_logs` row to: `` | `get_job_logs` | The end of a job's log (last 256 KiB), formatted | `job_id`, `step?`, `tail_bytes?` | `` and add under the table: "When the log is longer than the tail, the result ends with `[truncated: showing the last 256.0 KiB of up to 83.3 MiB — earlier lines omitted]`."

- [ ] **Step 4: CLAUDE.md, CONTEXT.md, TODO.md**

In `CLAUDE.md` § Log Storage replace the "Read fallback" bullet with:

```markdown
- **Reads are bounded** (spec `docs/superpowers/specs/2026-09-22-log-tail-streaming-design.md`): `LogStorage::read_tail` (default 256 KiB, whole lines, `truncated`/`total_bytes`) and `LogStorage::stream_full` (NDJSON stream) are the ONLY readers; the String-returning `get_log`/`get_step_log` were removed on purpose — never add a reader that materialises a whole log. Primitives live in `crate::log_read` (`matcher`, `splitter`, `local`, `archive`). Finished jobs: tail = union of local + archive tails; full = in-memory union under `log_storage.read.merge_max_{bytes,lines}`, else local-first single source. `X-Stroem-Log-Source` names the source. Bounds are enforced by `tests/log_peak_alloc_test.rs` (counting allocator, `harness = false`) — a new read path needs a case there. Read fallback: `.jsonl` → legacy `.log` → (terminal) archive; `NotFound` is never a 500.
```

In § WebSocket Log Streaming change the bullet to "backfill = the default tail (`read_tail`), then live via `tokio::sync::broadcast`". Correct `NOTIFY_MAX_BYTES` (7000) to 3500 at `:499`, and at `:513` replace "`ui/src/hooks/use-job-logs.ts` — WebSocket logs." with "`ui/src/hooks/use-step-log.ts` — step log polling, full-log load and stitching (`lib/log-tail.ts::appendTail`)."

In `CONTEXT.md` add, before the closing "See `CLAUDE.md`" line:

```markdown
- **Tail read** — the newest whole lines of a log, at most `tail_bytes` (256 KiB by default), with `truncated` (exact when `false`) and `total_bytes` (an upper bound). What every log reader gets by default.
- **Full read** — the whole log as an NDJSON stream (`?full=true`); for a finished job the in-memory union of local and archive while it fits `merge_max_bytes`/`merge_max_lines`, else one source.
- **Log source** — which source answered a read: `local`, `archive`, `merged` or `none` (`X-Stroem-Log-Source`).
```

In `docs/internal/TODO.md` mark the Performance entry "Opening the job page of a job with a large log OOM-kills every server replica" `[x]` with "Fixed: bounded tail/full reads (spec 2026-09-22-log-tail-streaming)"; and add open items under Performance or Bugs: (1) the WebSocket backfill-to-live gap (`ws.rs` sends the backfill before subscribing); (2) lines appended after the archive upload never reach the archive; (3) the recovery sweep has no startup grace period (an outage longer than `heartbeat_timeout_secs` + one sweep fails running steps of healthy workers); (4) the worker drops a log batch whose push fails (`stroem-worker/src/poller.rs`).

- [ ] **Step 5: Regenerate llms.txt**

Run: `cd docs && bun install && bun run generate-llms && git diff --stat public/llms.txt`
Expected: `llms.txt` updated if the edited pages are in its sections; unchanged otherwise.

- [ ] **Step 6: Run the full CI suite**

Run:

```bash
cargo fmt --check --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd ui && bun run lint && bunx tsc -b && bun run test && bun run build
```

Expected: all green. `log_storage::tests` has a known parallel-run flake (see memory "Known flaky tests"); rerun that module alone before investigating.

- [ ] **Step 7: Commit**

```bash
git add CLAUDE.md CONTEXT.md docs
git commit -m "docs(logs): bounded log reads — API, operations, MCP, CLAUDE.md, glossary"
```

---

### Task 18: Codex implementation review

The user's rule: implementation, like the spec, is reviewed by Codex before it is done.

- [ ] **Step 1: Request the review on the spec's thread**

Invoke the `codex:rescue` skill with `--resume` (thread `01a0c805-be6b-7053-8be2-52c6bb019745`, which holds the five spec rounds) and this request: "Review the implementation of spec revision 7 on branch feat/log-tail-streaming (`git diff main...HEAD`). Check each spec section against the code, the peak-allocation test's coverage and formulas against § 3.4, the removal of every unbounded reader, and the UI's keep-last-non-empty and appendTail behaviour. Report findings with severity and file:line; do not edit files; conclude with 'no blockers' or the list of blockers."

- [ ] **Step 2: Present, verify, fix, re-review**

Present Codex's output verbatim, verify each finding against the code before changing anything, fix confirmed findings in separate commits (TDD: a failing test first), and resume the same thread for a re-review until it concludes "no blockers". If findings drift into another subsystem, stop and ask the user whether to cut scope.

- [ ] **Step 3: Hand over**

Use the `superpowers:finishing-a-development-branch` skill. Merging, pushing and releasing are the user's decisions.
