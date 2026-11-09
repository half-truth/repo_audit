//! Wall-clock helpers.
//!
//! Python captured `NOW = datetime.now(timezone.utc)` at module import time, so
//! every `months_ago` call in one run saw the same instant. `OnceLock`
//! reproduces that: first read wins, and the whole process agrees.

use super::civil::{civil_from_days, days_from_unix};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_utc() -> (i64, i64) {
    static NOW: OnceLock<(i64, i64)> = OnceLock::new();
    *NOW.get_or_init(|| {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let (y, m, _) = civil_from_days(days_from_unix(secs));
        (y, m)
    })
}

/// `datetime.fromtimestamp(ts, tz=utc).strftime("%Y-%m-%d")`.
pub fn fmt_ts(ts: Option<i64>) -> String {
    match ts {
        None => "-".to_string(),
        Some(t) => {
            let (y, m, d) = civil_from_days(days_from_unix(t));
            format!("{:04}-{:02}-{:02}", y, m, d)
        }
    }
}

/// Whole months between `ts` and the process's captured "now".
/// Matches Python: `(NOW.year - d.year) * 12 + NOW.month - d.month`.
pub fn months_ago(ts: i64) -> i64 {
    let (y, m, _) = civil_from_days(days_from_unix(ts));
    let (ny, nm) = now_utc();
    (ny - y) * 12 + nm - m
}

/// RFC-2822 date string for a unix timestamp, as `git commit` expects:
/// `Thu Jan 1 12:00:00 2021 +0000`.
pub fn rfc2822(ts: i64) -> String {
    let days = days_from_unix(ts);
    let rem = ts - days * 86400;
    let (y, m, d) = civil_from_days(days);
    const DOW: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // 1970-01-01 was a Thursday, so index directly off the day count.
    let dow = DOW[days.rem_euclid(7) as usize];
    format!(
        "{} {} {} {:02}:{:02}:{:02} {} +0000",
        dow,
        MON[(m - 1) as usize],
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        y
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_epoch_and_known_dates() {
        assert_eq!(fmt_ts(Some(0)), "1970-01-01");
        assert_eq!(fmt_ts(Some(1609502400)), "2021-01-01"); // 2021-01-01T12:00:00Z
        assert_eq!(fmt_ts(None), "-");
    }

    #[test]
    fn rfc2822_matches_git_date_order() {
        assert_eq!(rfc2822(1609502400), "Fri Jan 1 12:00:00 2021 +0000");
        assert_eq!(rfc2822(0), "Thu Jan 1 00:00:00 1970 +0000");
    }

    #[test]
    fn months_ago_is_whole_months() {
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(months_ago(now), 0);
        // ~61 days back is two months, not 61/30.
        assert!((1..=3).contains(&months_ago(now - 61 * 86400)));
    }
}
