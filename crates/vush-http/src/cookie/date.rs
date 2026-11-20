//! The cookie-date parser of RFC 6265 §5.1.1.

/// Whether `b` separates tokens: `0x09`, `0x20..=0x2F`, `0x3B..=0x40`,
/// `0x5B..=0x60` or `0x7B..=0x7E`.
fn is_delimiter(b: u8) -> bool {
    matches!(b, 0x09 | 0x20..=0x2F | 0x3B..=0x40 | 0x5B..=0x60 | 0x7B..=0x7E)
}

/// Reads `min` to `max` digits from the start of `text`. The digits must be
/// followed by a non-digit or the end. Gives the value and the digits' length.
fn digits(text: &[u8], min: usize, max: usize) -> Option<(u32, usize)> {
    let len = text.iter().take_while(|b| b.is_ascii_digit()).count();
    if len < min || len > max {
        return None;
    }
    let value = text[..len]
        .iter()
        .fold(0, |acc, b| acc * 10 + u32::from(b - b'0'));
    Some((value, len))
}

/// `1*2DIGIT ":" 1*2DIGIT ":" 1*2DIGIT`, then a non-digit and anything.
fn time(token: &[u8]) -> Option<(u32, u32, u32)> {
    let (hour, n) = digits(token, 1, 2)?;
    let rest = token[n..].strip_prefix(b":")?;
    let (minute, n) = digits(rest, 1, 2)?;
    let rest = rest[n..].strip_prefix(b":")?;
    let (second, _) = digits(rest, 1, 2)?;
    Some((hour, minute, second))
}

/// The month whose first three letters start `token`, ignoring case, from 1.
fn month(token: &[u8]) -> Option<u32> {
    const NAMES: [&[u8; 3]; 12] = [
        b"jan", b"feb", b"mar", b"apr", b"may", b"jun", b"jul", b"aug", b"sep", b"oct", b"nov",
        b"dec",
    ];
    let head = token.get(..3)?;
    NAMES
        .iter()
        .position(|name| head.eq_ignore_ascii_case(*name))
        .map(|i| i as u32 + 1)
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the given civil date (Howard Hinnant's
/// `days_from_civil`). `month` is 1 to 12 and `day` is 1 to 31.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Reads a cookie date, as RFC 6265 §5.1.1 says. Gives seconds since the
/// Unix epoch, UTC, or `None` when `text` isn't a cookie date.
pub(crate) fn parse_date(text: &[u8]) -> Option<i64> {
    let mut found_time = None;
    let mut found_day = None;
    let mut found_month = None;
    let mut found_year = None;
    for token in text.split(|&b| is_delimiter(b)).filter(|t| !t.is_empty()) {
        if found_time.is_none()
            && let Some(t) = time(token)
        {
            found_time = Some(t);
        } else if found_day.is_none()
            && let Some((d, _)) = digits(token, 1, 2)
        {
            found_day = Some(d);
        } else if found_month.is_none()
            && let Some(m) = month(token)
        {
            found_month = Some(m);
        } else if found_year.is_none()
            && let Some((y, _)) = digits(token, 2, 4)
        {
            found_year = Some(y);
        }
    }
    let (hour, minute, second) = found_time?;
    let (day, month, year) = (found_day?, found_month?, found_year?);
    let year = i64::from(match year {
        70..=99 => year + 1900,
        0..=69 => year + 2000,
        _ => year,
    });
    if !(1..=31).contains(&day) || year < 1601 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    if day > days_in_month(year, month) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + i64::from(hour * 3600 + minute * 60 + second))
}

#[cfg(test)]
mod tests {
    use super::parse_date;

    fn p(text: &str) -> Option<i64> {
        parse_date(text.as_bytes())
    }

    #[test]
    fn the_three_common_formats_parse() {
        assert_eq!(p("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784111777));
        assert_eq!(p("Sunday, 06-Nov-94 08:49:37 GMT"), Some(784111777));
        assert_eq!(p("Sun Nov  6 08:49:37 1994"), Some(784111777));
    }

    #[test]
    fn the_zone_token_is_ignored() {
        assert_eq!(p("Wed, 21 Oct 2015 07:28:00 UTC"), Some(1445412480));
        assert_eq!(p("Wed, 21 Oct 2015 07:28:00 -0000"), Some(1445412480));
    }

    #[test]
    fn two_digit_years_follow_the_rule() {
        assert_eq!(p("01 Jan 69 00:00:00"), Some(3124224000));
        assert_eq!(p("01 Jan 70 00:00:00"), Some(0));
        assert_eq!(p("31 Dec 99 23:59:59"), Some(946684799));
    }

    #[test]
    fn tokens_may_carry_trailing_text() {
        assert_eq!(p("06th Nov 1994 08:49:37gmt"), Some(784111777));
        assert_eq!(p("6 NOVEMBER 1994 8:9:7"), Some(784109347));
    }

    #[test]
    fn leap_days_are_checked() {
        assert_eq!(p("29 Feb 2000 00:00:00"), Some(951782400));
        assert_eq!(p("29 Feb 1900 00:00:00"), None);
        assert_eq!(p("30 Feb 2001 00:00:00"), None);
    }

    #[test]
    fn the_earliest_year_is_1601() {
        assert_eq!(p("01 Jan 1601 00:00:00"), Some(-11644473600));
        assert_eq!(p("31 Dec 1600 23:59:59"), None);
    }

    #[test]
    fn bad_dates_fail() {
        for text in [
            "",
            "01 Jan 2001",
            "01 2001 00:00:00",
            "00 Jan 2001 00:00:00",
            "32 Jan 2001 00:00:00",
            "01 Jan 2001 24:00:00",
            "01 Jan 2001 00:60:00",
            "01 Jan 2001 00:00:60",
            "01 Jan 2001 123:00:00",
        ] {
            assert_eq!(p(text), None, "{text:?}");
        }
    }
}
