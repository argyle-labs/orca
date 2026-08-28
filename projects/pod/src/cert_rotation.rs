// Wire envelopes are opaque JSON; mirrors the allow in jsonrpc.rs.
#![allow(clippy::disallowed_types)]

//! Daily cert rotation task.
//!
//! Two paths, picked per host:
//!
//!   * **Secure path** (`has_mesh_ca_key`): self-sign new server+client certs
//!     locally and atomic-rename them over the old ones. Zero network.
//!
//!   * **Non-secure path** (no CA key): pick any active mutual-secure peer
//!     that does have the CA key, dial it on the mTLS pod surface, call
//!     `pod/refresh-cert` with fresh CSRs, install the returned certs.
//!     If every candidate peer is unreachable, log and retry next tick.
//!
//! The TLS resolver in plugin_host reads from disk on every handshake, so
//! `utils::pki::atomic_write_pem` is what makes rotation seamless — no resolver
//! swap, no in-process cache.

use anyhow::{Context, Result};
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tracing::{info, warn};
use utils::framing::{read_frame, write_frame};
use utils::jsonrpc::{Message, Request, Response};

use super::pki_dir;
use db::pod as pdb;
use system::periodic;

/// Once per day. Cheap (one cert parse + a comparison), and a stale cert
/// check on this cadence covers a 7-day refresh threshold comfortably.
const TICK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

pub fn spawn() -> tokio::task::JoinHandle<()> {
    periodic::spawn(
        periodic::PeriodicSpec {
            name: "pod.cert_rotation.run",
            // Small initial delay so we don't slam the daemon on every restart.
            initial_delay: Duration::from_secs(60),
            interval: TICK_INTERVAL,
        },
        periodic::boxed(tick),
    )
}

async fn tick() -> Result<()> {
    let pki_d = pki_dir();

    // Drop the previous CA slot once its overlap window has elapsed. Done
    // unconditionally (independent of whether leaf rotation is needed) so a
    // host that's been online through a rotation eventually shrinks back
    // to a single trust anchor without a daemon restart.
    if utils::pki::has_mesh_ca_previous(&pki_d)
        && let Err(e) = db::pool::with_pooled_or_open(|conn| {
            if let Ok(Some(expires_at)) = pdb::get_ca_previous_expires_at(conn)
                && now_secs() > expires_at
            {
                if let Err(e) = utils::pki::drop_mesh_ca_previous(&pki_d) {
                    warn!("[cert-rotation] could not drop previous CA: {e:#}");
                } else {
                    _ = pdb::set_ca_previous_expires_at(conn, None);
                    info!("[cert-rotation] dropped previous CA (overlap expired)");
                }
            }
            Ok(())
        })
    {
        warn!("[cert-rotation] previous-CA cleanup skipped (db unavailable): {e:#}");
    }

    if !utils::pki::mesh_server_cert_path(&pki_d).exists() {
        return Ok(()); // not a pod member yet
    }

    let server_pem = std::fs::read_to_string(utils::pki::mesh_server_cert_path(&pki_d))?;
    let client_pem = std::fs::read_to_string(utils::pki::mesh_client_cert_path(&pki_d))?;
    let threshold = utils::pki::PEER_REFRESH_THRESHOLD_DAYS;
    let need_server = utils::pki::should_rotate(&server_pem, threshold).unwrap_or(true);
    let need_client = utils::pki::should_rotate(&client_pem, threshold).unwrap_or(true);
    if !need_server && !need_client {
        return Ok(());
    }

    if utils::pki::has_mesh_ca_key(&pki_d) {
        // Cert CN must be stable across hostname flaps — use machine_id.
        let host = system::host_identity::machine_id().to_string();
        if need_server {
            utils::pki::reissue_mesh_server_cert(&pki_d).context("self-sign mesh server cert")?;
            info!("[cert-rotation] self-reissued mesh server cert");
        }
        if need_client {
            utils::pki::reissue_mesh_client_cert(&pki_d, &host)
                .context("self-sign mesh client cert")?;
            info!("[cert-rotation] self-reissued mesh client cert");
        }
    } else {
        refresh_via_peer().await?;
    }
    Ok(())
}

