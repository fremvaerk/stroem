//! Watcher availability state machine (spec § 4.4).
//!
//! Pure: no I/O, no locks, no clock reads — every `Instant` arrives inside an
//! `Event`. `transition` is the ONLY writer of an `Availability`; the entry
//! wraps it in a microsecond `std::sync::Mutex`.

use std::time::{Duration, Instant};

/// Who initiated a load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// The per-workspace watcher — the only caller with a retry policy.
    Watcher,
    /// API refresh, peer reload notification, scheduler/webhook `force_refresh`.
    External,
    /// `WorkspaceManager::new_with_reload`'s initial load of each workspace.
    Startup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InFlight {
    pub op_id: u64,
    pub started_at: Instant,
    pub deadline: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Serving a successfully loaded config. The counter saturates at K.
    Fresh { consecutive_peek_failures: u32 },
    /// Last load failed; the watcher retries at `next_attempt`.
    Errored {
        backoff: Duration,
        next_attempt: Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Availability {
    pub freshness: Freshness,
    pub load_in_flight: Option<InFlight>,
    pub peek_in_flight: Option<InFlight>,
}

/// Per-workspace policy: K, the poll interval, and the backoff cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub peek_failure_threshold: u32,
    pub poll_interval: Duration,
    pub max_backoff: Duration,
}

/// Server-wide reload tuning (config `workspace_reload:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReloadSettings {
    /// K — consecutive failed peeks before a forced load.
    pub peek_failure_threshold: u32,
    /// P — budget for one peek.
    pub peek_timeout: Duration,
    /// L — budget for one load.
    pub load_timeout: Duration,
    /// Cap of the watcher's retry backoff while `Errored`.
    pub max_backoff: Duration,
}

impl Default for ReloadSettings {
    fn default() -> Self {
        Self {
            peek_failure_threshold: 5,
            peek_timeout: Duration::from_secs(30),
            load_timeout: Duration::from_secs(300),
            max_backoff: Duration::from_secs(900),
        }
    }
}

impl ReloadSettings {
    pub fn policy(&self, poll_interval: Duration) -> Policy {
        Policy {
            peek_failure_threshold: self.peek_failure_threshold.max(1),
            poll_interval,
            max_backoff: self.max_backoff,
        }
    }
}

impl From<&crate::config::WorkspaceReloadConfig> for ReloadSettings {
    fn from(c: &crate::config::WorkspaceReloadConfig) -> Self {
        Self {
            peek_failure_threshold: c.peek_failure_threshold,
            peek_timeout: Duration::from_secs(c.peek_timeout_secs),
            load_timeout: Duration::from_secs(c.load_timeout_secs),
            max_backoff: Duration::from_secs(c.max_backoff_secs),
        }
    }
}

/// A peek outcome reduced to what the transition needs (spec § 4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeekObservation {
    /// `Peek::Revision(r)` with `r` == the published revision.
    Matches,
    /// `Peek::Revision(r)` with `r` != the published revision.
    Differs,
    /// `Peek::Unsupported` or `Peek::LocalInvalid` — only a load can decide.
    NeedsLoad,
    /// `Peek::Failed` — "I don't know".
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Tick {
        now: Instant,
    },
    PeekStarted {
        op_id: u64,
        started_at: Instant,
        deadline: Instant,
    },
    /// Observer: the peek answered within P.
    PeekCompleted {
        observation: PeekObservation,
    },
    /// Observer: P expired.
    PeekTimedOut,
    /// Finalizer: the peek worker really returned (or panicked).
    PeekFinished {
        op_id: u64,
    },
    LoadStarted {
        op_id: u64,
        started_at: Instant,
        deadline: Instant,
    },
    /// Finalizer: the load really returned. `op_id` is `None` for loads that
    /// were never recorded as in flight (external, startup).
    LoadCompleted {
        op_id: Option<u64>,
        caller: Caller,
        ok: bool,
        completed_at: Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    Skip,
    Peek,
    AttemptLoad,
    /// A failed peek below K: skip, keep serving. `first` ⇒ log once.
    PeekFailureSkipped {
        first: bool,
    },
}

