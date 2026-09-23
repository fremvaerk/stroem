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
fn envelope(t: usize) -> usize {
    6 * t + 256
}
fn tail_local(t: usize) -> usize {
    2 * t + envelope(t)
}
fn tail_step(t: usize, l: usize) -> usize {
    3 * t + 3 * l + envelope(t)
}
fn tail_merged(t: usize, l: usize, n: usize) -> usize {
    11 * t + 3 * l + R + H + D + K + (2 * t + 96 * n) + 256
}
fn full_local() -> usize {
    3 * K
}
fn full_local_filtered(l: usize) -> usize {
    5 * l + 4 * K
}
fn full_archive(l: usize, filtered: bool) -> usize {
    // Priming (C6) adds one more `K`-sized `BufReader` in front of the
    // decoder for the UNFILTERED branch (the filtered branch already reads
    // through a `BufReader` it reuses for priming, so its own term is
    // unchanged): `R + H + D + 3K` -> `R + H + D + 4K`.
    R + H + D + 3 * K + if filtered { 5 * l } else { K }
}
fn full_merged(c: usize, n: usize) -> usize {
    3 * c + 96 * n + R + H + D + K
}

/// `N'` for a merged read: the complete lines a tail read of `bytes` would
/// actually return — those fully inside its last `t` bytes (a leading
/// partial line, cut by the window boundary, is dropped, mirroring
/// `tail_unfiltered` / `byte_ring_tail`). Pass `t >= bytes.len()` to count
/// every line, for the full-merged cases where there is no window.
fn lines_in_last(bytes: &[u8], t: usize) -> usize {
    let start = bytes.len().saturating_sub(t);
    let newlines = bytes[start..].iter().filter(|&&b| b == b'\n').count();
    if start == 0 || bytes[start - 1] == b'\n' {
        newlines
    } else {
        newlines.saturating_sub(1)
    }
}

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
        .map(|i| {
            format!(r#"{{"ts":"2026-09-22T00:00:00Z","step":"s","line":"{tag}{i:08}"}}"#) + "\n"
        })
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
    Env {
        live: live.path().to_path_buf(),
        _live: live,
        _arch: arch,
        archive,
        storage,
    }
}

impl Env {
    async fn local(&self, job: Uuid, ext: &str, content: &[u8]) {
        tokio::fs::write(self.live.join(format!("{job}.{ext}")), content)
            .await
            .unwrap();
    }

    async fn archived(&self, job: Uuid, content: &[u8]) {
        self.archive
            .put(
                &archive_key("", job, &meta()),
                "application/gzip",
                gzip(content).into(),
            )
            .await
            .unwrap();
    }

    async fn tail(&self, job: Uuid, terminal: bool, filter: StepFilter<'_>, t: usize) -> LogSource {
        let tail = self
            .storage
            .read_tail(job, &meta(), terminal, filter, t as u64)
            .await
            .unwrap();
        drop(tail_body(&tail).unwrap());
        tail.source
    }

