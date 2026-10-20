use super::*;
use chrono::Duration;
use stroem_common::models::workflow::TriggerDef;

fn t(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

#[test]
fn on_time_daily_run_is_not_missed() {
    let r = missed_runs(
        "0 1 * * *",
        None,
        t("2026-10-20T01:00:40Z"),
        t("2026-10-20T09:00:00Z"),
    );
    assert_eq!(r, None);
}

#[test]
fn late_run_within_grace_is_not_missed() {
    let r = missed_runs(
        "0 1 * * *",
        None,
        t("2026-10-19T01:00:40Z"),
        t("2026-10-20T01:04:59Z"),
    );
    assert_eq!(r, None);
}

#[test]
fn one_missed_daily_run() {
    let r = missed_runs(
        "0 1 * * *",
        None,
        t("2026-10-19T01:00:40Z"),
        t("2026-10-20T01:05:00Z"),
    );
    assert_eq!(
        r,
        Some(MissedRuns {
            count: 1,
            capped: false,
            first: t("2026-10-20T01:00:00Z")
        })
    );
}

#[test]
fn outage_counts_every_missed_fire() {
    let r = missed_runs(
        "*/5 * * * *",
        None,
        t("2026-10-19T09:55:00Z"),
        t("2026-10-20T02:17:00Z"),
    )
    .unwrap();
    assert_eq!(r.count, 195);
    assert_eq!(r.first, t("2026-10-19T10:00:00Z"));
    assert!(!r.capped);
}

#[test]
fn count_stops_at_the_cap() {
    let last = t("2026-10-20T00:00:00Z");
    let r = missed_runs("* * * * * *", None, last, last + Duration::hours(2)).unwrap();
    assert_eq!(r.count, 1000);
    assert!(r.capped);
    assert_eq!(r.first, last + Duration::seconds(1));
}

#[test]
fn six_field_cron_is_accepted() {
    let last = t("2026-10-20T00:00:00Z");
    let r = missed_runs("*/10 * * * * *", None, last, last + Duration::minutes(6)).unwrap();
    assert_eq!(r.count, 6);
}

#[test]
fn timezone_is_applied() {
    let r = missed_runs(
        "0 3 * * *",
        Some("Europe/Copenhagen"),
        t("2026-10-18T01:00:00Z"),
        t("2026-10-20T09:00:00Z"),
    )
    .unwrap();
    assert_eq!(r.count, 2);
    assert_eq!(r.first, t("2026-10-19T01:00:00Z"));
}

#[test]
fn dst_gap_counts_each_utc_hour_once() {
    let r = missed_runs(
        "0 * * * *",
        Some("Europe/Copenhagen"),
        t("2026-03-28T23:00:00Z"),
        t("2026-03-29T05:05:00Z"),
    )
    .unwrap();
    assert_eq!(r.count, 6);
}

#[test]
fn bad_timezone_falls_back_to_utc() {
    let r = missed_runs(
        "0 1 * * *",
        Some("Mars/Base"),
        t("2026-10-19T01:00:00Z"),
        t("2026-10-20T02:00:00Z"),
    )
    .unwrap();
    assert_eq!(r.count, 1);
    assert_eq!(r.first, t("2026-10-20T01:00:00Z"));
}

#[test]
fn unparsable_cron_gives_no_mark() {
    let r = missed_runs(
        "not a cron",
        None,
        t("2026-10-19T01:00:00Z"),
        t("2026-10-20T02:00:00Z"),
    );
    assert_eq!(r, None);
}

#[test]
fn source_type_mapping() {
    let sched: TriggerDef =
        serde_yaml::from_str("type: scheduler\ncron: '0 1 * * *'\ntask: a\n").unwrap();
    let hook: TriggerDef = serde_yaml::from_str("type: webhook\nname: h\ntask: a\n").unwrap();
    let es: TriggerDef =
        serde_yaml::from_str("type: event_source\ntask: a\ntarget_task: b\n").unwrap();
    assert_eq!(source_type_of(&sched), "trigger");
    assert_eq!(source_type_of(&hook), "webhook");
    assert_eq!(source_type_of(&es), "event_source");
}
