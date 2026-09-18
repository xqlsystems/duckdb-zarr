//! CF-convention time decoding: `units = "<step> since <reference>"` → DuckDB `TIMESTAMP`.
//!
//! xarray writes time coordinates as plain numbers plus a `units` attribute
//! (`"hours since 1900-01-01"`) and a `calendar` attribute. Surfacing the raw
//! number is physically faithful but useless in SQL — `WHERE time >= '2024-01-01'`
//! and `date_trunc('month', time)` both need a real timestamp.
//!
//! Only the real-world calendars are decoded here. `noleap`, `360_day`, `julian`
//! and friends have arithmetic that does not map onto a wall clock at all, so
//! those columns stay raw.

use chrono::NaiveDate;
use serde_json::{Map, Value};

/// Microseconds in one day — DuckDB `TIMESTAMP` counts microseconds from the
/// Unix epoch, so every conversion here lands in that unit.
const US_PER_DAY: i64 = 86_400_000_000;

/// Parsed CF time encoding for one column.
///
/// The step is an exact rational (`step_num / step_den` microseconds) so that
/// sub-microsecond units such as `"nanoseconds since ..."` do not have to go
/// through floating point.
#[derive(Debug, Clone, PartialEq)]
pub struct CfTimeEncoding {
    /// Microseconds per unit step, numerator.
    pub step_num: i64,
    /// Microseconds per unit step, denominator (1, except 1000 for nanoseconds).
    pub step_den: i64,
    /// Reference instant, microseconds since 1970-01-01T00:00:00Z.
    pub epoch_us: i64,
    /// Earliest instant this encoding may emit. For the mixed `standard`/
    /// `gregorian` calendar that is the 1582-10-15 Gregorian reform: before it
    /// those files mean Julian dates, which proleptic arithmetic renders ~10
    /// days off. Checking only the *reference* date would miss an axis whose
    /// reference is post-reform but whose negative offsets reach back past it.
    /// `i64::MIN` for `proleptic_gregorian`, which is proleptic by definition.
    pub floor_us: i64,
}

impl CfTimeEncoding {
    /// Decode a raw integer coordinate value into microseconds since the Unix epoch.
    /// Returns `None` when the result falls outside DuckDB's representable range.
    pub fn decode_int(&self, raw: i64) -> Option<i64> {
        let offset = (raw as i128 * self.step_num as i128).div_euclid(self.step_den as i128);
        self.finish(offset)
    }

    /// Decode a raw floating-point coordinate value into microseconds since the epoch.
    ///
    /// The integer and fractional halves are scaled separately: `days since 1800-01-01`
    /// reaches ~7.0e15 microseconds, which is past the point where an f64 multiply
    /// still resolves single microseconds.
    pub fn decode_float(&self, raw: f64) -> Option<i64> {
        if !raw.is_finite() {
            return None;
        }
        let whole = raw.trunc();
        // Guard the i128 cast below and reject values that could never land in range.
        if whole.abs() >= 1.0e24 {
            return None;
        }
        let frac = raw - whole;
        let whole_us = (whole as i128 * self.step_num as i128).div_euclid(self.step_den as i128);
        let frac_us = (frac * self.step_num as f64 / self.step_den as f64).round() as i128;
        self.finish(whole_us + frac_us)
    }

    fn finish(&self, offset_us: i128) -> Option<i64> {
        let total = offset_us.checked_add(self.epoch_us as i128)?;
        // i64::MIN / i64::MAX are DuckDB's -infinity / infinity sentinels; a decoded
        // value must never collide with them.
        if total <= i64::MIN as i128 || total >= i64::MAX as i128 {
            return None;
        }
        // A value the declared calendar renders differently is NULL, not a date
        // that is silently ~10 days wrong. `decode_times := false` recovers the
        // raw offsets for anyone who actually has pre-reform data.
        if total < self.floor_us as i128 {
            return None;
        }
        Some(total as i64)
    }
}