/// Non-secure refresh dispatcher. While our mesh client cert is still valid we
/// authenticate the refresh over mTLS (cheap, no envelope). Once it has
/// **expired**, mTLS can no longer authenticate us to a signer — an expired
/// leaf can't authenticate the very call that would renew it — so we fall back
/// to the bootstrap channel, whose long-lived cert is unaffected by leaf
/// expiry. This is what lets a host that missed its refresh window (e.g. daemon
/// down across the 7-day threshold) still self-heal instead of deadlocking.
async fn refresh_via_peer() -> Result<()> {
    let pki_d = pki_dir();
    let client_valid = std::fs::read_to_string(utils::pki::mesh_client_cert_path(&pki_d))
        .ok()
        .and_then(|p| utils::pki::cert_days_remaining(&p).ok())
        .map(|days| days > 0)
        .unwrap_or(false);
    if client_valid {
        refresh_via_peer_mtls().await
    } else {
        warn!(
            "[cert-rotation] mesh client cert expired — refreshing leaves over the bootstrap channel"
        );
        refresh_via_peer_bootstrap().await
    }
}

async fn refresh_via_peer_mtls() -> Result<()> {
    let peers = db::pool::with_pooled_or_open(pdb::list_peers)?;
    // Prefer mutually-secure peers (those have the CA key). Skip departed.
    let mut candidates: Vec<_> = peers
        .into_iter()
        .filter(|p| p.departed_at.is_none() && p.local_secure && p.peer_secure)
        .collect();
    if candidates.is_empty() {
        anyhow::bail!("no mutual-secure peers available to sign a refresh");
    }
    // Most-recently-seen first to maximize success likelihood.
    candidates.sort_by_key(|p| std::cmp::Reverse(p.last_seen_at));

    let host = system::host_identity::machine_id().to_string();
    let (csr_client, key_client, csr_server, key_server) = utils::pki::build_refresh_csrs(&host)?;

    for p in candidates {
        match call_refresh(&p.peer_addr, p.peer_port, &host, &csr_client, &csr_server).await {
            Ok((client_cert, server_cert)) => {
                let pki_d = pki_dir();
                utils::pki::install_refreshed_peer_certs(
                    &pki_d,
                    &client_cert,
                    &key_client,
                    &server_cert,
                    &key_server,
                )?;
                info!(
                    "[cert-rotation] refreshed peer certs via {} ({}:{})",
                    p.peer_id, p.peer_addr, p.peer_port
                );
                return Ok(());
            }
            Err(e) => warn!("[cert-rotation] {} refused refresh: {e:#}", p.peer_id),
        }
    }
    anyhow::bail!("all candidate peers refused refresh");
}

/// Bootstrap-channel refresh: used when our mesh client cert has expired and
/// can no longer authenticate an mTLS refresh. We sign the CSRs with our
/// long-lived bootstrap key and dial each mutual-secure peer's bootstrap SNI
/// (pinned to its bootstrap fp). The peer verifies our signed envelope against
/// its own pinned record of us, then signs the CSRs. Dial targets come from the
/// multi-address dialer so a peer with a stale legacy addr is still reached.
async fn refresh_via_peer_bootstrap() -> Result<()> {
    // Any non-departed peer with a pinned bootstrap fp is a candidate. We do
    // NOT require mutual-secure here: a host whose leaf expired has usually
    // already had its `peer_secure` flag drop across the fleet (peers stop
    // trusting an unreachable member), so gating on mutual-secure would
    // exclude the exact recovery case. The signer authorizes us server-side
    // (known non-departed peer + matching bootstrap fp) and only a CA-key
    // holder can actually sign — non-holders just return an error and we move
    // on. Order `local_secure` first (most likely a CA-key holder we trust),
    // then most-recently-seen.
    let mut plans: Vec<(pdb::PeerRow, String, Vec<String>)> =
        db::pool::with_pooled_or_open(|conn| {
            let peers = pdb::list_peers(conn)?;
            let mut plans: Vec<(pdb::PeerRow, String, Vec<String>)> = Vec::new();
            for p in peers
                .into_iter()
                .filter(|p| p.departed_at.is_none() && p.pubkey_fp.is_some())
            {
                let fp = p.pubkey_fp.clone().unwrap_or_default();
                let targets = crate::dialer::dial_targets_for_peer(conn, &p.peer_id, &p.peer_addr)
                    .unwrap_or_else(|_| vec![p.peer_addr.clone()]);
                plans.push((p, fp, targets));
            }
            Ok(plans)
        })?;
    if plans.is_empty() {
        anyhow::bail!("no peers with a pinned bootstrap fp available to sign a refresh");
    }
    plans.sort_by_key(|(p, _, _)| (!p.local_secure, std::cmp::Reverse(p.last_seen_at)));

    let pki_d = pki_dir();
    let host = system::host_identity::machine_id().to_string();
    let (csr_client, key_client, csr_server, key_server) = utils::pki::build_refresh_csrs(&host)?;
    let signing = utils::pki::load_or_init_bootstrap_key(&pki_d)?;

    #[derive(serde::Serialize)]
    struct RefreshCertBootstrapBody<'a> {
        joiner_hostname: &'a str,
        csr_client_pem: &'a str,
        csr_server_pem: &'a str,
    }
    let env = utils::pki::sign_envelope(
        &signing,
        &RefreshCertBootstrapBody {
            joiner_hostname: &host,
            csr_client_pem: &csr_client,
            csr_server_pem: &csr_server,
        },
    )?;
    let params = serde_json::to_value(&env)?;

    for (p, fp, targets) in plans {
        for target in targets {
            match crate::cli::dial_bootstrap_pub(
                &target,
                p.peer_port,
                &fp,
                "pod/refresh-cert-bootstrap",
                params.clone(),
            )
            .await
            {
                Ok(v) => {
                    let client_cert = v
                        .get("client_cert_pem")
                        .and_then(|x| x.as_str())
                        .context("bootstrap refresh response missing client_cert_pem")?
                        .to_string();
                    let server_cert = v
                        .get("server_cert_pem")
                        .and_then(|x| x.as_str())
                        .context("bootstrap refresh response missing server_cert_pem")?
                        .to_string();
                    utils::pki::install_refreshed_peer_certs(
                        &pki_d,
                        &client_cert,
                        &key_client,
                        &server_cert,
                        &key_server,
                    )?;
                    info!(
                        "[cert-rotation] refreshed leaf certs over bootstrap via {} ({})",
                        p.peer_id, target
                    );
                    return Ok(());
                }
                Err(e) => warn!(
                    "[cert-rotation] bootstrap refresh via {} @ {} failed: {e:#}",
                    p.peer_id, target
                ),
            }
        }
    }
    anyhow::bail!("all candidate peers refused bootstrap refresh");
}

