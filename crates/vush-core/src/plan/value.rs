//! Text-form value types: durations, rates, sizes, percentages and numbers.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

/// A text value that does not match the syntax its position requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueError {
    /// What the position accepts, e.g. `a duration such as 500ms, 5s, 5m or 1h30m`.
    pub expected: &'static str,
    /// The rejected text, verbatim.
    pub input: String,
}

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid value \"{}\": expected {}",
            self.input, self.expected
        )
    }
}

impl std::error::Error for ValueError {}

fn invalid(expected: &'static str, input: &str) -> ValueError {
    ValueError {
        expected,
        input: input.to_owned(),
    }
}

/// Splits leading ASCII digits off `text` and parses them as a `u64`.
///
/// Returns `None` when there are no leading digits or the value overflows.
/// Only ASCII digits are ever handed to `u64::from_str`, because it also
/// accepts a leading `+`.
fn split_digits(text: &str) -> Option<(u64, &str)> {
    let end = text
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, rest) = text.split_at(end);
    if digits.is_empty() {
        return None;
    }
    Some((digits.parse().ok()?, rest))
}

/// Parses `text` as a `u64` made only of ASCII digits.
fn parse_whole(text: &str) -> Option<u64> {
    match split_digits(text)? {
        (value, "") => Some(value),
        _ => None,
    }
}

/// Parses a duration such as `500ms`, `5m` or `1h30m`.
///
/// The text is one or more `<integer><unit>` parts with units `h`, `m`, `s`
/// and `ms`, in strictly descending order. The total must fit in a `u64`
/// number of milliseconds.
pub fn parse_duration(text: &str) -> Result<Duration, ValueError> {
    duration_millis(text)
        .map(Duration::from_millis)
        .ok_or_else(|| invalid("a duration such as 500ms, 5s, 5m or 1h30m", text))
}

/// Splits a leading duration unit off `text`.
///
/// Returns the unit's rank (`h` first, `ms` last), its size in milliseconds and
/// the remaining text. `ms` is tried before `m` so that `5ms` is not read as `5m`
/// followed by `s`.
fn split_duration_unit(text: &str) -> Option<(u8, u64, &str)> {
    let units = [
        ("ms", 3, 1),
        ("h", 0, 3_600_000),
        ("m", 1, 60_000),
        ("s", 2, 1_000),
    ];
    units.into_iter().find_map(|(suffix, rank, millis)| {
        text.strip_prefix(suffix).map(|rest| (rank, millis, rest))
    })
}

fn duration_millis(text: &str) -> Option<u64> {
    let mut rest = text;
    let mut total: u64 = 0;
    // Units rank h < m < s < ms; each part must rank after the previous one.
    let mut min_rank = 0;
    loop {
        let (count, after) = split_digits(rest)?;
        let (rank, factor, after) = split_duration_unit(after)?;
        if rank < min_rank {
            return None;
        }
        min_rank = rank + 1;
        total = total.checked_add(count.checked_mul(factor)?)?;
        if after.is_empty() {
            return Some(total);
        }
        rest = after;
    }
}

/// The time unit a [`Rate`] is counted per.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateUnit {
    Second,
    Minute,
    Hour,
}

/// A whole-number rate such as `20/s`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub count: u64,
    pub per: RateUnit,
}

impl FromStr for Rate {
    type Err = ValueError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let rate = || {
            let (count, unit) = text.split_once('/')?;
            let per = match unit {
                "s" => RateUnit::Second,
                "m" => RateUnit::Minute,
                "h" => RateUnit::Hour,
                _ => return None,
            };
            Some(Rate {
                count: parse_whole(count)?,
                per,
            })
        };
        rate().ok_or_else(|| invalid("a rate such as 20/s, 300/m or 5/h", text))
    }
}

/// A size in bytes, such as `1MiB` or `500KB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl FromStr for ByteSize {
    type Err = ValueError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let size = || {
            let (count, unit) = split_digits(text)?;
            let factor: u64 = match unit {
                "B" => 1,
                "KB" => 1_000,
                "MB" => 1_000_000,
                "GB" => 1_000_000_000,
                "KiB" => 1 << 10,
                "MiB" => 1 << 20,
                "GiB" => 1 << 30,
                _ => return None,
            };
            count.checked_mul(factor).map(ByteSize)
        };
        size().ok_or_else(|| invalid("a size such as 512B, 500KB, 2MB or 1MiB", text))
    }
}

/// A percentage from 0 to 100, stored as integer parts per million.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ratio {
    ppm: u32,
}

impl Ratio {
    /// The ratio in parts per million: `1%` is `10_000`, `100%` is `1_000_000`.
    pub fn parts_per_million(self) -> u32 {
        self.ppm
    }
}