/// Parse CF time encoding from an array's attributes.
///
/// Returns `None` when the attributes do not describe a decodable time axis —
/// no `units`, a non-time unit, an unparseable reference date, or a calendar
/// whose arithmetic differs from the proleptic Gregorian one.
pub fn parse(attrs: &Map<String, Value>) -> Option<CfTimeEncoding> {
    let units = attrs.get("units")?.as_str()?;
    // Byte offsets from the lowercased copy are reused on the original string,
    // which only holds for ASCII.
    if !units.is_ascii() {
        return None;
    }
    let lower = units.to_ascii_lowercase();
    let sep = lower.find(" since ")?;
    let (step_num, step_den) = step_micros(lower[..sep].trim())?;
    let reference = units[sep + " since ".len()..].trim();
    let (epoch_us, ymd) = parse_reference(reference)?;

    // Absent `calendar` means "standard" per CF §4.4.1.
    let calendar = attrs
        .get("calendar")
        .and_then(Value::as_str)
        .unwrap_or("standard")
        .to_ascii_lowercase();
    let floor_us = match calendar.as_str() {
        "proleptic_gregorian" => i64::MIN,
        // The mixed Julian/Gregorian calendar only agrees with the proleptic
        // Gregorian one from the 1582-10-15 reform onward. A pre-reform
        // reference means the whole axis is Julian, so leave the column raw;
        // a post-reform reference still needs a per-value floor, since negative
        // offsets can reach back past the reform (see `floor_us`).
        "standard" | "gregorian" => {
            if ymd < (1582, 10, 15) {
                return None;
            }
            days_from_civil(1582, 10, 15) * US_PER_DAY
        }
        // noleap / 365_day / all_leap / 366_day / 360_day / julian / none.
        _ => return None,
    };

    Some(CfTimeEncoding {
        step_num,
        step_den,
        epoch_us,
        floor_us,
    })
}

/// Microseconds per unit step, as an exact rational, for the UDUNITS time names
/// CF permits. `years` and `months` are deliberately absent: they have no fixed
/// length, so xarray itself refuses to decode them without extra assumptions.
/// Bare `m` is also absent — in UDUNITS it means metres, not minutes.
fn step_micros(token: &str) -> Option<(i64, i64)> {
    Some(match token {
        "weeks" | "week" => (7 * US_PER_DAY, 1),
        "days" | "day" | "d" => (US_PER_DAY, 1),
        "hours" | "hour" | "hrs" | "hr" | "h" => (3_600_000_000, 1),
        "minutes" | "minute" | "mins" | "min" => (60_000_000, 1),
        "seconds" | "second" | "secs" | "sec" | "s" => (1_000_000, 1),
        "milliseconds" | "millisecond" | "msecs" | "msec" | "ms" => (1_000, 1),
        "microseconds" | "microsecond" | "usecs" | "usec" | "us" => (1, 1),
        "nanoseconds" | "nanosecond" | "nsecs" | "nsec" | "ns" => (1, 1_000),
        _ => return None,
    })
}

/// Parse a CF reference datetime into microseconds since the Unix epoch, plus the
/// `(year, month, day)` it names (needed for the Gregorian-reform calendar gate).
///
/// Accepts the shapes real stores use: `1900-01-01`, `1800-01-01 00:00:00`,
/// `1970-01-01T00:00:00Z`, `2000-01-01 12:30:00.5 +05:30`, `0001-1-1 0:0:0`.
fn parse_reference(s: &str) -> Option<(i64, (i64, u32, u32))> {
    let s = s.trim();
    let date_end = s
        .find(|c: char| c.is_whitespace() || c == 'T' || c == 't')
        .unwrap_or(s.len());
    let (year, month, day) = parse_date(&s[..date_end])?;

    let rest =
        s[date_end..].trim_start_matches(|c: char| c.is_whitespace() || c == 'T' || c == 't');
    let (time_us, tz_offset_us) = parse_time_and_zone(rest)?;

    let days = days_from_civil(year, month, day);
    let epoch_us = (days as i128 * US_PER_DAY as i128) + time_us as i128 - tz_offset_us as i128;
    if epoch_us <= i64::MIN as i128 || epoch_us >= i64::MAX as i128 {
        return None;
    }
    Some((epoch_us as i64, (year, month, day)))
}