async fn call_refresh(
    host: &str,
    port: u16,
    joiner_hostname: &str,
    csr_client_pem: &str,
    csr_server_pem: &str,
) -> Result<(String, String)> {
    let pki_d = pki_dir();
    let bundle = utils::pki::load_mesh_client(&pki_d)?;
    let (chain, key) = utils::pki::parse_cert_and_key(&bundle.cert_pem, &bundle.key_pem)?;
    let roots = utils::pki::ca_root_store(&bundle.ca_cert_pem)?;
    let client_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key)?;

    let connector = TlsConnector::from(Arc::new(client_config));
    let target = format!("{host}:{port}");
    let tcp = TcpStream::connect(&target)
        .await
        .with_context(|| format!("connect {target}"))?;
    let sni = ServerName::try_from(utils::pki::POD_SERVER_SAN)?.to_owned();
    let mut tls = connector.connect(sni, tcp).await?;

    let params = serde_json::json!({
        "joiner_hostname": joiner_hostname,
        "csr_client_pem": csr_client_pem,
        "csr_server_pem": csr_server_pem,
    });
    write_frame(
        &mut tls,
        &serde_json::to_vec(&Request::new(1, "pod/refresh-cert", Some(params)))?,
    )
    .await?;
    let raw = tokio::time::timeout(Duration::from_secs(15), read_frame(&mut tls))
        .await
        .context("pod/refresh-cert timed out")??;
    let msg: Message = serde_json::from_slice(&raw)?;
    let resp: Response = match msg {
        Message::Response(r) => r,
        _ => anyhow::bail!("non-response frame"),
    };
    if let Some(err) = resp.error {
        anyhow::bail!("peer rejected refresh: {}", err.message);
    }
    let r = resp.result.context("empty refresh result")?;
    let client_cert = r
        .get("client_cert_pem")
        .and_then(|v| v.as_str())
        .context("response missing client_cert_pem")?
        .to_string();
    let server_cert = r
        .get("server_cert_pem")
        .and_then(|v| v.as_str())
        .context("response missing server_cert_pem")?
        .to_string();
    Ok((client_cert, server_cert))
}

