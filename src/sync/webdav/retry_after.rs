use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, RETRY_AFTER};

pub(super) fn parse(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    parse_value(value, unix_now())
}

fn parse_value(value: &str, now_unix: u64) -> Option<Duration> {
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        let seconds = value.bytes().fold(0_u64, |seconds, byte| {
            seconds
                .saturating_mul(10)
                .saturating_add(u64::from(byte - b'0'))
        });
        return Some(Duration::from_secs(seconds));
    }
    let target = parse_imf_fixdate(value)?;
    Some(Duration::from_secs(target.saturating_sub(now_unix)))
}

fn parse_imf_fixdate(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() != 29
        || !matches!(
            &bytes[0..3],
            b"Mon" | b"Tue" | b"Wed" | b"Thu" | b"Fri" | b"Sat" | b"Sun"
        )
        || &bytes[3..5] != b", "
        || bytes[7] != b' '
        || bytes[11] != b' '
        || bytes[16] != b' '
        || bytes[19] != b':'
        || bytes[22] != b':'
        || &bytes[25..29] != b" GMT"
    {
        return None;
    }
    let day = decimal(&bytes[5..7])?;
    let month = match &bytes[8..11] {
        b"Jan" => 1,
        b"Feb" => 2,
        b"Mar" => 3,
        b"Apr" => 4,
        b"May" => 5,
        b"Jun" => 6,
        b"Jul" => 7,
        b"Aug" => 8,
        b"Sep" => 9,
        b"Oct" => 10,
        b"Nov" => 11,
        b"Dec" => 12,
        _ => return None,
    };
    let year = decimal(&bytes[12..16])?;
    let hour = decimal(&bytes[17..19])?;
    let minute = decimal(&bytes[20..22])?;
    let second = decimal(&bytes[23..25])?;
    if year < 1970
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let years = year.checked_sub(1970)?;
    let leap_days = leap_years_before(year).checked_sub(leap_years_before(1970))?;
    let mut days = years.checked_mul(365)?.checked_add(leap_days)?;
    for prior_month in 1..month {
        days = days.checked_add(days_in_month(year, prior_month))?;
    }
    days = days.checked_add(day.checked_sub(1)?)?;
    days.checked_mul(86_400)?
        .checked_add(hour.checked_mul(3_600)?)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)
}

fn decimal(bytes: &[u8]) -> Option<u64> {
    bytes.iter().try_fold(0_u64, |value, byte| {
        value
            .checked_mul(10)?
            .checked_add(u64::from(byte.checked_sub(b'0')?))
            .filter(|_| byte.is_ascii_digit())
    })
}

fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: u64) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn leap_years_before(year: u64) -> u64 {
    let prior = year.saturating_sub(1);
    prior / 4 - prior / 100 + prior / 400
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_delta_seconds_and_imf_fixdate() {
        assert_eq!(parse_value("75", 1_000), Some(Duration::from_secs(75)));
        assert_eq!(
            parse_value("Sun, 06 Nov 1994 08:49:37 GMT", 784_111_700),
            Some(Duration::from_secs(77))
        );
    }

    #[test]
    fn past_date_is_an_immediate_hint_and_invalid_dates_are_ignored() {
        assert_eq!(
            parse_value("Sun, 06 Nov 1994 08:49:37 GMT", 784_111_777),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_value("Sun, 31 Feb 2026 08:49:37 GMT", 0), None);
        assert_eq!(parse_value("not a date", 0), None);
    }

    #[test]
    fn oversized_delta_seconds_saturate_instead_of_dropping_the_embargo() {
        assert_eq!(
            parse_value("999999999999999999999999999999999999", 0),
            Some(Duration::from_secs(u64::MAX))
        );
    }
}