fn parse_date(s: &str) -> Option<(i64, u32, u32)> {
    // A leading '-' marks a negative year, not a field separator.
    let (sign, body) = match s.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1i64, s.strip_prefix('+').unwrap_or(s)),
    };
    let mut fields = body.split('-');
    let year: i64 = sign * fields.next()?.parse::<i64>().ok()?;
    let month: u32 = match fields.next() {
        Some(f) => f.parse().ok()?,
        None => 1,
    };
    let day: u32 = match fields.next() {
        Some(f) => f.parse().ok()?,
        None => 1,
    };
    if fields.next().is_some() {
        return None;
    }
    // NaiveDate::from_ymd_opt validates month range, leap years, and day-of-month
    // together; a year outside chrono's i32 range is rejected the same as one that
    // would later overflow the i64-microseconds epoch check in parse_reference.
    NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month, day)?;
    Some((year, month, day))
}

/// Split the post-date remainder into a time-of-day offset and a UTC offset,
/// both in microseconds. An empty remainder is midnight UTC.
fn parse_time_and_zone(rest: &str) -> Option<(i64, i64)> {
    let mut time_tok: Option<&str> = None;
    let mut zone_tok: Option<&str> = None;
    for tok in rest.split_whitespace() {
        if time_tok.is_none() && tok.starts_with(|c: char| c.is_ascii_digit()) {
            time_tok = Some(tok);
        } else if zone_tok.is_none() {
            zone_tok = Some(tok);
        } else {
            return None;
        }
    }

    let mut zone_us = 0i64;
    let mut clock = time_tok.unwrap_or("");
    // A zone can be glued to the clock (`00:00:00Z`, `12:30:00+05:30`) or stand
    // alone (`00:00:00 UTC`). Peel a glued one off first.
    if let Some(idx) = clock
        .char_indices()
        .find(|&(i, c)| i > 0 && (c == '+' || c == '-' || c == 'Z' || c == 'z'))
        .map(|(i, _)| i)
    {
        let (head, tail) = clock.split_at(idx);
        if zone_tok.is_some() {
            return None;
        }
        zone_tok = Some(tail);
        clock = head;
    }
    if let Some(tok) = zone_tok {
        zone_us = parse_zone(tok)?;
    }

    Some((parse_clock(clock)?, zone_us))
}

fn parse_clock(s: &str) -> Option<i64> {
    if s.is_empty() {
        return Some(0);
    }
    let mut fields = s.split(':');
    let hours: i64 = fields.next()?.parse().ok()?;
    let minutes: i64 = match fields.next() {
        Some(f) => f.parse().ok()?,
        None => 0,
    };
    // Seconds may carry a fraction (`00:00:00.5`).
    let seconds: f64 = match fields.next() {
        Some(f) => f.parse().ok()?,
        None => 0.0,
    };
    if fields.next().is_some() {
        return None;
    }
    // 24:00:00 is a legal end-of-day marker in ISO 8601; 60 seconds covers leap seconds.
    if !(0..=24).contains(&hours)
        || !(0..=59).contains(&minutes)
        || !(0.0..=60.0).contains(&seconds)
    {
        return None;
    }
    Some(hours * 3_600_000_000 + minutes * 60_000_000 + (seconds * 1_000_000.0).round() as i64)
}

/// Parse a UTC offset (`Z`, `UTC`, `+05:30`, `-0800`, `+05`, `0:00`) into microseconds.
fn parse_zone(s: &str) -> Option<i64> {
    let upper = s.to_ascii_uppercase();
    if matches!(upper.as_str(), "Z" | "UTC" | "GMT" | "UT") {
        return Some(0);
    }
    let (sign, body) = match upper.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        // UDUNITS makes the `+` optional, and its own canonical output omits it:
        // `"hours since 1800-01-01 00:00:0.0 0:00"` is how NOAA/NCEP-derived
        // stores spell a zero offset. A bare token only reaches here once the
        // clock has already been claimed, so this can't swallow a time-of-day.
        None => (1i64, upper.strip_prefix('+').unwrap_or(upper.as_str())),
    };
    let (hours, minutes): (i64, i64) = match body.split_once(':') {
        Some((h, m)) => (h.parse().ok()?, m.parse().ok()?),
        None => match body.len() {
            1 | 2 => (body.parse().ok()?, 0),
            4 => (body[..2].parse().ok()?, body[2..].parse().ok()?),
            _ => return None,
        },
    };
    if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
        return None;
    }
    Some(sign * (hours * 3_600_000_000 + minutes * 60_000_000))
}

