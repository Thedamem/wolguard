//! Small formatting helpers shared by the daemon and the dashboard.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn local_tm(at: SystemTime) -> Option<libc::tm> {
    let seconds = at.duration_since(UNIX_EPOCH).ok()?.as_secs() as _;
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    let result = unsafe { libc::localtime_r(&seconds, &mut tm) };
    if result.is_null() {
        None
    } else {
        Some(tm)
    }
}

/// "HH:MM:SS" in the router's local timezone (UTC when TZ is unusable).
pub fn format_hms(at: SystemTime) -> String {
    match local_tm(at) {
        Some(tm) => format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec),
        None => {
            let secs = at
                .duration_since(UNIX_EPOCH)
                .map(|age| age.as_secs())
                .unwrap_or(0);
            format!(
                "{:02}:{:02}:{:02}",
                (secs / 3600) % 24,
                (secs / 60) % 60,
                secs % 60
            )
        }
    }
}

/// "YYYY-MM-DD HH:MM:SS" in the router's local timezone.
pub fn format_datetime(at: SystemTime) -> String {
    match local_tm(at) {
        Some(tm) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        ),
        None => "????-??-?? ??:??:??".to_string(),
    }
}

/// Human readable age of a timestamp: "never", "4s", "3m", "5h", "2d".
pub fn format_age(now: SystemTime, at: Option<SystemTime>) -> String {
    let Some(at) = at else {
        return "never".to_string();
    };
    match now.duration_since(at) {
        Ok(age) => format_seconds(age.as_secs()),
        Err(_) => "now".to_string(),
    }
}

/// Compact duration for log lines and the dashboard footer: "1h 02m 03s".
pub fn format_uptime(started: SystemTime, now: SystemTime) -> String {
    let secs = now
        .duration_since(started)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_seconds(secs)
}

pub fn format_seconds(total_secs: u64) -> String {
    if total_secs < 60 {
        return format!("{total_secs}s");
    }
    let days = total_secs / 86_400;
    let hours = total_secs % 86_400 / 3_600;
    let minutes = total_secs % 3_600 / 60;
    let seconds = total_secs % 60;
    if days > 0 {
        format!("{days}d {hours:02}h {minutes:02}m")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else {
        format!("{minutes}m {seconds:02}s")
    }
}

/// Sub-second duration as milliseconds: "1.2 ms".
pub fn format_millis(duration: Duration) -> String {
    let micros = duration.as_micros();
    if micros < 10_000 {
        format!("{:.1} ms", micros as f64 / 1000.0)
    } else {
        format!("{} ms", micros / 1000)
    }
}

/// Render a MAC address as "aa:bb:cc:dd:ee:ff".
pub fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_seconds() {
        assert_eq!(format_seconds(0), "0s");
        assert_eq!(format_seconds(59), "59s");
        assert_eq!(format_seconds(60), "1m 00s");
        assert_eq!(format_seconds(3_661), "1h 01m 01s");
        assert_eq!(format_seconds(90_061), "1d 01h 01m");
    }

    #[test]
    fn formats_mac() {
        assert_eq!(
            format_mac([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]),
            "aa:bb:cc:dd:ee:ff"
        );
    }

    #[test]
    fn formats_millis() {
        assert_eq!(format_millis(Duration::from_micros(1_250)), "1.2 ms");
        assert_eq!(format_millis(Duration::from_millis(250)), "250 ms");
    }

    #[test]
    fn formats_age_and_never() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        assert_eq!(format_age(now, None), "never");
        assert_eq!(
            format_age(now, Some(UNIX_EPOCH + Duration::from_secs(9_990))),
            "10s"
        );
    }
}
