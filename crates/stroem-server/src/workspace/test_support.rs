//! Scriptable `WorkspaceSource` for lifecycle tests.

use super::source::{LoadOutcome, Peek, WorkspaceSource};
use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use stroem_common::budget::LoadBudget;
use stroem_common::models::workflow::WorkspaceConfig;

#[derive(Debug, Clone, Copy)]
pub(crate) enum TestLoad {
    Ok {
        action: &'static str,
        revision: &'static str,
    },
    Err(&'static str),
    Panic,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TestPeek {
    Revision(&'static str),
    Failed,
    LocalInvalid,
    Hang(Duration),
}

pub(crate) struct TestSource {
    pub loads: AtomicUsize,
    pub started: AtomicBool,
    load: Mutex<TestLoad>,
    load_sleep: Mutex<Duration>,
    peek: Mutex<TestPeek>,
    /// When set, `load` spins until the flag is true.
    pub gate: Option<Arc<AtomicBool>>,
    pub poll_secs: u64,
}

pub(crate) fn config_with(action: &str) -> WorkspaceConfig {
    serde_yaml::from_str(&format!(
        "actions:\n  {action}:\n    type: script\n    script: echo hi\n"
    ))
    .unwrap()
}

impl TestSource {
    pub fn new(load: TestLoad, peek: TestPeek) -> Self {
        Self {
            loads: AtomicUsize::new(0),
            started: AtomicBool::new(false),
            load: Mutex::new(load),
            load_sleep: Mutex::new(Duration::ZERO),
            peek: Mutex::new(peek),
            gate: None,
            poll_secs: 60,
        }
    }
    pub fn gated(mut self, gate: Arc<AtomicBool>) -> Self {
        self.gate = Some(gate);
        self
    }
    pub fn set_load(&self, load: TestLoad) {
        *self.load.lock().unwrap_or_else(|e| e.into_inner()) = load;
    }
    pub fn set_load_sleep(&self, d: Duration) {
        *self.load_sleep.lock().unwrap_or_else(|e| e.into_inner()) = d;
    }
    pub fn set_peek(&self, peek: TestPeek) {
        *self.peek.lock().unwrap_or_else(|e| e.into_inner()) = peek;
    }
    pub fn load_count(&self) -> usize {
        self.loads.load(Ordering::SeqCst)
    }
}

impl WorkspaceSource for TestSource {
    fn load(&self, _budget: &LoadBudget) -> Result<LoadOutcome> {
        self.started.store(true, Ordering::SeqCst);
        self.loads.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            while !gate.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let sleep = *self.load_sleep.lock().unwrap_or_else(|e| e.into_inner());
        if !sleep.is_zero() {
            std::thread::sleep(sleep);
        }
        let load = *self.load.lock().unwrap_or_else(|e| e.into_inner());
        match load {
            TestLoad::Ok { action, revision } => Ok(LoadOutcome {
                config: config_with(action),
                warnings: Vec::new(),
                revision: Some(revision.to_string()),
            }),
            TestLoad::Err(msg) => Err(anyhow!("{msg}")),
            TestLoad::Panic => panic!("test source panicked"),
        }
    }

    fn path(&self) -> &Path {
        Path::new("/dev/null")
    }

    fn peek_revision(&self, _budget: &LoadBudget) -> Peek {
        let peek = *self.peek.lock().unwrap_or_else(|e| e.into_inner());
        match peek {
            TestPeek::Revision(r) => Peek::Revision(r.to_string()),
            TestPeek::Failed => Peek::Failed(anyhow!("remote unreachable")),
            TestPeek::LocalInvalid => Peek::LocalInvalid(anyhow!("no clone")),
            TestPeek::Hang(d) => {
                std::thread::sleep(d);
                Peek::Failed(anyhow!("hung"))
            }
        }
    }

    fn poll_interval_secs(&self) -> u64 {
        self.poll_secs
    }
}