/// Days from 1970-01-01 to `y-m-d` in the proleptic Gregorian calendar.
///
/// `y`/`m`/`d` must already be a calendar-valid date (checked by `parse_date`'s
/// `NaiveDate::from_ymd_opt` call, or a hardcoded literal below) — this panics
/// otherwise, since a garbage day count would be worse than a loud failure.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let date = NaiveDate::from_ymd_opt(i32::try_from(y).expect("year fits i32"), m, d)
        .expect("caller validated y/m/d");
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    (date - epoch).num_days()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), Value::String((*v).to_string())))
            .collect()
    }

    #[test]
    fn days_from_civil_matches_known_anchors() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        // 1900 is not a leap year; 2000 is.
        assert_eq!(
            days_from_civil(1900, 3, 1) - days_from_civil(1900, 2, 28),
            1
        );
        assert_eq!(
            days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 28),
            2
        );
    }

    #[test]
    fn arco_era5_hours_since_1900_decodes() {
        // The ARCO-ERA5 store: int64 hours since 1900-01-01, standard calendar.
        let cf = parse(&attrs(&[
            ("units", "hours since 1900-01-01"),
            ("calendar", "standard"),
        ]))
        .expect("ERA5 time units should decode");
        assert_eq!(cf.epoch_us, days_from_civil(1900, 1, 1) * US_PER_DAY);
        // Hours from the reference to 2020-01-01, back to microseconds at that date.
        let hours = (days_from_civil(2020, 1, 1) - days_from_civil(1900, 1, 1)) * 24;
        assert_eq!(
            cf.decode_int(hours),
            Some(days_from_civil(2020, 1, 1) * US_PER_DAY)
        );
    }

    #[test]
    fn float_days_keep_microsecond_resolution() {
        let cf = parse(&attrs(&[
            ("units", "days since 1800-01-01"),
            ("calendar", "proleptic_gregorian"),
        ]))
        .unwrap();
        // ersstv5's actual_range upper bound.
        let us = cf.decode_float(81204.0).unwrap();
        assert_eq!(us, (days_from_civil(1800, 1, 1) + 81204) * US_PER_DAY);
        // Half a day must land exactly on noon, not one microsecond either side.
        let noon = cf.decode_float(81204.5).unwrap();
        assert_eq!(noon - us, US_PER_DAY / 2);
    }

    #[test]
    fn reference_forms_are_accepted() {
        let epoch = |units: &str| parse(&attrs(&[("units", units)])).map(|c| c.epoch_us);
        assert_eq!(epoch("seconds since 1970-01-01"), Some(0));
        assert_eq!(epoch("seconds since 1970-01-01 00:00:00"), Some(0));
        assert_eq!(epoch("seconds since 1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch("seconds since 1970-01-01 00:00:00 UTC"), Some(0));
        assert_eq!(epoch("seconds since 1970-1-1 0:0:0"), Some(0));
        // +05:30 means the reference instant is 5.5 h *earlier* in UTC.
        assert_eq!(
            epoch("seconds since 1970-01-01 05:30:00+05:30"),
            Some(0),
            "zone offset must be subtracted, not added"
        );
        assert_eq!(
            epoch("hours since 1969-12-31 23:00:00"),
            Some(-3_600_000_000)
        );
    }

    #[test]
    fn sign_less_udunits_zone_offset_is_accepted() {
        // UDUNITS' canonical output omits the '+', and NOAA/NCEP-derived stores
        // spell a zero offset exactly this way. Rejecting it silently left the
        // whole axis raw, with no diagnostic.
        let epoch = |units: &str| parse(&attrs(&[("units", units)])).map(|c| c.epoch_us);
        let plain = epoch("hours since 1800-01-01 00:00:0.0").unwrap();
        assert_eq!(epoch("hours since 1800-01-01 00:00:0.0 0:00"), Some(plain));
        assert_eq!(
            epoch("seconds since 1970-01-01 05:30:00 5:30"),
            Some(0),
            "a sign-less zone means +, so the offset is still subtracted"
        );
        // A named local zone stays unparseable — it is not a fixed UTC offset.
        assert_eq!(epoch("seconds since 1970-01-01 00:00:00 EST"), None);
    }

    #[test]
    fn standard_calendar_floors_at_the_gregorian_reform() {
        // Post-reform reference, but negative offsets reach back past 1582-10-15,
        // where "standard" means Julian and proleptic arithmetic is ~10 days off.
        let cf = parse(&attrs(&[
            ("units", "days since 1900-01-01"),
            ("calendar", "standard"),
        ]))
        .unwrap();
        let reform = days_from_civil(1582, 10, 15) - days_from_civil(1900, 1, 1);
        assert_eq!(
            cf.decode_int(reform),
            Some(days_from_civil(1582, 10, 15) * US_PER_DAY),
            "the reform date itself is the first decodable instant"
        );
        assert_eq!(cf.decode_int(reform - 1), None);
        assert_eq!(cf.decode_float(reform as f64 - 0.5), None);
        // proleptic_gregorian declares proleptic arithmetic, so it has no floor.
        let proleptic = parse(&attrs(&[
            ("units", "days since 1900-01-01"),
            ("calendar", "proleptic_gregorian"),
        ]))
        .unwrap();
        assert!(proleptic.decode_int(reform - 1).is_some());
    }

    #[test]
    fn sub_microsecond_units_stay_exact() {
        let cf = parse(&attrs(&[("units", "nanoseconds since 1970-01-01")])).unwrap();
        assert_eq!(cf.decode_int(1_500), Some(1));
        // Floor division, so ordering survives the truncation.
        assert_eq!(cf.decode_int(-1_500), Some(-2));
    }

    #[test]
    fn non_time_and_unsupported_calendars_are_rejected() {
        assert_eq!(parse(&attrs(&[("units", "K")])), None);
        assert_eq!(parse(&attrs(&[("units", "degrees_north")])), None);
        assert_eq!(parse(&attrs(&[("units", "meters since 1970-01-01")])), None);
        // Ambiguous UDUNITS lengths.
        assert_eq!(parse(&attrs(&[("units", "years since 1970-01-01")])), None);
        assert_eq!(parse(&attrs(&[("units", "months since 1970-01-01")])), None);
        // rasm: noleap has 365-day years, so wall-clock arithmetic does not apply.
        assert_eq!(
            parse(&attrs(&[
                ("units", "days since 0001-01-01"),
                ("calendar", "noleap"),
            ])),
            None
        );
        assert_eq!(
            parse(&attrs(&[
                ("units", "days since 1900-01-01"),
                ("calendar", "360_day"),
            ])),
            None
        );
        // A "standard" reference before the Gregorian reform is a Julian date.
        assert_eq!(
            parse(&attrs(&[
                ("units", "days since 0001-01-01"),
                ("calendar", "standard"),
            ])),
            None
        );
        // proleptic_gregorian says the same date explicitly, so it decodes.
        assert!(parse(&attrs(&[
            ("units", "days since 0001-01-01"),
            ("calendar", "proleptic_gregorian"),
        ]))
        .is_some());
    }

    #[test]
    fn malformed_references_are_rejected() {
        assert_eq!(parse(&attrs(&[("units", "days since ")])), None);
        assert_eq!(parse(&attrs(&[("units", "days since not-a-date")])), None);
        assert_eq!(parse(&attrs(&[("units", "days since 1970-13-01")])), None);
        assert_eq!(parse(&attrs(&[("units", "days since 1970-02-30")])), None);
        assert_eq!(
            parse(&attrs(&[("units", "days since 1970-01-01 25:00")])),
            None
        );
    }

    #[test]
    fn out_of_range_values_decode_to_none() {
        let cf = parse(&attrs(&[("units", "days since 1970-01-01")])).unwrap();
        assert_eq!(cf.decode_int(i64::MAX), None);
        assert_eq!(cf.decode_float(f64::NAN), None);
        assert_eq!(cf.decode_float(f64::INFINITY), None);
    }
}
