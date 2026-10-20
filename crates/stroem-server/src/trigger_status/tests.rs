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
    assert_eq!(r.first, last + Duration::seconds(2));
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

fn attention_item(
    name: &str,
    fires_here: bool,
    last: Option<(&str, &str)>,
    hidden: bool,
    missed: Option<u32>,
) -> TriggerStatusItem {
    let def: TriggerDef =
        serde_yaml::from_str("type: scheduler\ncron: '0 1 * * *'\ntask: a\n").unwrap();
    let mut info = TriggerInfo::from_def(name, &def, 0);
    info.last_run = last.map(|(status, at)| LastRun {
        job_id: Uuid::new_v4(),
        created_at: t(at),
        status: status.to_string(),
        retry_job_id: None,
    });
    info.last_run_hidden = hidden;
    info.missed = missed.map(|count| MissedRuns {
        count,
        capped: false,
        first: t("2026-10-20T01:00:00Z"),
    });
    TriggerStatusItem {
        workspace: "etl".to_string(),
        fires_here,
        def,
        info,
    }
}

#[test]
fn needs_attention_rules() {
    let at = "2026-10-20T01:00:00Z";
    assert!(needs_attention(&attention_item(
        "a",
        true,
        None,
        false,
        Some(1)
    )));
    assert!(needs_attention(&attention_item(
        "a",
        true,
        Some(("failed", at)),
        false,
        None
    )));
    for status in ["cancelled", "skipped", "completed"] {
        assert!(!needs_attention(&attention_item(
            "a",
            true,
            Some((status, at)),
            false,
            None
        )));
    }
    assert!(!needs_attention(&attention_item(
        "a",
        false,
        Some(("failed", at)),
        false,
        None
    )));
    assert!(!needs_attention(&attention_item(
        "a",
        false,
        None,
        false,
        Some(3)
    )));
    // A hidden run never makes a trigger need attention by itself.
    assert!(!needs_attention(&attention_item(
        "a", true, None, true, None
    )));
}

fn attention(item: TriggerStatusItem) -> AttentionTrigger {
    let failed = last_run_failed(&item);
    AttentionTrigger {
        workspace: item.workspace,
        last_run_failed: failed,
        trigger: item.info,
    }
}

#[test]
fn attention_order() {
    let mut v = vec![
        attention(attention_item(
            "failed-old",
            true,
            Some(("failed", "2026-10-19T00:00:00Z")),
            false,
            None,
        )),
        attention(attention_item(
            "missed-2",
            true,
            Some(("completed", "2026-10-18T00:00:00Z")),
            false,
            Some(2),
        )),
        attention(attention_item(
            "failed-new",
            true,
            Some(("failed", "2026-10-20T00:00:00Z")),
            false,
            None,
        )),
        attention(attention_item(
            "missed-5",
            true,
            Some(("failed", "2026-10-17T00:00:00Z")),
            false,
            Some(5),
        )),
    ];
    sort_attention(&mut v);
    let names: Vec<&str> = v.iter().map(|a| a.trigger.name.as_str()).collect();
    assert_eq!(names, ["missed-5", "missed-2", "failed-new", "failed-old"]);
    assert!(v[0].last_run_failed && !v[1].last_run_failed);
}

#[test]
fn hidden_failed_run_is_not_last_run_failed() {
    let item = attention_item("a", true, None, true, Some(1));
    assert!(!last_run_failed(&item));
}

#[test]
fn fire_recorded_just_before_its_occurrence_is_not_missed() {
    let r = missed_runs(
        "0 1 * * *",
        None,
        t("2026-10-19T00:59:59.990Z"),
        t("2026-10-19T09:00:00Z"),
    );
    assert_eq!(r, None);
}

#[test]
fn occurrence_beyond_the_tolerance_is_still_missed() {
    let r = missed_runs(
        "0 1 * * *",
        None,
        t("2026-10-19T00:59:54Z"),
        t("2026-10-19T09:00:00Z"),
    )
    .unwrap();
    assert_eq!(r.count, 1);
    assert_eq!(r.first, t("2026-10-19T01:00:00Z"));
}

#[test]
fn adjacent_occurrence_after_a_late_fire_is_missed() {
    let r = missed_runs(
        "0,1 1 * * *",
        None,
        t("2026-10-19T01:00:40Z"),
        t("2026-10-19T09:00:00Z"),
    )
    .unwrap();
    assert_eq!(r.count, 1);
    assert_eq!(r.first, t("2026-10-19T01:01:00Z"));
}

#[test]
fn only_one_occurrence_is_absorbed() {
    let tt = t("2026-10-19T10:00:00Z");
    let r = missed_runs(
        "* * * * * *",
        None,
        tt - Duration::milliseconds(500),
        tt + Duration::minutes(10),
    )
    .unwrap();
    assert_eq!(r.first, tt + Duration::seconds(1));
    assert_eq!(r.count, 300);
}
