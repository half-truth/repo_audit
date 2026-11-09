//! Proleptic Gregorian civil-date arithmetic (Howard Hinnant's algorithms).
//!
//! Python used `datetime.fromtimestamp(ts, timezone.utc)`; pulling in `chrono`
//! for four date ops is not worth the dependency, so the ~20 lines of integer
//! arithmetic live here. `i64` division truncates toward zero in both Rust and
//! C++, which is what the era formulas assume.

/// Days since 1970-01-01 for a civil date.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Civil date for a day count since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = mp + if mp < 10 { 3 } else { -9 }; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

/// Whole days from the epoch, truncating toward negative infinity so that a
/// timestamp before 1970 lands on the previous day rather than the next.
pub fn days_from_unix(ts: i64) -> i64 {
    ts.div_euclid(86400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_epoch_and_modern_dates() {
        for &(y, m, d) in &[
            (1970, 1, 1),
            (2000, 2, 29),
            (2021, 1, 1),
            (2026, 10, 4),
            (2024, 12, 31),
        ] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
    }

    #[test]
    fn known_offsets() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 1, 1), 10957);
        assert_eq!(days_from_civil(2026, 10, 4), 20730);
    }

    #[test]
    fn pre_epoch_truncates_downward() {
        // -1s is 1969-12-31, not 1970-01-01.
        assert_eq!(civil_from_days(days_from_unix(-1)), (1969, 12, 31));
        assert_eq!(civil_from_days(days_from_unix(0)), (1970, 1, 1));
        assert_eq!(civil_from_days(days_from_unix(1)), (1970, 1, 1));
    }
}