impl Availability {
    pub fn fresh() -> Self {
        Self {
            freshness: Freshness::Fresh {
                consecutive_peek_failures: 0,
            },
            load_in_flight: None,
            peek_in_flight: None,
        }
    }

    pub fn is_errored(&self) -> bool {
        matches!(self.freshness, Freshness::Errored { .. })
    }

    /// Derived, never stored (spec § 4.5 (4)).
    pub fn load_overdue(&self, now: Instant) -> bool {
        self.load_in_flight.is_some_and(|f| now > f.deadline)
    }
}

/// Stand-in for a delay too large to add to an `Instant`: 24 h, the same cap
/// config validation puts on `workspace_reload` timeouts and backoff.
pub(crate) const OVERFLOW_FALLBACK: Duration = Duration::from_secs(86_400);

/// `t + d`, saturating to `t + OVERFLOW_FALLBACK` when `d` is unrepresentable
/// (an absurd git `poll_interval_secs`, which config does not cap).
pub(crate) fn instant_after(t: Instant, d: Duration) -> Instant {
    t.checked_add(d)
        .or_else(|| t.checked_add(OVERFLOW_FALLBACK))
        .unwrap_or(t)
}

/// The single writer of an [`Availability`].
pub fn transition(a: &mut Availability, event: Event, policy: &Policy) -> Effect {
    match event {
        Event::Tick { now } => match a.freshness {
            Freshness::Errored { next_attempt, .. } => {
                if now < next_attempt {
                    Effect::Skip
                } else {
                    Effect::AttemptLoad
                }
            }
            Freshness::Fresh { .. } => {
                if a.peek_in_flight.is_some() {
                    peek_failed(a, policy)
                } else {
                    Effect::Peek
                }
            }
        },
        Event::PeekStarted {
            op_id,
            started_at,
            deadline,
        } => {
            a.peek_in_flight = Some(InFlight {
                op_id,
                started_at,
                deadline,
            });
            Effect::None
        }
        Event::PeekCompleted { observation } => {
            if a.is_errored() {
                return Effect::None;
            }
            match observation {
                PeekObservation::Matches => {
                    a.freshness = Freshness::Fresh {
                        consecutive_peek_failures: 0,
                    };
                    Effect::None
                }
                PeekObservation::Differs | PeekObservation::NeedsLoad => Effect::AttemptLoad,
                PeekObservation::Failed => peek_failed(a, policy),
            }
        }
        Event::PeekTimedOut => {
            if a.is_errored() {
                Effect::None
            } else {
                peek_failed(a, policy)
            }
        }
        Event::PeekFinished { op_id } => {
            if a.peek_in_flight.is_some_and(|f| f.op_id == op_id) {
                a.peek_in_flight = None;
            }
            Effect::None
        }
        Event::LoadStarted {
            op_id,
            started_at,
            deadline,
        } => {
            a.load_in_flight = Some(InFlight {
                op_id,
                started_at,
                deadline,
            });
            if !a.is_errored() {
                a.freshness = Freshness::Fresh {
                    consecutive_peek_failures: 0,
                };
            }
            Effect::None
        }
        Event::LoadCompleted {
            op_id,
            caller,
            ok,
            completed_at,
        } => {
            if let Some(id) = op_id {
                if a.load_in_flight.is_some_and(|f| f.op_id == id) {
                    a.load_in_flight = None;
                }
            }
            a.freshness = match (ok, a.freshness, caller) {
                (true, _, _) => Freshness::Fresh {
                    consecutive_peek_failures: 0,
                },
                (false, _, Caller::Startup) => Freshness::Errored {
                    backoff: policy.poll_interval,
                    next_attempt: completed_at,
                },
                (false, Freshness::Fresh { .. }, _) => Freshness::Errored {
                    backoff: policy.poll_interval,
                    next_attempt: instant_after(completed_at, policy.poll_interval),
                },
                (false, Freshness::Errored { backoff, .. }, Caller::Watcher) => {
                    let next = backoff.saturating_mul(2).min(policy.max_backoff);
                    Freshness::Errored {
                        backoff: next,
                        next_attempt: instant_after(completed_at, next),
                    }
                }
                (false, errored @ Freshness::Errored { .. }, Caller::External) => errored,
            };
            Effect::None
        }
    }
}

