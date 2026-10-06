//! The one cron parser configuration: workspace validation, the scheduler and
//! the triggers API must accept exactly the same expressions.

use croner::errors::CronError;
use croner::parser::{CronParser, Seconds};
use croner::Cron;

/// Parse a scheduler trigger's `cron`: 5 or 6 fields (seconds optional).
///
/// `sloppy_ranges` keeps the step shorthand croner 3 accepted, such as
/// `5/5 * * * *` (every 5 minutes from :05). croner 4 rejects it by default
/// and would drop existing schedules on upgrade.
pub fn parse(expr: &str) -> Result<Cron, CronError> {
    CronParser::builder()
        .seconds(Seconds::Optional)
        .sloppy_ranges(true)
        .build()
        .parse(expr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn next_minutes(expr: &str, count: usize) -> Vec<u32> {
        use chrono::Timelike;
        let cron = parse(expr).unwrap();
        let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        cron.iter_after(start)
            .take(count)
            .map(|t| t.minute())
            .collect()
    }

    #[test]
    fn accepts_five_and_six_fields() {
        assert!(parse("0 3 * * *").is_ok());
        assert!(parse("*/10 * * * * *").is_ok());
    }

    #[test]
    fn keeps_croner_3_step_shorthand() {
        // `5/5` = start at 5, step 5: rejected by croner 4's default parser.
        assert_eq!(next_minutes("5/5 * * * *", 3), vec![5, 10, 15]);
        assert_eq!(
            next_minutes("5/5 * * * *", 3),
            next_minutes("5-59/5 * * * *", 3)
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("not a cron").is_err());
        assert!(parse("61 * * * *").is_err());
        assert!(parse("").is_err());
    }
}
