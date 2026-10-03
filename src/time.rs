use chrono::{DateTime, Duration, NaiveDate, Utc};

use crate::error::{Error, Result};

/// Parses a `--since` value into the cutoff timestamp it means.
///
/// Accepted forms: a relative duration (`30m`, `48h`, `7d`, `2w`, `1y`) or an
/// absolute date/timestamp (`2026-09-01`, or any RFC 3339 value, which is
/// normalized to UTC). `now` is injected so tests do not depend on the clock.
pub fn parse_since(raw: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(invalid(raw));
    }

    if let Ok(when) = DateTime::parse_from_rfc3339(value) {
        return Ok(when.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        let midnight = date.and_hms_opt(0, 0, 0).ok_or_else(|| invalid(raw))?;
        return Ok(midnight.and_utc());
    }

    let Some((amount, unit)) = split_duration(value) else {
        return Err(invalid(raw));
    };
    let seconds = match unit {
        'm' => amount.checked_mul(60),
        'h' => amount.checked_mul(3_600),
        'd' => amount.checked_mul(86_400),
        'w' => amount.checked_mul(7 * 86_400),
        'y' => amount.checked_mul(365 * 86_400),
        _ => return Err(invalid(raw)),
    }
    .ok_or_else(|| invalid(raw))?;

    let delta = Duration::try_seconds(seconds).ok_or_else(|| invalid(raw))?;
    now.checked_sub_signed(delta).ok_or_else(|| invalid(raw))
}

/// Splits `7d` into `(7, 'd')`; the unit letter is lowercased so `7D` works.
fn split_duration(value: &str) -> Option<(i64, char)> {
    let mut chars = value.chars();
    let unit = chars.next_back()?;
    let digits: String = chars.collect();
    if digits.is_empty() || !digits.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some((digits.parse().ok()?, unit.to_ascii_lowercase()))
}

fn invalid(raw: &str) -> Error {
    Error::InvalidValue(format!(
        "invalid --since value: `{raw}` (use a duration like 7d/48h/2w or a date like 2026-09-01)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn noon() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, 0).unwrap()
    }

    #[test]
    fn parses_relative_durations() {
        let now = noon();
        assert_eq!(
            parse_since("30m", now).unwrap(),
            now - Duration::minutes(30)
        );
        assert_eq!(parse_since("48h", now).unwrap(), now - Duration::hours(48));
        assert_eq!(parse_since("7d", now).unwrap(), now - Duration::days(7));
        assert_eq!(parse_since("2w", now).unwrap(), now - Duration::days(14));
        assert_eq!(parse_since("1y", now).unwrap(), now - Duration::days(365));
        assert_eq!(parse_since(" 3d ", now).unwrap(), now - Duration::days(3));
        assert_eq!(parse_since("3D", now).unwrap(), now - Duration::days(3));
    }

    #[test]
    fn parses_dates_and_rfc3339_timestamps() {
        let now = noon();
        assert_eq!(
            parse_since("2026-09-01", now).unwrap(),
            Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap(),
            "a bare date means midnight UTC"
        );
        assert_eq!(
            parse_since("2026-09-01T06:30:00Z", now).unwrap(),
            Utc.with_ymd_and_hms(2026, 9, 1, 6, 30, 0).unwrap()
        );
        assert_eq!(
            parse_since("2026-09-01T08:00:00+02:00", now).unwrap(),
            Utc.with_ymd_and_hms(2026, 9, 1, 6, 0, 0).unwrap(),
            "offsets normalize to UTC"
        );
    }

    #[test]
    fn rejects_garbage_and_out_of_range_values() {
        let now = noon();
        for raw in [
            "",
            "   ",
            "7x",
            "d7",
            "7 d",
            "2026-13-40",
            "99999999999999999999d",
            "yesterday",
        ] {
            let err = parse_since(raw, now).unwrap_err();
            assert_eq!(err.exit_code(), 1, "raw: {raw:?}, error: {err}");
            assert!(
                err.to_string().contains("--since"),
                "raw: {raw:?}, error: {err}"
            );
        }
    }
}
