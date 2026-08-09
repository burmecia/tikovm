//! Human-readable formatting helpers for durations, sizes, and timestamps —
//! pure and unit-tested. The TUI keeps columns narrow, so values stay terse.

use chrono::{DateTime, Utc};

/// MiB count into a terse label: `< 1024` stays `512M`, otherwise gigabytes
/// with one decimal (`1.5G`).
pub(crate) fn memory_mib(mib: u64) -> String {
    if mib >= 1024 {
        format!("{:.1}G", mib as f64 / 1024.0)
    } else {
        format!("{mib}M")
    }
}

/// Disk sizes are also in MiB; same labeling as memory.
pub(crate) fn disk_mib(mib: u64) -> String {
    memory_mib(mib)
}

/// Duration since `created`, in "top style": seconds only for sub-minute,
/// `Mm` up to an hour, `HhMMm` up to a day, `DdHh` above.
pub(crate) fn duration(total_secs: i64) -> String {
    let secs = total_secs.max(0);
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    let seconds = secs % 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        format!("{seconds}s")
    }
}

/// Elapsed since `created` relative to `now`.
pub(crate) fn uptime(now: DateTime<Utc>, created: DateTime<Utc>) -> String {
    duration((now - created).num_seconds())
}

/// `12m ago` style age for a snapshot / last event relative to `now`.
pub(crate) fn ago(now: DateTime<Utc>, when: DateTime<Utc>) -> String {
    format!("{} ago", duration((now - when).num_seconds()))
}

/// Local clock time `HH:MM:SS` for the "last refresh" column.
pub(crate) fn clock(t: DateTime<Utc>) -> String {
    t.with_timezone(&chrono::Local)
        .format("%H:%M:%S")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_labels() {
        assert_eq!(memory_mib(512), "512M");
        assert_eq!(memory_mib(1023), "1023M");
        assert_eq!(memory_mib(1024), "1.0G");
        assert_eq!(memory_mib(2560), "2.5G");
    }

    #[test]
    fn durations_terse() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(5), "5s");
        assert_eq!(duration(59), "59s");
        assert_eq!(duration(60), "1m");
        assert_eq!(duration(90), "1m");
        assert_eq!(duration(3599), "59m");
        assert_eq!(duration(3600), "1h00m");
        assert_eq!(duration(3661), "1h01m");
        assert_eq!(duration(86_400), "1d0h");
        assert_eq!(duration(172_800 + 3_600 + 120), "2d1h");
        assert_eq!(duration(-10), "0s");
    }
}
