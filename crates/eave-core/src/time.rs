//! Clock arithmetic shared by every crate here.
//!
//! One function, and it is here rather than beside its first caller
//! because it has a second one now. [`rfc3339_epoch`] was written for the
//! browser's peek cards (Slice BM1) and is what Slice GH1 needs to turn a
//! GitHub `updatedAt` into the epoch milliseconds every age in this app
//! is drawn from. Copying it into the engine would have left two
//! transcriptions of the same calendar to drift apart — the rule
//! `FilesSnapshot::diff_stat` established when `ui.rs`'s private
//! `files_diffstat` was deleted rather than kept in step.

/// Seconds since the epoch for `2026-08-27T09:15:22Z` and friends
/// (fractional seconds and `±hh:mm` offsets included). `None` for
/// anything that does not look like that.
pub fn rfc3339_epoch(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' && b[10] != b't' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Skip fractional seconds, then read the offset.
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    let offset = match b.get(i) {
        Some(b'Z') | Some(b'z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let oh: i64 = s.get(i + 1..i + 3)?.parse().ok()?;
            let om: i64 = s.get(i + 4..i + 6)?.parse().ok()?;
            let mag = oh * 3600 + om * 60;
            if *sign == b'+' {
                mag
            } else {
                -mag
            }
        }
        _ => return None,
    };
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

/// [`rfc3339_epoch`] in milliseconds, saturating at zero.
///
/// Every timestamp on this wire is unsigned epoch **ms**
/// (`created_ts_ms`, `last_activity_ms`), and `coarse_age` reads them.
/// A pre-1970 date is not a date any of these surfaces can mean, so it
/// floors rather than wrapping into an age far in the future.
pub fn rfc3339_epoch_ms(s: &str) -> Option<u64> {
    rfc3339_epoch(s).map(|secs| (secs.max(0) as u64).saturating_mul(1_000))
}

/// Days since 1970-01-01 for a proleptic Gregorian date — Howard
/// Hinnant's `days_from_civil`, transcribed.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_rfc3339_stamp_becomes_an_epoch() {
        assert_eq!(rfc3339_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_epoch("1970-01-02T00:00:00Z"), Some(86_400));
        assert_eq!(rfc3339_epoch("2001-09-09T01:46:40Z"), Some(1_000_000_000));
        // Fractional seconds are skipped, not parsed into the answer.
        assert_eq!(rfc3339_epoch("1970-01-01T00:00:00.500Z"), Some(0));
        // An offset moves the instant, in both directions.
        assert_eq!(rfc3339_epoch("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(rfc3339_epoch("1969-12-31T23:00:00-01:00"), Some(0));
        assert_eq!(rfc3339_epoch("not a date"), None);
        assert_eq!(rfc3339_epoch("1970-13-01T00:00:00Z"), None);
    }

    #[test]
    fn milliseconds_floor_at_the_epoch_rather_than_wrapping() {
        assert_eq!(rfc3339_epoch_ms("2001-09-09T01:46:40Z"), Some(1_000_000_000_000));
        // A date before 1970 is not an age any surface here can draw;
        // unsigned wrapping would make it the far future instead.
        assert_eq!(rfc3339_epoch_ms("1969-12-31T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_epoch_ms("nope"), None);
    }
}
