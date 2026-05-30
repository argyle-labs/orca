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
