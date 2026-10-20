//! Pure core of the trigger run-status view: which scheduled runs were missed.

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use serde::Serialize;
use stroem_common::models::workflow::TriggerDef;

/// A fire later than this after its scheduled time still counts as on time.
pub const MISSED_GRACE_SECS: i64 = 300;
/// Counting stops here; the UI shows `1000+`.
pub const MISSED_CAP: u32 = 1000;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MissedRuns {
    pub count: u32,
    pub capped: bool,
    pub first: DateTime<Utc>,
}

/// The `job.source_type` a trigger's jobs carry.
pub fn source_type_of(def: &TriggerDef) -> &'static str {
    match def {
        TriggerDef::Scheduler { .. } => "trigger",
        TriggerDef::Webhook { .. } => "webhook",
        TriggerDef::EventSource { .. } => "event_source",
    }
}

/// Scheduled occurrences `t` with `last_fire < t <= now - grace`.
/// `None` for an unparsable cron or when there is none.
pub fn missed_runs(
    cron: &str,
    timezone: Option<&str>,
    last_fire: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<MissedRuns> {
    let cron = stroem_common::cron::parse(cron).ok()?;
    let tz: Tz = timezone
        .and_then(|s| s.parse().ok())
        .unwrap_or(chrono_tz::UTC);
    let limit = now - Duration::seconds(MISSED_GRACE_SECS);

    let mut prev = last_fire;
    let mut first = None;
    let mut count: u32 = 0;
    for dt in cron.iter_after(last_fire.with_timezone(&tz)) {
        let utc = dt.with_timezone(&Utc);
        // A DST gap or overlap can repeat or reorder local times.
        if utc <= prev {
            continue;
        }
        if utc > limit {
            break;
        }
        if count == MISSED_CAP {
            return Some(MissedRuns {
                count,
                capped: true,
                first: first?,
            });
        }
        first.get_or_insert(utc);
        count += 1;
        prev = utc;
    }
    first.map(|first| MissedRuns {
        count,
        capped: false,
        first,
    })
}

#[cfg(test)]
mod tests;