/// A failed or timed-out peek: count it; at K, force one load.
fn peek_failed(a: &mut Availability, policy: &Policy) -> Effect {
    let Freshness::Fresh {
        consecutive_peek_failures: n,
    } = a.freshness
    else {
        return Effect::None;
    };
    let k = policy.peek_failure_threshold.max(1);
    let next = n.saturating_add(1);
    if next < k {
        a.freshness = Freshness::Fresh {
            consecutive_peek_failures: next,
        };
        Effect::PeekFailureSkipped { first: n == 0 }
    } else {
        a.freshness = Freshness::Fresh {
            consecutive_peek_failures: k,
        };
        Effect::AttemptLoad
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLL: Duration = Duration::from_secs(60);

    fn policy() -> Policy {
        Policy {
            peek_failure_threshold: 3,
            poll_interval: POLL,
            max_backoff: Duration::from_secs(900),
        }
    }

    fn fresh(n: u32) -> Availability {
        Availability {
            freshness: Freshness::Fresh {
                consecutive_peek_failures: n,
            },
            ..Availability::fresh()
        }
    }

    fn errored(backoff: Duration, next_attempt: Instant) -> Availability {
        Availability {
            freshness: Freshness::Errored {
                backoff,
                next_attempt,
            },
            ..Availability::fresh()
        }
    }

    fn inflight(op_id: u64, t: Instant) -> InFlight {
        InFlight {
            op_id,
            started_at: t,
            deadline: t + Duration::from_secs(5),
        }
    }

    fn done(caller: Caller, ok: bool, at: Instant) -> Event {
        Event::LoadCompleted {
            op_id: None,
            caller,
            ok,
            completed_at: at,
        }
    }

    #[test]
    fn tick_while_errored_before_next_attempt_skips() {
        let now = Instant::now();
        let mut a = errored(POLL, now + POLL);
        assert_eq!(
            transition(&mut a, Event::Tick { now }, &policy()),
            Effect::Skip
        );
    }

    #[test]
    fn tick_while_errored_at_next_attempt_loads_without_peeking() {
        let now = Instant::now();
        let mut a = errored(POLL, now);
        assert_eq!(
            transition(&mut a, Event::Tick { now }, &policy()),
            Effect::AttemptLoad
        );
    }

    #[test]
    fn tick_while_fresh_peeks() {
        let mut a = fresh(0);
        assert_eq!(
            transition(
                &mut a,
                Event::Tick {
                    now: Instant::now()
                },
                &policy()
            ),
            Effect::Peek
        );
    }

    #[test]
    fn tick_with_a_hung_peek_counts_as_timeout_and_starts_no_peek() {
        let now = Instant::now();
        let mut a = fresh(0);
        a.peek_in_flight = Some(inflight(1, now));
        assert_eq!(
            transition(&mut a, Event::Tick { now }, &policy()),
            Effect::PeekFailureSkipped { first: true }
        );
        assert_eq!(
            a.freshness,
            Freshness::Fresh {
                consecutive_peek_failures: 1
            }
        );
    }

    #[test]
    fn matching_peek_resets_the_counter() {
        let mut a = fresh(2);
        let e = transition(
            &mut a,
            Event::PeekCompleted {
                observation: PeekObservation::Matches,
            },
            &policy(),
        );
        assert_eq!(e, Effect::None);
        assert_eq!(a, fresh(0));
    }

    #[test]
    fn differing_or_undecidable_peek_attempts_a_load_without_state_change() {
        for obs in [PeekObservation::Differs, PeekObservation::NeedsLoad] {
            let mut a = fresh(1);
            let e = transition(&mut a, Event::PeekCompleted { observation: obs }, &policy());
            assert_eq!(e, Effect::AttemptLoad);
            assert_eq!(a, fresh(1));
        }
    }

    #[test]
    fn failed_peek_below_k_skips_and_counts() {
        let mut a = fresh(0);
        let p = policy();
        let failed = Event::PeekCompleted {
            observation: PeekObservation::Failed,
        };
        assert_eq!(
            transition(&mut a, failed, &p),
            Effect::PeekFailureSkipped { first: true }
        );
        assert_eq!(
            transition(&mut a, failed, &p),
            Effect::PeekFailureSkipped { first: false }
        );
        assert_eq!(a, fresh(2));
    }

    #[test]
    fn kth_failure_saturates_and_attempts_a_forced_load() {
        let mut a = fresh(2);
        let e = transition(&mut a, Event::PeekTimedOut, &policy());
        assert_eq!(e, Effect::AttemptLoad);
        assert_eq!(a, fresh(3));
    }

    /// Round-4 finding 4.1: a rejected admission (no LoadStarted) must keep
    /// the forced probe pending instead of losing it.
    #[test]
    fn k_boundary_rejected_admission_keeps_the_probe_pending() {
        let p = policy();
        let failed = Event::PeekCompleted {
            observation: PeekObservation::Failed,
        };
        let mut a = fresh(2);
        assert_eq!(transition(&mut a, failed, &p), Effect::AttemptLoad);
        // admission Busy/Saturated: nothing is applied
        assert_eq!(
            transition(&mut a, failed, &p),
            Effect::AttemptLoad,
            "probe still pending"
        );
        assert_eq!(a, fresh(3));
        // a matching peek clears it
        let e = transition(
            &mut a,
            Event::PeekCompleted {
                observation: PeekObservation::Matches,
            },
            &p,
        );
        assert_eq!(e, Effect::None);
        assert_eq!(a, fresh(0));
    }

    #[test]
    fn load_started_records_in_flight_and_resets_the_counter() {
        let now = Instant::now();
        let mut a = fresh(3);
        transition(
            &mut a,
            Event::LoadStarted {
                op_id: 7,
                started_at: now,
                deadline: now + POLL,
            },
            &policy(),
        );
        assert_eq!(
            a.freshness,
            Freshness::Fresh {
                consecutive_peek_failures: 0
            }
        );
        assert_eq!(a.load_in_flight.map(|f| f.op_id), Some(7));
    }

    #[test]
    fn load_started_while_errored_keeps_the_backoff() {
        let now = Instant::now();
        let mut a = errored(Duration::from_secs(120), now);
        transition(
            &mut a,
            Event::LoadStarted {
                op_id: 1,
                started_at: now,
                deadline: now + POLL,
            },
            &policy(),
        );
        assert_eq!(
            a.freshness,
            Freshness::Errored {
                backoff: Duration::from_secs(120),
                next_attempt: now
            }
        );
    }

    #[test]
    fn peek_result_is_ignored_while_errored() {
        let now = Instant::now();
        let mut a = errored(POLL, now);
        for ev in [
            Event::PeekCompleted {
                observation: PeekObservation::Failed,
            },
            Event::PeekTimedOut,
        ] {
            assert_eq!(transition(&mut a, ev, &policy()), Effect::None);
        }
        assert!(a.is_errored());
    }

    #[test]
    fn finishes_clear_only_their_own_operation() {
        let now = Instant::now();
        let mut a = fresh(0);
        a.peek_in_flight = Some(inflight(9, now));
        a.load_in_flight = Some(inflight(10, now));
        transition(&mut a, Event::PeekFinished { op_id: 8 }, &policy());
        transition(
            &mut a,
            Event::LoadCompleted {
                op_id: Some(4),
                caller: Caller::Watcher,
                ok: true,
                completed_at: now,
            },
            &policy(),
        );
        assert!(
            a.peek_in_flight.is_some(),
            "stale op_id must not clear a newer peek"
        );
        assert!(
            a.load_in_flight.is_some(),
            "stale op_id must not clear a newer load"
        );
        transition(&mut a, Event::PeekFinished { op_id: 9 }, &policy());
        transition(
            &mut a,
            Event::LoadCompleted {
                op_id: Some(10),
                caller: Caller::Watcher,
                ok: true,
                completed_at: now,
            },
            &policy(),
        );
        assert!(a.peek_in_flight.is_none());
        assert!(a.load_in_flight.is_none());
    }

    #[test]
    fn successful_load_from_any_state_is_fresh() {
        let now = Instant::now();
        for mut a in [fresh(3), errored(Duration::from_secs(600), now)] {
            transition(&mut a, done(Caller::External, true, now), &policy());
            assert_eq!(
                a.freshness,
                Freshness::Fresh {
                    consecutive_peek_failures: 0
                }
            );
        }
    }

    /// Round-2 regression: an EXTERNAL failure while fresh must land in
    /// Errored, where the watcher's backoff retries it.
    #[test]
    fn failure_while_fresh_enters_errored_for_watcher_and_external() {
        let now = Instant::now();
        for caller in [Caller::Watcher, Caller::External] {
            let mut a = fresh(0);
            transition(&mut a, done(caller, false, now), &policy());
            assert_eq!(
                a.freshness,
                Freshness::Errored {
                    backoff: POLL,
                    next_attempt: now + POLL
                }
            );
            assert_eq!(
                transition(&mut a, Event::Tick { now: now + POLL }, &policy()),
                Effect::AttemptLoad
            );
        }
    }

    #[test]
    fn startup_failure_retries_on_the_first_tick() {
        let completed_at = Instant::now();
        let mut a = Availability::fresh();
        transition(
            &mut a,
            done(Caller::Startup, false, completed_at),
            &policy(),
        );
        assert_eq!(
            a.freshness,
            Freshness::Errored {
                backoff: POLL,
                next_attempt: completed_at
            }
        );
        assert_eq!(
            transition(&mut a, Event::Tick { now: completed_at }, &policy()),
            Effect::AttemptLoad
        );
    }

    #[test]
    fn watcher_failure_while_errored_doubles_backoff_up_to_the_cap() {
        let now = Instant::now();
        let mut a = errored(Duration::from_secs(600), now);
        transition(&mut a, done(Caller::Watcher, false, now), &policy());
        assert_eq!(
            a.freshness,
            Freshness::Errored {
                backoff: Duration::from_secs(900),
                next_attempt: now + Duration::from_secs(900),
            }
        );
        let mut b = errored(POLL, now);
        transition(&mut b, done(Caller::Watcher, false, now), &policy());
        assert_eq!(
            b.freshness,
            Freshness::Errored {
                backoff: 2 * POLL,
                next_attempt: now + 2 * POLL
            }
        );
    }

    #[test]
    fn external_failure_while_errored_leaves_the_ladder_unchanged() {
        let now = Instant::now();
        let before = errored(Duration::from_secs(240), now + Duration::from_secs(10));
        let mut a = before;
        transition(&mut a, done(Caller::External, false, now), &policy());
        assert_eq!(a, before);
    }

    #[test]
    fn instant_after_saturates_instead_of_panicking() {
        let now = Instant::now();
        assert_eq!(instant_after(now, POLL), now + POLL);
        assert_eq!(instant_after(now, Duration::MAX), now + OVERFLOW_FALLBACK);
    }

    /// Git `poll_interval_secs` is not capped by config validation, so the
    /// completion rows must not panic on an absurd poll interval.
    #[test]
    fn failure_with_an_absurd_poll_interval_does_not_panic() {
        let now = Instant::now();
        let p = Policy {
            poll_interval: Duration::MAX,
            max_backoff: Duration::MAX,
            ..policy()
        };
        let mut a = fresh(0);
        transition(&mut a, done(Caller::Watcher, false, now), &p);
        assert!(a.is_errored());
        transition(&mut a, done(Caller::Watcher, false, now), &p);
        assert!(a.is_errored());
    }

    #[test]
    fn overdue_is_derived_from_the_deadline() {
        let now = Instant::now();
        let mut a = fresh(0);
        a.load_in_flight = Some(InFlight {
            op_id: 1,
            started_at: now,
            deadline: now + Duration::from_secs(1),
        });
        assert!(!a.load_overdue(now));
        assert!(a.load_overdue(now + Duration::from_secs(2)));
    }

    #[test]
    fn settings_default_to_the_spec_values() {
        let s = ReloadSettings::default();
        assert_eq!(s.peek_failure_threshold, 5);
        assert_eq!(s.peek_timeout, Duration::from_secs(30));
        assert_eq!(s.load_timeout, Duration::from_secs(300));
        assert_eq!(s.max_backoff, Duration::from_secs(900));
        assert_eq!(
            ReloadSettings {
                peek_failure_threshold: 0,
                ..s
            }
            .policy(POLL)
            .peek_failure_threshold,
            1
        );
    }
}
