//! Process-wide shutdown signal.
//!
//! Background loops (periodic tickers, pull/push replicators, host-status
//! writers) honor this Notify so daemon shutdown drains them cleanly instead
//! of letting the Tokio runtime abort them mid-await on drop.
//!
//! Callers select! on `signal().notified()` against their sleep/recv future.
//! `shutdown()` is idempotent — called from every daemon exit branch.

use std::sync::OnceLock;
use tokio::sync::Notify;

pub fn signal() -> &'static Notify {
    static NOTIFY: OnceLock<Notify> = OnceLock::new();
    NOTIFY.get_or_init(Notify::new)
}

pub fn shutdown() {
    signal().notify_waiters();
}