use utils::time::now_secs_since_epoch as now_secs;

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CN: &str = "24647a14a251e863cdf8dcee692f2915";

    // `tick()` reads `pki_dir()`, which is derived from the process-global
    /// Run `body` with HOME repointed at `dir`, serialized behind the crate-wide
    /// HOME lock and restored afterwards. `body` gets a live current-thread runtime
    /// handle so `tick()` executes while HOME (hence `pki_dir()`) points at the temp
    /// dir. The lock is crate-wide so this can't race a roster_sync or cli HOME test.
    fn with_home<T>(dir: &std::path::Path, body: impl FnOnce(&tokio::runtime::Runtime) -> T) -> T {
        let _guard = crate::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("HOME").ok();
        // SAFETY: HOME is set for the duration of the closure and restored
        // immediately after; access is serialized behind ENV_LOCK.
        unsafe { std::env::set_var("HOME", dir) };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = body(&rt);
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        out
    }

    #[test]
    fn tick_is_noop_for_non_member_host() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), |rt| {
            let pki = pki_dir();
            assert!(!utils::pki::mesh_server_cert_path(&pki).exists());
            rt.block_on(tick()).unwrap();
            assert!(!utils::pki::mesh_server_cert_path(&pki).exists());
            assert!(!utils::pki::mesh_client_cert_path(&pki).exists());
        });
    }

    #[test]
    fn tick_leaves_fresh_certs_untouched() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), |rt| {
            let pki = pki_dir();
            utils::pki::init_mesh_ca(&pki, TEST_CN).unwrap();
            let before_server =
                std::fs::read_to_string(utils::pki::mesh_server_cert_path(&pki)).unwrap();
            let before_client =
                std::fs::read_to_string(utils::pki::mesh_client_cert_path(&pki)).unwrap();
            rt.block_on(tick()).unwrap();
            let after_server =
                std::fs::read_to_string(utils::pki::mesh_server_cert_path(&pki)).unwrap();
            let after_client =
                std::fs::read_to_string(utils::pki::mesh_client_cert_path(&pki)).unwrap();
            assert_eq!(before_server, after_server);
            assert_eq!(before_client, after_client);
        });
    }

    #[test]
    fn tick_reissues_corrupt_leaves_via_local_ca() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), |rt| {
            system::host_identity::init(dir.path()).unwrap();
            let pki = pki_dir();
            utils::pki::init_mesh_ca(&pki, TEST_CN).unwrap();
            let junk = "-----BEGIN CERTIFICATE-----\nnot a cert\n-----END CERTIFICATE-----\n";
            utils::pki::atomic_write_pem(&utils::pki::mesh_server_cert_path(&pki), junk).unwrap();
            utils::pki::atomic_write_pem(&utils::pki::mesh_client_cert_path(&pki), junk).unwrap();
            rt.block_on(tick()).unwrap();
            let server = std::fs::read_to_string(utils::pki::mesh_server_cert_path(&pki)).unwrap();
            let client = std::fs::read_to_string(utils::pki::mesh_client_cert_path(&pki)).unwrap();
            let server_sum = utils::pki::cert_summary(&server).unwrap();
            let client_sum = utils::pki::cert_summary(&client).unwrap();
            assert_eq!(server_sum.cn, "orca-pod-server");
            assert_eq!(
                client_sum.cn,
                system::host_identity::machine_id().to_string()
            );
        });
    }

    // Run `body` with both HOME (→ pki_dir) and an ephemeral DB pointed at temp
    // locations, on a current-thread runtime. `db::with_db_path` uses a
    // task-local override that survives on the single-threaded executor.
    fn with_home_db<T>(dir: &std::path::Path, body: impl std::future::Future<Output = T>) -> T {
        with_home(dir, |rt| {
            let db_path = dir.join("orca-test.db");
            rt.block_on(db::with_db_path(db_path, body))
        })
    }

    #[test]
    fn refresh_via_peer_mtls_bails_without_mutual_secure_peers() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            // Fresh DB has no peers at all → no mutual-secure candidates.
            let err = refresh_via_peer_mtls().await.unwrap_err();
            assert!(
                err.to_string().contains("no mutual-secure peers"),
                "unexpected error: {err:#}"
            );
        });
    }

    #[test]
    fn refresh_via_peer_mtls_attempts_dial_then_bails_when_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            system::host_identity::init(dir.path()).unwrap();
            // Seed one non-departed mutual-secure peer at an unroutable target.
            // mtls path builds CSRs, sorts candidates, dials, and exhausts the
            // loop against the dead peer.
            let conn = db::open_default().unwrap();
            let peer_id = utils::id::new();
            pdb::upsert_peer(
                &conn,
                &peer_id,
                "peer-host",
                "127.0.0.1",
                1,
                Some("fp-1"),
                "",
            )
            .unwrap();
            pdb::set_trust(&conn, &peer_id, Some(true), Some(true)).unwrap();
            drop(conn);
            let err = refresh_via_peer_mtls().await.unwrap_err();
            assert!(
                err.to_string()
                    .contains("all candidate peers refused refresh"),
                "unexpected error: {err:#}"
            );
        });
    }

    #[test]
    fn refresh_via_peer_bootstrap_bails_without_pinned_fp_peers() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            // Fresh DB has no peers → no pinned bootstrap fp to sign against.
            let err = refresh_via_peer_bootstrap().await.unwrap_err();
            assert!(
                err.to_string()
                    .contains("no peers with a pinned bootstrap fp"),
                "unexpected error: {err:#}"
            );
        });
    }

    #[test]
    fn refresh_via_peer_dispatches_to_bootstrap_when_client_cert_absent() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            // No mesh client cert on disk → cert_days_remaining fails → treated
            // as expired → bootstrap channel. Empty DB then bails on the
            // bootstrap-specific message, proving the dispatch went that way.
            let err = refresh_via_peer().await.unwrap_err();
            assert!(
                err.to_string().contains("pinned bootstrap fp"),
                "expected bootstrap dispatch, got: {err:#}"
            );
        });
    }

    #[test]
    fn refresh_via_peer_dispatches_to_mtls_when_client_cert_valid() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            let pki = pki_dir();
            // A freshly-issued mesh client cert is valid (days_remaining > 0),
            // so refresh_via_peer takes the mTLS path.
            utils::pki::init_mesh_ca(&pki, TEST_CN).unwrap();
            let err = refresh_via_peer().await.unwrap_err();
            assert!(
                err.to_string().contains("mutual-secure"),
                "expected mTLS dispatch, got: {err:#}"
            );
        });
    }

    #[test]
    fn refresh_via_peer_bootstrap_attempts_dial_then_bails_when_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            system::host_identity::init(dir.path()).unwrap();
            // Seed one non-departed peer carrying a pinned pubkey_fp and an
            // unroutable address, so the bootstrap path builds CSRs, signs the
            // envelope, and exhausts the dial loop against a dead target.
            let conn = db::open_default().unwrap();
            let peer_id = utils::id::new();
            pdb::upsert_peer(
                &conn,
                &peer_id,
                "peer-host",
                "127.0.0.1",
                1,
                Some("boot-fp-1"),
                "",
            )
            .unwrap();
            drop(conn);
            let err = refresh_via_peer_bootstrap().await.unwrap_err();
            assert!(
                err.to_string()
                    .contains("all candidate peers refused bootstrap refresh"),
                "unexpected error: {err:#}"
            );
        });
    }

    #[test]
    fn tick_drops_previous_ca_when_overlap_expired() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            let pki = pki_dir();
            utils::pki::init_mesh_ca(&pki, TEST_CN).unwrap();
            // Rotate to populate the previous CA slot, then mark its overlap
            // window as already elapsed (expires far in the past).
            utils::pki::rotate_mesh_ca(&pki).unwrap();
            assert!(utils::pki::has_mesh_ca_previous(&pki));
            let conn = db::open_default().unwrap();
            pdb::set_ca_previous_expires_at(&conn, Some(1)).unwrap();
            drop(conn);

            rt_now_tick().await.unwrap();

            // Previous slot dropped and DB expiry cleared.
            assert!(!utils::pki::has_mesh_ca_previous(&pki));
            let conn = db::open_default().unwrap();
            assert_eq!(pdb::get_ca_previous_expires_at(&conn).unwrap(), None);
        });
    }

    #[test]
    fn tick_keeps_previous_ca_while_overlap_active() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            let pki = pki_dir();
            utils::pki::init_mesh_ca(&pki, TEST_CN).unwrap();
            utils::pki::rotate_mesh_ca(&pki).unwrap();
            assert!(utils::pki::has_mesh_ca_previous(&pki));
            // Overlap window ends far in the future → must be retained.
            let future = now_secs() + 86_400;
            let conn = db::open_default().unwrap();
            pdb::set_ca_previous_expires_at(&conn, Some(future)).unwrap();
            drop(conn);

            rt_now_tick().await.unwrap();

            assert!(utils::pki::has_mesh_ca_previous(&pki));
            let conn = db::open_default().unwrap();
            assert_eq!(
                pdb::get_ca_previous_expires_at(&conn).unwrap(),
                Some(future)
            );
        });
    }

    // Tiny wrapper so the two CA-cleanup tests can `.await tick()` inside the
    // async body without re-block_on-ing (with_home_db already drives the rt).
    async fn rt_now_tick() -> Result<()> {
        tick().await
    }
}
