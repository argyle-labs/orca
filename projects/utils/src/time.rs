use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch as i64, saturating to 0 on the
/// (effectively impossible) pre-epoch clock case. Centralizes the
/// `SystemTime::now().duration_since(UNIX_EPOCH)...` boilerplate.
pub fn now_secs_since_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Current time as an RFC3339 string. Single source of truth so
/// mesh/replication timestamps stay byte-identical across crates.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_secs_since_epoch_is_recent_and_positive() {
        let now = now_secs_since_epoch();
        // Sometime after 2024-01-01.
        assert!(now > 1_704_067_200, "got {now}");
        // And before year 2100.
        assert!(now < 4_102_444_800, "got {now}");
    }

    #[test]
    fn now_rfc3339_parses_back_to_datetime() {
        let s = now_rfc3339();
        let parsed = chrono::DateTime::parse_from_rfc3339(&s);
        assert!(parsed.is_ok(), "should round-trip via RFC3339: {s}");
    }
}