impl FromStr for Ratio {
    type Err = ValueError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let ratio = || {
            let body = text.strip_suffix('%')?;
            let (whole, fraction) = match body.split_once('.') {
                Some((whole, fraction)) => (whole, Some(fraction)),
                None => (body, None),
            };
            // Up to four decimals, so the value is a whole number of parts per million.
            let fraction_ppm = match fraction {
                None => 0,
                Some(digits) if (1..=4).contains(&digits.len()) => {
                    parse_whole(digits)? * 10u64.pow(4 - digits.len() as u32)
                }
                Some(_) => return None,
            };
            let ppm = parse_whole(whole)?
                .checked_mul(10_000)?
                .checked_add(fraction_ppm)?;
            if ppm > 1_000_000 {
                return None;
            }
            Some(Ratio { ppm: ppm as u32 })
        };
        ratio().ok_or_else(|| {
            invalid(
                "a percentage from 0% to 100% with up to 4 decimals, such as 1% or 0.5%",
                text,
            )
        })
    }
}

/// A JSON number, keeping integers exact.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JsonNumber {
    Int(i64),
    UInt(u64),
    Float(f64),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn durations_parse_to_exact_values() {
        let cases = [
            ("500ms", Duration::from_millis(500)),
            ("5s", Duration::from_secs(5)),
            ("5m", Duration::from_secs(300)),
            ("2h", Duration::from_secs(7200)),
            ("1h30m", Duration::from_secs(5400)),
            ("1s500ms", Duration::from_millis(1500)),
            ("1m5ms", Duration::from_millis(60_005)),
            ("0s", Duration::ZERO),
        ];
        for (text, want) in cases {
            assert_eq!(parse_duration(text), Ok(want), "{text}");
        }
    }

    #[test]
    fn invalid_durations_are_rejected() {
        for text in [
            "",
            "5",
            "s",
            "ms",
            "1.5s",
            "5 m",
            " 5m",
            "5m ",
            "+5m",
            "-5m",
            "５m",
            "5d",
            "5M",
            "5mss",
            "30m1h",
            "1ms5m",
            "1m1m",
            "1h30",
            "18446744073709551616ms",
            "5124095576031h",
            "5124095576030h26m",
        ] {
            assert!(parse_duration(text).is_err(), "{text}");
        }
    }

    #[test]
    fn rates_parse_to_count_and_unit() {
        let cases = [
            ("20/s", 20, RateUnit::Second),
            ("300/m", 300, RateUnit::Minute),
            ("5/h", 5, RateUnit::Hour),
            ("0/s", 0, RateUnit::Second),
        ];
        for (text, count, per) in cases {
            assert_eq!(text.parse::<Rate>(), Ok(Rate { count, per }), "{text}");
        }
    }

    #[test]
    fn invalid_rates_are_rejected() {
        for text in [
            "",
            "20",
            "/s",
            "20/",
            "0.5/s",
            "20/d",
            "20/S",
            "20 /s",
            "20/ s",
            "+20/s",
            "-20/s",
            "２０/s",
            "18446744073709551616/s",
        ] {
            assert!(text.parse::<Rate>().is_err(), "{text}");
        }
    }

    #[test]
    fn sizes_parse_to_bytes() {
        let cases = [
            ("0B", 0),
            ("512B", 512),
            ("500KB", 500_000),
            ("2MB", 2_000_000),
            ("1GB", 1_000_000_000),
            ("64KiB", 65_536),
            ("1MiB", 1_048_576),
            ("1GiB", 1_073_741_824),
        ];
        for (text, want) in cases {
            assert_eq!(text.parse::<ByteSize>(), Ok(ByteSize(want)), "{text}");
        }
    }

    #[test]
    fn invalid_sizes_are_rejected() {
        for text in [
            "",
            "1024",
            "MB",
            "1.5MB",
            "1 MB",
            "1mb",
            "1Mb",
            "+1MB",
            "-1B",
            "18446744073709551616B",
            "17179869184GiB",
        ] {
            assert!(text.parse::<ByteSize>().is_err(), "{text}");
        }
    }

    #[test]
    fn percentages_parse_to_parts_per_million() {
        let cases = [
            ("0%", 0),
            ("1%", 10_000),
            ("0.5%", 5_000),
            ("12.25%", 122_500),
            ("99.9999%", 999_999),
            ("0.0001%", 1),
            ("100%", 1_000_000),
            ("100.0000%", 1_000_000),
        ];
        for (text, want) in cases {
            assert_eq!(
                text.parse::<Ratio>().unwrap().parts_per_million(),
                want,
                "{text}"
            );
        }
    }

    #[test]
    fn invalid_percentages_are_rejected() {
        for text in [
            "",
            "%",
            "1",
            "0.01",
            "101%",
            "100.0001%",
            "0.00001%",
            "-1%",
            "+1%",
            ".5%",
            "1.%",
            "1 %",
            "１%",
            "99999999999%",
        ] {
            assert!(text.parse::<Ratio>().is_err(), "{text}");
        }
    }
}
