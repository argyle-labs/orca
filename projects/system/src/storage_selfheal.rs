//! Fast per-host autofs self-heal loop.
//!
//! Separate from the cron scheduler ([`crate::scheduler`]) on purpose: cron is
//! minute-resolution, and stale-mount recovery wants a tighter, seconds-scale
//! cadence. This loop ticks every [`INTERVAL_SECS`], probes every declared
//! network-share mount, and force-recovers one only after it has been stale for
//! [`CONFIRM_TICKS`] consecutive probes.
//!
//! The confirm-before-act counter is the safety valve: NFS `hard` mounts have
//! long built-in `timeo`/`retrans` patience, so a single stale probe often just
//! means the server is briefly slow. Force-unmounting on that first blip would
//! cause the very outage we're preventing. Requiring N consecutive stale probes
//! rides out transient slowness and only acts on a genuinely-down source — at
//! which point autofs remounts and fails over to the next ordered source.
//!
//! Tuned defaults give ~60–90s worst-case recovery (CONFIRM_TICKS × INTERVAL +
//! remount), near-instant for a cleanly-unreachable server. Deliberately not
//! sub-10s — that range is false-positive territory for network fs.

use crate::{autofs, managed_mounts, periodic};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Seconds between self-heal ticks. Recovery latency ≈ this × [`CONFIRM_TICKS`].
pub const INTERVAL_SECS: u64 = 30;
/// Per-target liveness-probe timeout. A live NFS `stat` answers in ms; this long
/// a hang means the server is unreachable, not merely slow.
pub const PROBE_TIMEOUT_SECS: u64 = 5;
/// Consecutive stale probes required before a target is force-recovered. The
/// blip filter — 2 ticks (~60s) rides out transient server slowness.
pub const CONFIRM_TICKS: u32 = 2;

/// Spawn the self-heal loop. Returns the periodic-loop handle; the daemon drops
/// it ("leaks it") for the process lifetime, matching the scheduler convention.
pub fn spawn() -> JoinHandle<()> {
    // Per-target consecutive-stale counters, shared across ticks. `Arc` so each
    // tick's future can own a clone (the future must be `'static`); the `Mutex`
    // guards the map, held only for the (await-free) bookkeeping in `tick`.
    let counters = Arc::new(Mutex::new(HashMap::<String, u32>::new()));
    periodic::spawn(
        periodic::PeriodicSpec {
            name: "storage.selfheal.run",
            initial_delay: Duration::from_secs(10),
            interval: Duration::from_secs(INTERVAL_SECS),
        },
        periodic::boxed(move || {
            let counters = counters.clone();
            async move { tick(&counters).await }
        }),
    )
}

/// One self-heal pass: probe declared network-share targets, advance the
/// consecutive-stale counters, and recover any target that has crossed
/// [`CONFIRM_TICKS`]. Counters for now-healthy or removed mounts are cleared so
/// a recovered mount must go stale afresh before acting again.
async fn tick(counters: &Mutex<HashMap<String, u32>>) -> anyhow::Result<()> {
    let targets: Vec<String> = managed_mounts::endpoint_db::list()?
        .into_iter()
        .filter(|m| m.enabled && m.kind == "network_share")
        .map(|m| m.target)
        .collect();
    if targets.is_empty() {
        return Ok(());
    }

    let timeout = Duration::from_secs(PROBE_TIMEOUT_SECS);
    let stale: std::collections::HashSet<String> = autofs::probe_stale(&targets, timeout)
        .await
        .into_iter()
        .collect();

    // Update counters and decide who to act on, holding the lock only for the
    // bookkeeping (no `.await` while locked).
    let mut to_recover = Vec::new();
    {
        let mut counts = counters.lock().expect("selfheal counters poisoned");
        counts.retain(|target, _| targets.contains(target)); // drop removed mounts
        for target in &targets {
            if stale.contains(target) {
                let c = counts.entry(target.clone()).or_insert(0);
                *c += 1;
                if *c >= CONFIRM_TICKS {
                    to_recover.push(target.clone());
                    *c = 0; // reset so a still-down mount re-confirms before re-acting
                }
            } else {
                counts.remove(target); // healthy → clear any streak
            }
        }
    }

    for target in &to_recover {
        let (recovered, errors) = autofs::force_and_retrigger(target, timeout).await;
        if recovered {
            info!("[selfheal] recovered stale mount {target} (failed over)");
        } else {
            warn!("[selfheal] {target} still stale after recovery; errors={errors:?}");
        }
    }
    Ok(())
}
