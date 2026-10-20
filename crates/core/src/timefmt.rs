//! Minimal civil-time helpers so the project needs no date/time crate.
//! Stored timestamps are UTC milliseconds since the Unix epoch.

use std::time::{SystemTime, UNIX_EPOCH};

const MS_PER_DAY: i64 = 86_400_000;

/// Days from 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Current wall clock as UTC milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `2026-10-18 14:03:22` shifted by `offset_ms`.
pub fn format_ms(ms: i64, offset_ms: i64) -> String {
    let t = ms + offset_ms;
    let days = t.div_euclid(MS_PER_DAY);
    let tod = t.rem_euclid(MS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        tod / 3_600_000,
        (tod / 60_000) % 60,
        (tod / 1_000) % 60
    )
}

/// Compact `20261018-140322`, used for backup directory names.
pub fn stamp_ms(ms: i64, offset_ms: i64) -> String {
    let t = ms + offset_ms;
    let days = t.div_euclid(MS_PER_DAY);
    let tod = t.rem_euclid(MS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        tod / 3_600_000,
        (tod / 60_000) % 60,
        (tod / 1_000) % 60
    )
}

#[repr(C)]
#[allow(non_snake_case)]
struct SystemTimeRaw {
    w_year: u16,
    w_month: u16,
    w_day_of_week: u16,
    w_day: u16,
    w_hour: u16,
    w_minute: u16,
    w_second: u16,
    w_milliseconds: u16,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetSystemTime(st: *mut SystemTimeRaw);
    fn GetLocalTime(st: *mut SystemTimeRaw);
}

#[cfg(windows)]
fn to_ms(st: &SystemTimeRaw) -> i64 {
    let days = days_from_civil(st.w_year as i64, st.w_month as i64, st.w_day as i64);
    let sec = (st.w_hour as i64) * 3600 + (st.w_minute as i64) * 60 + (st.w_second as i64);
    days * MS_PER_DAY + sec * 1000 + (st.w_milliseconds as i64)
}

/// Local timezone offset from UTC in milliseconds. Display only.
#[cfg(windows)]
pub fn local_offset_ms() -> i64 {
    unsafe {
        let mut utc: SystemTimeRaw = std::mem::zeroed();
        let mut local: SystemTimeRaw = std::mem::zeroed();
        GetSystemTime(&mut utc);
        GetLocalTime(&mut local);
        to_ms(&local) - to_ms(&utc)
    }
}

#[cfg(not(windows))]
pub fn local_offset_ms() -> i64 {
    0
}

/// Render a stored UTC timestamp in the machine's local timezone.
pub fn format_local(ms: i64) -> String {
    format_ms(ms, local_offset_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_roundtrip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(days_from_civil(2026, 10, 18)), (2026, 10, 18));
        assert_eq!(civil_from_days(days_from_civil(2000, 2, 29)), (2000, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn formats_timestamp() {
        let ms =
            days_from_civil(2026, 10, 18) * MS_PER_DAY + 14 * 3_600_000 + 3 * 60_000 + 2 * 1000;
        assert_eq!(format_ms(ms, 0), "2026-10-18 14:03:02");
        assert_eq!(format_ms(ms, 8 * 3_600_000), "2026-10-18 22:03:02");
        assert_eq!(stamp_ms(ms, 0), "20261018-140302");
    }
}