    async fn drain(&self, job: Uuid, terminal: bool, filter: StepFilter<'_>) -> LogSource {
        let (source, mut stream) = self
            .storage
            .stream_full(job, &meta(), terminal, filter)
            .await
            .unwrap();
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
    let c = cfg.merge_max_bytes as usize;
    let mut report = Report::default();

    // (a) Incident-shaped log, local and archived.
    let e = env_with(cfg);
    let job = Uuid::new_v4();
    // 9 MiB, not 8: local + archive must exceed merge_max_bytes (16 MiB)
    // so the "over the cap" case below is really over it.
    let log = incident_log(if large { 100 * MIB } else { 9 * MIB }, "Uploading");
    e.local(job, "jsonl", log.as_bytes()).await;
    e.archived(job, log.as_bytes()).await;
    // Local and archive are byte-identical here, so both tails contribute
    // the same count: the lines each one actually returns from its last `t`
    // bytes (spec § 5: ~2 800 for the default 256 KiB tail), not
    // `merge_max_lines`.
    let incident_tail_n = 2 * lines_in_last(log.as_bytes(), t);
    // Warm up the blocking pool and lazy statics before any measurement.
    e.tail(job, false, StepFilter::All, 1024).await;

    let (s, p) = peak_of(e.tail(job, false, StepFilter::All, t)).await;
    assert_eq!(s, LogSource::Local);
    report.check("tail, unfiltered, local", p, tail_local(t));
    let (_, p) = peak_of(e.tail(job, false, StepFilter::Step("sync"), t)).await;
    report.check("tail, step (chatty), local", p, tail_step(t, l));
    let (_, p) = peak_of(e.tail(job, false, StepFilter::Step("quiet"), t)).await;
    report.check(
        "tail, step (quiet: scans back to the start or the cap), local",
        p,
        tail_step(t, l),
    );
    let (_, p) = peak_of(e.drain(job, false, StepFilter::All)).await;
    report.check("full, unfiltered, local", p, full_local());
    let (_, p) = peak_of(e.drain(job, false, StepFilter::Step("sync"))).await;
    report.check("full, filtered, local", p, full_local_filtered(l));
    let (s, p) = peak_of(e.tail(job, true, StepFilter::All, t)).await;
    assert_eq!(s, LogSource::Merged);
    report.check(
        "tail, terminal, merged",
        p,
        tail_merged(t, l, incident_tail_n),
    );
    let (_, p) = peak_of(e.tail(job, true, StepFilter::Step("sync"), t)).await;
    report.check(
        "tail, terminal step, merged",
        p,
        tail_merged(t, l, incident_tail_n),
    );
    let (s, p) = peak_of(e.drain(job, true, StepFilter::All)).await;
    assert_eq!(
        s,
        LogSource::Local,
        "the incident log is over merge_max_bytes"
    );
    report.check("full, terminal over the cap, local", p, full_local());
    tokio::fs::remove_file(e.live.join(format!("{job}.jsonl")))
        .await
        .unwrap();
    let (s, p) = peak_of(e.drain(job, true, StepFilter::All)).await;
    assert_eq!(s, LogSource::Archive);
    report.check("full, archive, single source", p, full_archive(l, false));
    let (_, p) = peak_of(e.drain(job, true, StepFilter::Step("sync"))).await;
    report.check(
        "full, archive filtered, single source",
        p,
        full_archive(l, true),
    );

    // Full merged under both caps: 10 MiB local + 4 MiB archive, disjoint.
    {
        let e = env_with(cfg);
        let job = Uuid::new_v4();
        let local_log = incident_log(10 * MIB, "Uploading");
        let archive_log = incident_log(4 * MIB, "Archived");
        // A full read has no window: N' is every line of both inputs.
        let full_n = lines_in_last(local_log.as_bytes(), local_log.len())
            + lines_in_last(archive_log.as_bytes(), archive_log.len());
        e.local(job, "jsonl", local_log.as_bytes()).await;
        e.archived(job, archive_log.as_bytes()).await;
        let (s, p) = peak_of(e.drain(job, true, StepFilter::All)).await;
        assert_eq!(s, LogSource::Merged);
        report.check("full, terminal, merged", p, full_merged(c, full_n));
    }

    // (b) Distinct short lines filling a 4 MiB tail on each side, just
    // under the merge line cap.
    {
        let e = env_with(cfg);
        let job = Uuid::new_v4();
        let t4 = 4 * MIB;
        // 60-byte lines: 60 000 per side is 3.4 MiB (under the 4 MiB tail)
        // and 120 000 in the union (under merge_max_lines = 131 072); every
        // line fits inside the 4 MiB window, so N' is exactly 120 000.
        let local_log = short_lines(60_000, "L");
        let archive_log = short_lines(60_000, "A");
        let short_n =
            lines_in_last(local_log.as_bytes(), t4) + lines_in_last(archive_log.as_bytes(), t4);
        e.local(job, "jsonl", local_log.as_bytes()).await;
        e.archived(job, archive_log.as_bytes()).await;
        let (s, p) = peak_of(e.tail(job, true, StepFilter::All, t4)).await;
        assert_eq!(
            s,
            LogSource::Merged,
            "the two tails must fit merge_max_lines"
        );
        report.check(
            "tail 4 MiB, terminal, merged, short lines",
            p,
            tail_merged(t4, l, short_n),
        );
    }

    // (c) Worst-case envelope escapes: a legacy file of raw control bytes.
    {
        let e = env_with(cfg);
        let job = Uuid::new_v4();
        let line = format!("{}\n", "\u{1}".repeat(200));
        e.local(job, "log", line.repeat(MIB / line.len()).as_bytes())
            .await;
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
        // The escape is built at runtime: a literal backslash-u sequence in
        // source text does not survive every editing tool.
        let esc = format!("{}u0041", '\\');
        let hit = format!(r#"{{"step":"{body}{esc}","line":"{body}A"}}"#);
        let miss = r#"{"step":"other","line":"x"}"#;
        let content: String = (0..5).map(|_| format!("{hit}\n{miss}\n")).collect();
        e.local(job, "jsonl", content.as_bytes()).await;
        let (_, p) = peak_of(e.drain(job, false, StepFilter::Step(&step))).await;
        report.check(
            "full, filtered, escaped step values",
            p,
            full_local_filtered(l),
        );
    }

    // (d2) Matcher scratch, the 2 L case: a NON-matching line whose
    // `contains` guard passes because the target ("A") appears in `line`,
    // not because the decoded step name repeats there — so almost the
    // whole line can be one escaped string, and the scratch buffer that
    // holds its decoded content grows close to `2 L`. A MATCHING line
    // can't reach this: matching (d)'s design needs the decoded name to
    // also appear literally elsewhere in the line, which halves the room
    // available to the escaped value.
    {
        let e = env_with(cfg);
        let job = Uuid::new_v4();
        let shell = r#"{"step":"","line":"A"}"#;
        // The escape is built at runtime: a literal backslash-u sequence in
        // source text does not survive every editing tool. Decodes to 'A'.
        let esc = format!("{}u0041", '\\');
        let body = "s".repeat(l - 64 - shell.len());
        let line = format!(r#"{{"step":"{body}{esc}","line":"A"}}"#);
        assert!(line.len() <= l, "fixture line must fit the line cap");
        e.local(job, "jsonl", format!("{line}\n").as_bytes()).await;
        let (_, p) = peak_of(e.drain(job, false, StepFilter::Step("A"))).await;
        report.check(
            "full, filtered, 2L matcher scratch (non-matching)",
            p,
            full_local_filtered(l),
        );
    }

    // MCP tail formatting: `format_logs` must not materialise a large
    // unused field (C1). A local file with one ~4 MiB record holding an
    // unused array sits inside the requested 4 MiB tail; `format_logs`
    // must render it without allocating anywhere near the array's size.
    #[cfg(feature = "mcp")]
    {
        use stroem_server::mcp::tools::format_logs;

        let e = env_with(cfg);
        let job = Uuid::new_v4();
        let t4 = cfg.tail_max_bytes as usize;
        let small1 = r#"{"ts":"2026-09-22T00:00:00Z","step":"s","line":"first"}"#;
        let small2 = r#"{"ts":"2026-09-22T00:00:00Z","step":"s","line":"last"}"#;
        let shell = r#"{"ts":"2026-09-22T00:00:00Z","step":"s","line":"x","blob":[]}"#;
        let slack = 8 * KIB;
        let overhead = small1.len() + 1 + small2.len() + 1 + shell.len() + 1 + slack;
        let array_budget = t4 - overhead;
        let mut blob = "0,".repeat(array_budget / 2 + 1);
        blob.truncate(array_budget);
        if blob.ends_with(',') {
            blob.pop();
        }
        let big =
            format!(r#"{{"ts":"2026-09-22T00:00:00Z","step":"s","line":"x","blob":[{blob}]}}"#);
        let content = format!("{small1}\n{big}\n{small2}\n");
        assert!(
            content.len() < t4,
            "fixture must fit the tail window untruncated"
        );
        e.local(job, "jsonl", content.as_bytes()).await;
        let (_, p) = peak_of(async {
            let tail = e
                .storage
                .read_tail(job, &meta(), false, StepFilter::All, t4 as u64)
                .await
                .unwrap();
            drop(format_logs(&tail.logs));
        })
        .await;
        report.check(
            "MCP tail, one 4 MiB array record, formatted",
            p,
            tail_local(t4) + 2 * t4,
        );
    }

    // (e) One 15 MiB line under merge_max_lines = 2: the merger's
    // byte-based reservation, not its line count, dominates.
    {
        let cfg2 = LogReadConfig {
            merge_max_lines: 2,
            ..cfg
        };
        let e = env_with(cfg2);
        let job = Uuid::new_v4();
        let huge = format!(
            r#"{{"ts":"2026-09-22T00:00:00Z","step":"s","line":"{}"}}"#,
            "x".repeat(15 * MIB)
        ) + "\n";
        e.local(job, "jsonl", huge.as_bytes()).await;
        e.archived(job, short_lines(1, "A").as_bytes()).await;
        let (s, p) = peak_of(e.drain(job, true, StepFilter::All)).await;
        assert_eq!(s, LogSource::Merged);
        report.check(
            "full, terminal, merged, one 15 MiB line",
            p,
            full_merged(c, 2),
        );
    }

    #[cfg(feature = "s3")]
    s3_cases(&mut report, cfg).await;

    assert!(
        report.over.is_empty(),
        "reads over their memory bound: {:#?}",
        report.over
    );
}

#[cfg(feature = "s3")]
async fn s3_cases(report: &mut Report, cfg: LogReadConfig) {
    use stroem_server::blob_storage::S3BlobArchive;
    use testcontainers::runners::AsyncRunner;
    use testcontainers::ImageExt;
    use testcontainers_modules::minio::MinIO;

    let container = MinIO::default()
        .with_name("quay.io/minio/minio")
        .start()
        .await
        .unwrap();
    let port = container.get_host_port_ipv4(9000).await.unwrap();
    let creds =
        aws_sdk_s3::config::Credentials::new("minioadmin", "minioadmin", None, None, "test");
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
    let log = incident_log(8 * MIB, "Uploading");
    tokio::fs::write(live.path().join(format!("{job}.jsonl")), &log)
        .await
        .unwrap();
    archive
        .put(&key, "application/gzip", gzip(log.as_bytes()).into())
        .await
        .unwrap();
    // Warm the SDK's connection pool, credentials and endpoint caches.
    let obj = archive.open(&key).await.unwrap().unwrap();
    archive
        .read_range(&obj, 0, 16, &mut Vec::new())
        .await
        .unwrap();
    drop(obj);

    let (t, l) = (cfg.tail_default_bytes as usize, cfg.max_line_bytes);
    // Local and archive are byte-identical here too.
    let s3_tail_n = 2 * lines_in_last(log.as_bytes(), t);
    let (_, p) = peak_of(async {
        let tail = storage
            .read_tail(job, &meta(), true, StepFilter::All, t as u64)
            .await
            .unwrap();
        assert_eq!(tail.source, LogSource::Merged);
        drop(tail_body(&tail).unwrap());
    })
    .await;
    report.check(
        "S3: tail, terminal, merged",
        p,
        tail_merged(t, l, s3_tail_n),
    );

    tokio::fs::remove_file(live.path().join(format!("{job}.jsonl")))
        .await
        .unwrap();
    let (_, p) = peak_of(async {
        let (source, mut stream) = storage
            .stream_full(job, &meta(), true, StepFilter::All)
            .await
            .unwrap();
        assert_eq!(source, LogSource::Archive);
        while let Some(chunk) = stream.next().await {
            drop(chunk.unwrap());
        }
    })
    .await;
    report.check(
        "S3: full, archive, single source",
        p,
        full_archive(l, false),
    );
}
