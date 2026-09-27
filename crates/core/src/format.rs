//! Human-readable formatting following Windows Explorer conventions
//! (binary multiples labelled KB/MB/GB, German decimal comma).

use std::fmt::Write;

const UNITS: [&str; 6] = ["KB", "MB", "GB", "TB", "PB", "EB"];

/// Formats a byte count, e.g. `1536` → `1,5 KB`, `0` → `0 Bytes`.
pub fn bytes(n: u64) -> String {
    if n < 1024 {
        return format!("{n} Bytes");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let decimals = if value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    let mut s = format!("{value:.decimals$}");
    // Drop insignificant trailing zeros ("1,50" → "1,5", "2,00" → "2").
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    let mut out = s.replace('.', ",");
    out.push(' ');
    out.push_str(UNITS[unit]);
    out
}

/// Formats an integer with German thousands separators, e.g. `1234567` → `1.234.567`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(ch);
    }
    out
}

/// Formats a fraction `0.0..=1.0` as percentage, e.g. `0.1234` → `12,3 %`.
pub fn percent(f: f32) -> String {
    format!("{:.1} %", f * 100.0).replace('.', ",")
}

/// Formats Unix seconds as local-agnostic `YYYY-MM-DD HH:MM` (UTC).
pub fn unix_time(secs: u32) -> String {
    if secs == 0 {
        return String::new();
    }
    let days = i64::from(secs / 86_400);
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let mut s = String::with_capacity(16);
    let _ = write!(s, "{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, (rem % 3600) / 60);
    s
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Formats a duration in seconds with adaptive precision, e.g. `2,41 s`.
pub fn seconds(secs: f64) -> String {
    let s = if secs < 10.0 {
        format!("{secs:.2} s")
    } else if secs < 60.0 {
        format!("{secs:.1} s")
    } else {
        let m = (secs / 60.0).floor();
        return format!("{m:.0} min {:.0} s", secs - m * 60.0);
    };
    s.replace('.', ",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(bytes(0), "0 Bytes");
        assert_eq!(bytes(1023), "1023 Bytes");
        assert_eq!(bytes(1024), "1 KB");
        assert_eq!(bytes(1536), "1,5 KB");
        assert_eq!(bytes(10 * 1024 * 1024 + 1024 * 512), "10,5 MB");
        assert_eq!(bytes(500 * 1024 * 1024 * 1024), "500 GB");
        assert_eq!(bytes(u64::MAX), "16 EB");
    }

    #[test]
    fn formats_counts() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1.000");
        assert_eq!(count(1_234_567), "1.234.567");
    }

    #[test]
    fn formats_percent_and_time() {
        assert_eq!(percent(0.1234), "12,3 %");
        assert_eq!(unix_time(0), "");
        assert_eq!(unix_time(951_782_400), "2000-02-29 00:00");
        assert_eq!(unix_time(1_700_000_000), "2023-11-14 22:13");
        assert_eq!(seconds(2.414), "2,41 s");
        assert_eq!(seconds(75.0), "1 min 15 s");
    }
}
