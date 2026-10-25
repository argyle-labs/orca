//! Per-member clock offsets learned from refusals. A receiver tags clock-based
//! refusals with its own time; the sender records the difference and re-signs
//! once with the receiver's clock, so drift does not lock a member out.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

static OFFSETS: LazyLock<Mutex<HashMap<String, i64>>> = LazyLock::new(Mutex::default);

pub(crate) fn offset(peer: &str) -> i64 {
    OFFSETS
        .lock()
        .ok()
        .and_then(|m| m.get(peer).copied())
        .unwrap_or(0)
}

pub(crate) fn now_for(peer: &str) -> i64 {
    utils::time::now().unix_seconds() + offset(peer)
}

#[cfg(test)]
pub(crate) fn assume(peer: &str, offset: i64) {
    OFFSETS.lock().unwrap().insert(peer.to_string(), offset);
}

/// Record `peer`'s clock from a refusal. Returns true when the offset changed,
/// meaning a retry signed with the new offset can succeed.
pub(crate) fn learn(peer: &str, err: &anyhow::Error) -> bool {
    let Some(receiver_now) = utils::pki::receiver_time_in(&format!("{err:#}")) else {
        return false;
    };
    let measured = receiver_now - utils::time::now().unix_seconds();
    let Ok(mut m) = OFFSETS.lock() else {
        return false;
    };
    let previous = m.insert(peer.to_string(), measured).unwrap_or(0);
    (measured - previous).abs() > 1
}

/// Run `attempt` with `peer`'s estimated clock; when the refusal carries the
/// receiver's time and the offset moved, retry once.
pub(crate) async fn with_clock_retry<T, F, Fut>(peer: &str, mut attempt: F) -> anyhow::Result<T>
where
    F: FnMut(i64) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    match attempt(now_for(peer)).await {
        Err(e) if learn(peer, &e) => attempt(now_for(peer)).await,
        r => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_refusal_with_the_receivers_time_retries_on_its_clock() {
        let peer = utils::id::new();
        let receiver_skew = 300;
        let seen = std::sync::Mutex::new(Vec::new());
        let r = with_clock_retry(&peer, |now| {
            seen.lock().unwrap().push(now);
            async move {
                let receiver_now = utils::time::now().unix_seconds() + receiver_skew;
                if (receiver_now - now).abs() > 30 {
                    anyhow::bail!(utils::pki::with_receiver_time("expired", receiver_now));
                }
                Ok(())
            }
        })
        .await;
        r.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert!((offset(&peer) - receiver_skew).abs() <= 1);
    }

    #[tokio::test]
    async fn a_refusal_without_the_receivers_time_is_not_retried() {
        let peer = utils::id::new();
        let mut calls = 0;
        let r: anyhow::Result<()> = with_clock_retry(&peer, |_| {
            calls += 1;
            async { anyhow::bail!("refused") }
        })
        .await;
        assert!(r.is_err());
        assert_eq!(calls, 1);
    }
}
