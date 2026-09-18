//! Cooperative deadlines for workspace loading (spec § 4.5).
//!
//! A `LoadBudget` is checked between units of work (files, git callbacks) and
//! enforced on subprocesses (`sops`, `vals`) by killing them. It cannot
//! interrupt a single blocked syscall — that residual risk is stated in the spec.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::process::{Command, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A deadline threaded through a workspace load. `Copy`, so it can be moved
/// into git2 callbacks and Tera filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LoadBudget {
    deadline: Option<Instant>,
}

impl LoadBudget {
    pub fn unbounded() -> Self {
        Self { deadline: None }
    }
    pub fn until(deadline: Instant) -> Self {
        Self {
            deadline: Some(deadline),
        }
    }
    pub fn from_now(duration: Duration) -> Self {
        Self::until(Instant::now() + duration)
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
    pub fn expired(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }
    pub fn check(&self) -> std::result::Result<(), DeadlineExceeded> {
        if self.expired() {
            Err(DeadlineExceeded)
        } else {
            Ok(())
        }
    }
}

/// The load ran past its `LoadBudget`. A distinct type so callers can tell it
/// apart from ordinary per-file errors and abort the whole load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadlineExceeded;

impl std::fmt::Display for DeadlineExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "workspace load deadline exceeded")
    }
}

impl std::error::Error for DeadlineExceeded {}

/// True when `err` is, or wraps, [`DeadlineExceeded`].
pub fn is_deadline_exceeded(err: &anyhow::Error) -> bool {
    err.downcast_ref::<DeadlineExceeded>().is_some()
        || err.chain().any(|cause| cause.is::<DeadlineExceeded>())
}

const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Run `cmd` to completion, killing and reaping it if `budget` expires.
///
/// stdout/stderr are drained on their own threads so a chatty child cannot
/// deadlock on a full pipe. After a kill the reader threads are DETACHED, not
/// joined: a grandchild that inherited the pipes may keep them open.
pub fn run_with_deadline(
    mut cmd: Command,
    stdin: Option<&[u8]>,
    budget: &LoadBudget,
) -> Result<Output> {
    budget.check()?;
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("failed to spawn subprocess")?;

    let writer = match (stdin, child.stdin.take()) {
        (Some(bytes), Some(mut pipe)) => {
            let bytes = bytes.to_vec();
            Some(std::thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            }))
        }
        _ => None,
    };
    let stdout = spawn_reader(child.stdout.take());
    let stderr = spawn_reader(child.stderr.take());

    let status = if budget.deadline().is_none() {
        child.wait().context("failed to wait for subprocess")?
    } else {
        loop {
            if let Some(status) = child.try_wait().context("failed to poll subprocess")? {
                break status;
            }
            if budget.expired() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(DeadlineExceeded.into());
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    };

    if let Some(writer) = writer {
        let _ = writer.join();
    }
    Ok(Output {
        status,
        stdout: join_reader(stdout),
        stderr: join_reader(stderr),
    })
}

fn spawn_reader<R: Read + Send + 'static>(pipe: Option<R>) -> Option<JoinHandle<Vec<u8>>> {
    pipe.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    })
}

fn join_reader(handle: Option<JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle.and_then(|h| h.join().ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbounded_never_expires() {
        let b = LoadBudget::unbounded();
        assert!(!b.expired());
        assert!(b.check().is_ok());
        assert_eq!(b.deadline(), None);
        assert_eq!(LoadBudget::default(), b);
    }

    #[test]
    fn deadline_in_the_past_is_expired() {
        let b = LoadBudget::until(Instant::now());
        assert!(b.expired());
        assert_eq!(b.check(), Err(DeadlineExceeded));
    }

    #[test]
    fn deadline_detected_through_context_layers() {
        let err = anyhow::Error::new(DeadlineExceeded)
            .context("inner")
            .context("outer");
        assert!(is_deadline_exceeded(&err));
        assert!(!is_deadline_exceeded(&anyhow::anyhow!("something else")));
    }

    #[cfg(unix)]
    #[test]
    fn returns_output_of_a_normal_command() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2"]);
        let out =
            run_with_deadline(cmd, None, &LoadBudget::from_now(Duration::from_secs(10))).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
    }

    #[cfg(unix)]
    #[test]
    fn passes_stdin() {
        let out = run_with_deadline(
            Command::new("cat"),
            Some(b"payload"),
            &LoadBudget::unbounded(),
        )
        .unwrap();
        assert_eq!(out.stdout, b"payload");
    }

    #[cfg(unix)]
    #[test]
    fn large_output_does_not_deadlock() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "head -c 300000 /dev/zero"]);
        let out =
            run_with_deadline(cmd, None, &LoadBudget::from_now(Duration::from_secs(10))).unwrap();
        assert_eq!(out.stdout.len(), 300_000);
    }

    #[cfg(unix)]
    #[test]
    fn kills_the_child_at_the_deadline() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let started = Instant::now();
        let err = run_with_deadline(cmd, None, &LoadBudget::from_now(Duration::from_millis(200)))
            .unwrap_err();
        assert!(is_deadline_exceeded(&err), "got {err:#}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn expired_budget_does_not_spawn() {
        let cmd = Command::new("definitely-not-a-real-binary-7f3a");
        let err = run_with_deadline(cmd, None, &LoadBudget::until(Instant::now())).unwrap_err();
        assert!(
            is_deadline_exceeded(&err),
            "must fail on the deadline, not on spawn: {err:#}"
        );
    }
}
