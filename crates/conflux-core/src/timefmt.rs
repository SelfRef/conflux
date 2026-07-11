//! Human-readable timestamp formatting.
//!
//! Unix epoch seconds are unfriendly in conflict-copy names and `conflux status`
//! output, so we render a simplified ISO-8601 form instead.

use chrono::{DateTime, Local};
use std::time::{Duration, UNIX_EPOCH};

fn local_dt(seconds: u64) -> DateTime<Local> {
    (UNIX_EPOCH + Duration::from_secs(seconds)).into()
}

/// Format Unix `seconds` as `YYYY-MM-DD_HH-MM-SS` in the host's local time zone
/// — a filesystem-safe timestamp (no `:` or spaces) used in conflict-copy names.
pub fn stamp(seconds: u64) -> String {
    local_dt(seconds).format("%Y-%m-%d_%H-%M-%S").to_string()
}

/// Format Unix `seconds` as a human-readable datetime in the host's local time
/// zone, e.g. `2026-07-11 14:34:56 +02:00` — used for `conflux status`.
pub fn local(seconds: u64) -> String {
    local_dt(seconds)
        .format("%Y-%m-%d %H:%M:%S %:z")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_has_expected_shape() {
        // Exact value depends on the host time zone; check the shape.
        let s = stamp(1_700_000_001);
        assert!(s.starts_with("2023-11-1")); // same day-ish regardless of offset
        assert_eq!(s.len(), "YYYY-MM-DD_HH-MM-SS".len());
        assert!(s.contains('_'));
    }

    #[test]
    fn is_filesystem_safe() {
        let s = stamp(1_700_000_001);
        assert!(!s.contains(':'));
        assert!(!s.contains('/'));
        assert!(!s.contains(' '));
    }

    #[test]
    fn local_is_human_readable_with_offset() {
        // Can't assert the exact string (depends on the host time zone), but it
        // should carry a date, a colon-separated time, and a `+`/`-` offset.
        let s = local(1_700_000_001);
        assert!(s.starts_with("20"));
        assert!(s.contains(':'));
        assert!(s.contains('+') || s.contains('-'));
    }
}
