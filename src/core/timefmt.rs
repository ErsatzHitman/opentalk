//! Local-time helpers for history grouping and labels. The calendar maths is pure (testable);
//! only `local_offset_secs` talks to Windows to learn the UTC offset in force at a timestamp.

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Calendar fields of a local timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub year: i32,
    pub month: u32, // 1..=12
    pub day: u32,   // 1..=31
    pub hour: u32,
    pub minute: u32,
}

/// Days since 1970-01-01 -> (year, month, day). Howard Hinnant's civil_from_days.
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((y + (m <= 2) as i64) as i32, m, d)
}

/// Fields of `ts_ms` (unix ms) shifted by `offset_secs` (local = UTC + offset).
pub fn civil(ts_ms: i64, offset_secs: i64) -> Civil {
    let secs = ts_ms.div_euclid(1000) + offset_secs;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Civil { year, month, day, hour: (rem / 3600) as u32, minute: ((rem % 3600) / 60) as u32 }
}

/// Local day number (days since epoch in local time).
pub fn day_number(ts_ms: i64, offset_secs: i64) -> i64 {
    (ts_ms.div_euclid(1000) + offset_secs).div_euclid(86_400)
}

/// "Today", "Yesterday" or "Oct 9, 2026" (OpenWhispr's `formatDateGroup`).
pub fn day_label(ts_ms: i64, now_ms: i64, offset_of: &dyn Fn(i64) -> i64) -> String {
    let d = day_number(ts_ms, offset_of(ts_ms));
    let today = day_number(now_ms, offset_of(now_ms));
    match today - d {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => {
            let c = civil(ts_ms, offset_of(ts_ms));
            format!("{} {}, {}", MONTHS[(c.month - 1) as usize], c.day, c.year)
        }
    }
}

/// "14:05" (24 h; matches the default `toLocaleTimeString` 2-digit style closely enough).
pub fn clock(ts_ms: i64, offset_secs: i64) -> String {
    let c = civil(ts_ms, offset_secs);
    format!("{:02}:{:02}", c.hour, c.minute)
}

/// "Oct 9, 2026, 14:05"
pub fn full(ts_ms: i64, offset_secs: i64) -> String {
    let c = civil(ts_ms, offset_secs);
    format!("{} {}, {}, {:02}:{:02}", MONTHS[(c.month - 1) as usize], c.day, c.year, c.hour, c.minute)
}

/// Compact "when" for list rows: "14:05" today, "Yesterday 14:05", "Oct 2, 14:05" this year,
/// "Oct 2, 2025, 14:05" before that.
pub fn when_label(ts_ms: i64, now_ms: i64, offset_of: &dyn Fn(i64) -> i64) -> String {
    let off = offset_of(ts_ms);
    let d = day_number(ts_ms, off);
    let today = day_number(now_ms, offset_of(now_ms));
    let clock = clock(ts_ms, off);
    match today - d {
        0 => clock,
        1 => format!("Yesterday {clock}"),
        _ => {
            let c = civil(ts_ms, off);
            let month = MONTHS[(c.month - 1) as usize];
            if c.year == civil(now_ms, offset_of(now_ms)).year { format!("{month} {}, {clock}", c.day) } else { format!("{month} {}, {}, {clock}", c.day, c.year) }
        }
    }
}

/// Seconds to add to UTC to get local time at `ts_ms`. 0 when the OS call fails.
#[cfg(windows)]
pub fn local_offset_secs(ts_ms: i64) -> i64 {
    use windows::Win32::Foundation::SYSTEMTIME;
    use windows::Win32::System::Time::SystemTimeToTzSpecificLocalTime;
    let secs = ts_ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    let utc = SYSTEMTIME {
        wYear: y.clamp(1601, 30000) as u16,
        wMonth: m as u16,
        wDayOfWeek: 0,
        wDay: d as u16,
        wHour: (rem / 3600) as u16,
        wMinute: ((rem % 3600) / 60) as u16,
        wSecond: (rem % 60) as u16,
        wMilliseconds: 0,
    };
    let mut local = SYSTEMTIME::default();
    // SAFETY: both pointers are valid SYSTEMTIME values; a null zone means "the active time zone".
    if unsafe { SystemTimeToTzSpecificLocalTime(None, &utc, &mut local) }.is_err() {
        return 0;
    }
    let (ly, lm, ld) = (local.wYear as i32, local.wMonth as u32, local.wDay as u32);
    // Re-derive local seconds from fields and compare with UTC seconds.
    let days = days_from_civil(ly, lm, ld);
    let local_secs = days * 86_400 + local.wHour as i64 * 3600 + local.wMinute as i64 * 60 + local.wSecond as i64;
    local_secs - secs
}

#[cfg(not(windows))]
pub fn local_offset_secs(_ts_ms: i64) -> i64 {
    0
}

/// (year, month, day) -> days since 1970-01-01.
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = y as i64 - (m <= 2) as i64;
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    const UTC: &dyn Fn(i64) -> i64 = &|_| 0;

    #[test]
    fn epoch_and_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        assert_eq!(days_from_civil(2026, 10, 9), 20_735);
        assert_eq!(civil_from_days(days_from_civil(2026, 10, 9)), (2026, 10, 9));
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn civil_applies_offset() {
        // 2026-10-09 23:30 UTC, +02:00 -> 2026-10-10 01:30.
        let ts = (days_from_civil(2026, 10, 9) * 86_400 + 23 * 3600 + 30 * 60) * 1000;
        let c = civil(ts, 2 * 3600);
        assert_eq!((c.year, c.month, c.day, c.hour, c.minute), (2026, 10, 10, 1, 30));
        assert_eq!(clock(ts, 0), "23:30");
        assert_eq!(full(ts, 0), "Oct 9, 2026, 23:30");
    }

    #[test]
    fn labels() {
        let day = |d: u32, h: i64| (days_from_civil(2026, 10, d) * 86_400 + h * 3600) * 1000;
        let now = day(9, 12);
        assert_eq!(day_label(day(9, 1), now, UTC), "Today");
        assert_eq!(day_label(day(8, 23), now, UTC), "Yesterday");
        assert_eq!(day_label(day(2, 5), now, UTC), "Oct 2, 2026");
        // Same instant, different zone: 23:30 UTC on the 8th is already "Today" at +01:00.
        let plus1: &dyn Fn(i64) -> i64 = &|_| 3600;
        assert_eq!(day_label(day(8, 23) + 30 * 60_000, now, plus1), "Today");
    }

    #[test]
    fn when_labels_for_rows() {
        let day = |y: i32, m: u32, d: u32, h: i64| (days_from_civil(y, m, d) * 86_400 + h * 3600) * 1000;
        let now = day(2026, 10, 9, 12);
        assert_eq!(when_label(day(2026, 10, 9, 14) + 5 * 60_000, now, UTC), "14:05");
        assert_eq!(when_label(day(2026, 10, 8, 9), now, UTC), "Yesterday 09:00");
        assert_eq!(when_label(day(2026, 10, 2, 9), now, UTC), "Oct 2, 09:00");
        assert_eq!(when_label(day(2025, 12, 31, 23), now, UTC), "Dec 31, 2025, 23:00");
        // A timestamp after midnight in the local zone belongs to the local day.
        let plus2: &dyn Fn(i64) -> i64 = &|_| 2 * 3600;
        assert_eq!(when_label(day(2026, 10, 8, 23), now, plus2), "01:00");
    }

    #[test]
    fn local_offset_is_sane() {
        let off = local_offset_secs(1_760_000_000_000);
        assert!(off.abs() <= 14 * 3600);
    }
}
