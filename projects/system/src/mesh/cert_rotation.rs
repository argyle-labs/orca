// Wire envelopes are opaque JSON; mirrors the allow in jsonrpc.rs.
#![allow(clippy::disallowed_types)]

//! Hourly cert rotation task.
//!
//! Two paths, picked per host:
//!
//!   * **Secure path** (`has_mesh_ca_key`): self-sign new server+client certs
//!     locally and atomic-rename them over the old ones. Zero network.
//!
//!   * **Non-secure path** (no CA key): ask any other non-departed peer to
//!     sign fresh CSRs via `mesh/refresh-cert` over mTLS, falling back to the
//!     bootstrap channel when mTLS fails. Peers are not pre-filtered for CA
//!     custody — no local flag records it — so a signer without the CA key
//!     refuses and the next candidate is tried. When every candidate fails,
//!     the failure is raised as a `mesh.cert_renewal` notification and
//!     retried next tick.
//!
//! The TLS resolver in plugin_host reads from disk on every handshake, so
//! `utils::pki::atomic_write_pem` is what makes rotation seamless — no resolver
//! swap, no in-process cache.

use anyhow::{Context, Result};
use notifications::dismissable::{RaiseInput, Severity};
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use std::cmp::Reverse;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tracing::{info, warn};
use utils::framing::{read_frame, write_frame};
use utils::jsonrpc::{Message, Request, Response};

use super::pki_dir;
use crate::periodic;
use db::mesh as pdb;

/// Cheap when nothing is due (two cert parses). Hourly so a failed renewal
/// retries many times inside the 7-day refresh window instead of once a day.
const TICK_INTERVAL: Duration = Duration::from_secs(60 * 60);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

const NOTIFY_KEY: &str = "mesh.cert_renewal";
const NOTIFY_SOURCE: &str = "mesh.cert_rotation";

pub fn spawn() -> tokio::task::JoinHandle<()> {
    periodic::spawn(
        periodic::PeriodicSpec {
            name: "mesh.cert_rotation.run",
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
        return Ok(()); // not a mesh member yet
    }

    let server_pem = std::fs::read_to_string(utils::pki::mesh_server_cert_path(&pki_d))?;
    let client_pem = std::fs::read_to_string(utils::pki::mesh_client_cert_path(&pki_d))?;
    let threshold = utils::pki::PEER_REFRESH_THRESHOLD_DAYS;
    // A cert issued before the mesh stopped being called a "mesh" carries only
    // the legacy SAN, so an upgraded peer dialing `mesh.orca.local` cannot
    // validate it. Certs live 30 days and rotate lazily under 7, so waiting for
    // expiry would leave this host unreachable for up to 23 days — a partition,
    // not an upgrade. Treat a missing SAN as rotation-due and converge on the
    // first tick after the upgrade instead.
    let stale_san = utils::pki::cert_lacks_mesh_san(&server_pem);
    if stale_san {
        info!("[cert-rotation] mesh server cert predates the mesh SAN — reissuing");
    }
    let need_server =
        stale_san || utils::pki::should_rotate(&server_pem, threshold).unwrap_or(true);
    let need_client = utils::pki::should_rotate(&client_pem, threshold).unwrap_or(true);
    if !need_server && !need_client {
        return Ok(());
    }

    let renewed = renew(&pki_d, need_server, need_client).await;
    match &renewed {
        Ok(()) => {
            if let Err(e) = notifications::dismissable::dismiss(NOTIFY_KEY) {
                warn!("[cert-rotation] notify dismiss failed: {e:#}");
            }
        }
        Err(e) => report_renewal_failure(&server_pem, &client_pem, e),
    }
    renewed
}

async fn renew(pki_d: &std::path::Path, need_server: bool, need_client: bool) -> Result<()> {
    if utils::pki::has_mesh_ca_key(pki_d) {
        // Cert CN must be stable across hostname flaps — use machine_id.
        let host = crate::host_identity::machine_id().to_string();
        if need_server {
            utils::pki::reissue_mesh_server_cert(pki_d).context("self-sign mesh server cert")?;
            info!("[cert-rotation] self-reissued mesh server cert");
        }
        if need_client {
            utils::pki::reissue_mesh_client_cert(pki_d, &host)
                .context("self-sign mesh client cert")?;
            info!("[cert-rotation] self-reissued mesh client cert");
        }
        Ok(())
    } else {
        refresh_via_peer().await
    }
}

fn renewal_severity(days_left: i64) -> Severity {
    if days_left <= 3 {
        Severity::Error
    } else {
        Severity::Warn
    }
}

/// An unparseable cert counts as 0 days: it cannot serve a handshake either.
fn renewal_days_left(server_pem: &str, client_pem: &str) -> i64 {
    let days = |pem: &str| utils::pki::cert_days_remaining(pem).unwrap_or(0);
    days(server_pem).min(days(client_pem))
}

fn not_after(cert_pem: &str) -> String {
    utils::pki::cert_summary(cert_pem)
        .ok()
        .and_then(|s| utils::time::Timestamp::from_unix_seconds(s.expires_at))
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| "unparseable".to_string())
}

/// `periodic` logs tick errors at debug, so the warn here is what makes a
/// failing renewal visible before the certs actually lapse. Best-effort: a
/// notify error must never mask the renewal error.
fn report_renewal_failure(server_pem: &str, client_pem: &str, err: &anyhow::Error) {
    let days_left = renewal_days_left(server_pem, client_pem);
    warn!("[cert-rotation] mesh cert renewal failed with {days_left} day(s) left: {err:#}");
    let host = crate::host_identity::display_hostname();
    let input = RaiseInput {
        key: NOTIFY_KEY.to_string(),
        source: NOTIFY_SOURCE.to_string(),
        source_ref: None,
        severity: renewal_severity(days_left),
        actionable: true,
        fix: None,
        title: format!("Mesh cert renewal failing on {host}: {days_left} days left"),
        body: Some(format!(
            "{err:#}\nserver notAfter: {}\nclient notAfter: {}",
            not_after(server_pem),
            not_after(client_pem)
        )),
        user_id: None,
    };
    if let Err(e) = notifications::dismissable::raise(input) {
        warn!("[cert-rotation] notify raise failed: {e:#}");
    }
}

/// Non-secure refresh dispatcher. While our mesh client cert is still valid we
/// authenticate the refresh over mTLS (cheap, no envelope). If that fails, or
/// the leaf has already **expired** (an expired leaf can't authenticate the
/// very call that would renew it), we use the bootstrap channel, whose
/// long-lived cert is unaffected by leaf expiry. Any mTLS failure falls back,
/// so a signer reachable only over bootstrap still renews us before the leaf
/// lapses.
async fn refresh_via_peer() -> Result<()> {
    let pki_d = pki_dir();
    let client_valid = std::fs::read_to_string(utils::pki::mesh_client_cert_path(&pki_d))
        .ok()
        .and_then(|p| utils::pki::cert_days_remaining(&p).ok())
        .map(|days| days > 0)
        .unwrap_or(false);
    if client_valid {
        match refresh_via_peer_mtls().await {
            Ok(()) => Ok(()),
            Err(mtls_err) => {
                warn!(
                    "[cert-rotation] mTLS refresh failed, falling back to the bootstrap channel: {mtls_err:#}"
                );
                refresh_via_peer_bootstrap().await.with_context(|| {
                    format!("mTLS refresh failed ({mtls_err:#}); bootstrap fallback")
                })
            }
        }
    } else {
        warn!(
            "[cert-rotation] mesh client cert expired — refreshing leaves over the bootstrap channel"
        );
        refresh_via_peer_bootstrap().await
    }
}

/// Every other non-departed peer is a candidate. `local_secure`/`peer_secure`
/// are secrets-tier trust flags that say nothing about CA custody, so they
/// only order candidates; the signer (`listener::handle_refresh_cert`)
/// enforces CA custody and CN identity. `local_secure` peers go first as the
/// likeliest key holders, then most-recently-seen.
async fn refresh_via_peer_mtls() -> Result<()> {
    let host = crate::host_identity::machine_id().to_string();
    let mut plans: Vec<(pdb::PeerRow, Vec<String>)> = db::pool::with_pooled_or_open(|conn| {
        let peers = pdb::list_peers(conn)?;
        Ok(peers
            .into_iter()
            .filter(|p| p.departed_at.is_none() && p.peer_id != host)
            .map(|p| {
                let targets =
                    crate::mesh::dialer::dial_targets_for_peer(conn, &p.peer_id, &p.peer_addr)
                        .unwrap_or_else(|_| vec![p.peer_addr.clone()]);
                (p, targets)
            })
            .collect())
    })?;
    if plans.is_empty() {
        anyhow::bail!("no peers available to sign a refresh");
    }
    plans.sort_by_key(|(p, _)| (!p.local_secure, Reverse(p.last_seen_at)));

    let (csr_client, key_client, csr_server, key_server) = utils::pki::build_refresh_csrs(&host)?;

    for (p, targets) in plans {
        for target in targets {
            match call_refresh(&target, p.peer_port, &host, &csr_client, &csr_server).await {
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
                        p.peer_id, target, p.peer_port
                    );
                    return Ok(());
                }
                Err(e) => warn!(
                    "[cert-rotation] refresh via {} @ {} failed: {e:#}",
                    p.peer_id, target
                ),
            }
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
    let host = crate::host_identity::machine_id().to_string();
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
                .filter(|p| p.departed_at.is_none() && p.peer_id != host && p.pubkey_fp.is_some())
            {
                let fp = p.pubkey_fp.clone().unwrap_or_default();
                let targets =
                    crate::mesh::dialer::dial_targets_for_peer(conn, &p.peer_id, &p.peer_addr)
                        .unwrap_or_else(|_| vec![p.peer_addr.clone()]);
                plans.push((p, fp, targets));
            }
            Ok(plans)
        })?;
    if plans.is_empty() {
        anyhow::bail!("no peers with a pinned bootstrap fp available to sign a refresh");
    }
    plans.sort_by_key(|(p, _, _)| (!p.local_secure, Reverse(p.last_seen_at)));

    let pki_d = pki_dir();
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
            match crate::mesh::cli::dial_bootstrap_pub(
                &target,
                p.peer_port,
                &fp,
                "mesh/refresh-cert-bootstrap",
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
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&target))
        .await
        .with_context(|| format!("connect {target} timed out"))?
        .with_context(|| format!("connect {target}"))?;
    let sni = ServerName::try_from(utils::pki::MESH_SERVER_SAN)?.to_owned();
    let mut tls = connector.connect(sni, tcp).await?;

    let params = serde_json::json!({
        "joiner_hostname": joiner_hostname,
        "csr_client_pem": csr_client_pem,
        "csr_server_pem": csr_server_pem,
    });
    write_frame(
        &mut tls,
        &serde_json::to_vec(&Request::new(1, "mesh/refresh-cert", Some(params)))?,
    )
    .await?;
    let raw = tokio::time::timeout(Duration::from_secs(15), read_frame(&mut tls))
        .await
        .context("mesh/refresh-cert timed out")??;
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
        let _guard = crate::mesh::pin_home(dir);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        body(&rt)
    }

    #[serial_test::serial(env)]
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

    #[serial_test::serial(env)]
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

    #[serial_test::serial(env)]
    #[test]
    fn tick_reissues_corrupt_leaves_via_local_ca() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), |rt| {
            crate::host_identity::init(dir.path()).unwrap();
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
            assert_eq!(server_sum.cn, "orca-mesh-server");
            assert_eq!(
                client_sum.cn,
                crate::host_identity::machine_id().to_string()
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
    fn refresh_via_peer_mtls_bails_without_any_peers() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            crate::host_identity::init(dir.path()).unwrap();
            let err = refresh_via_peer_mtls().await.unwrap_err();
            assert!(
                err.to_string()
                    .contains("no peers available to sign a refresh"),
                "unexpected error: {err:#}"
            );
        });
    }

    #[test]
    fn refresh_via_peer_mtls_tries_peers_without_trust_flags() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            crate::host_identity::init(dir.path()).unwrap();
            // No set_trust: neither secure flag is set, yet the peer must still
            // be dialed — reaching the dial loop proves it was a candidate.
            let conn = db::open_default().unwrap();
            pdb::upsert_peer(
                &conn,
                &utils::id::new(),
                "peer-host",
                "127.0.0.1",
                1,
                Some("fp-1"),
                "",
            )
            .unwrap();
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
    fn refresh_via_peer_mtls_excludes_self_and_departed() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            crate::host_identity::init(dir.path()).unwrap();
            let own = crate::host_identity::machine_id().to_string();
            let departed = utils::id::new();
            let conn = db::open_default().unwrap();
            pdb::upsert_peer(&conn, &own, "me", "127.0.0.1", 1, Some("fp-self"), "").unwrap();
            pdb::upsert_peer(
                &conn,
                &departed,
                "gone",
                "127.0.0.1",
                1,
                Some("fp-gone"),
                "",
            )
            .unwrap();
            pdb::mark_peer_departed(&conn, &departed).unwrap();
            drop(conn);
            let err = refresh_via_peer_mtls().await.unwrap_err();
            assert!(
                err.to_string()
                    .contains("no peers available to sign a refresh"),
                "unexpected error: {err:#}"
            );
        });
    }

    #[test]
    fn refresh_via_peer_bootstrap_excludes_self() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            crate::host_identity::init(dir.path()).unwrap();
            let own = crate::host_identity::machine_id().to_string();
            let conn = db::open_default().unwrap();
            pdb::upsert_peer(&conn, &own, "me", "127.0.0.1", 1, Some("fp-self"), "").unwrap();
            drop(conn);
            let err = refresh_via_peer_bootstrap().await.unwrap_err();
            assert!(
                err.to_string()
                    .contains("no peers with a pinned bootstrap fp"),
                "unexpected error: {err:#}"
            );
        });
    }

    #[test]
    fn refresh_via_peer_falls_back_to_bootstrap_when_mtls_fails() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            crate::host_identity::init(dir.path()).unwrap();
            utils::pki::init_mesh_ca(&pki_dir(), TEST_CN).unwrap();
            let conn = db::open_default().unwrap();
            pdb::upsert_peer(
                &conn,
                &utils::id::new(),
                "peer-host",
                "127.0.0.1",
                1,
                Some("boot-fp-1"),
                "",
            )
            .unwrap();
            drop(conn);
            // Valid client cert → mTLS first; the dead peer fails it, so the
            // bootstrap channel must be tried against the same peer.
            let err = format!("{:#}", refresh_via_peer().await.unwrap_err());
            assert!(
                err.contains("all candidate peers refused refresh"),
                "mTLS not attempted: {err}"
            );
            assert!(
                err.contains("all candidate peers refused bootstrap refresh"),
                "bootstrap fallback not attempted: {err}"
            );
        });
    }

    #[test]
    fn refresh_via_peer_mtls_attempts_dial_then_bails_when_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            crate::host_identity::init(dir.path()).unwrap();
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
            crate::host_identity::init(dir.path()).unwrap();
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
            crate::host_identity::init(dir.path()).unwrap();
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
            crate::host_identity::init(dir.path()).unwrap();
            let pki = pki_dir();
            // A freshly-issued mesh client cert is valid (days_remaining > 0),
            // so refresh_via_peer takes the mTLS path first.
            utils::pki::init_mesh_ca(&pki, TEST_CN).unwrap();
            let err = format!("{:#}", refresh_via_peer().await.unwrap_err());
            assert!(
                err.contains("no peers available to sign a refresh"),
                "expected mTLS dispatch, got: {err}"
            );
        });
    }

    #[test]
    fn refresh_via_peer_bootstrap_attempts_dial_then_bails_when_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            crate::host_identity::init(dir.path()).unwrap();
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

    #[test]
    fn renewal_severity_thresholds() {
        assert_eq!(renewal_severity(7), Severity::Warn);
        assert_eq!(renewal_severity(4), Severity::Warn);
        assert_eq!(renewal_severity(3), Severity::Error);
        assert_eq!(renewal_severity(0), Severity::Error);
        assert_eq!(renewal_severity(-1), Severity::Error);
    }

    fn apply_schema() {
        let conn = db::open_default().unwrap();
        db::schema_fragments::apply_fragments(&conn).unwrap();
    }

    fn corrupt_leaves(pki: &std::path::Path) {
        let junk = "-----BEGIN CERTIFICATE-----\nnot a cert\n-----END CERTIFICATE-----\n";
        utils::pki::atomic_write_pem(&utils::pki::mesh_server_cert_path(pki), junk).unwrap();
        utils::pki::atomic_write_pem(&utils::pki::mesh_client_cert_path(pki), junk).unwrap();
    }

    #[test]
    fn tick_raises_renewal_notification_when_refresh_fails() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            crate::host_identity::init(dir.path()).unwrap();
            apply_schema();
            let pki = pki_dir();
            utils::pki::init_mesh_ca(&pki, TEST_CN).unwrap();
            // Without the CA key the host must renew via a peer, and with no
            // peers that renewal fails.
            std::fs::remove_file(utils::pki::mesh_ca_key_path(&pki)).unwrap();
            corrupt_leaves(&pki);

            assert!(rt_now_tick().await.is_err());

            let n = notifications::dismissable::get(NOTIFY_KEY)
                .unwrap()
                .expect("renewal notification raised");
            assert_eq!(n.source, NOTIFY_SOURCE);
            assert_eq!(n.severity, Severity::Error);
            assert!(n.actionable);
            assert!(n.title.contains("0 days left"), "title: {}", n.title);
            let body = n.body.unwrap_or_default();
            assert!(body.contains("pinned bootstrap fp"), "body: {body}");
            assert!(body.contains("notAfter"), "body: {body}");
        });
    }

    #[test]
    fn tick_dismisses_renewal_notification_on_success() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            crate::host_identity::init(dir.path()).unwrap();
            apply_schema();
            let pki = pki_dir();
            utils::pki::init_mesh_ca(&pki, TEST_CN).unwrap();
            notifications::dismissable::raise(RaiseInput {
                key: NOTIFY_KEY.to_string(),
                source: NOTIFY_SOURCE.to_string(),
                source_ref: None,
                severity: Severity::Warn,
                actionable: true,
                fix: None,
                title: "stale".to_string(),
                body: None,
                user_id: None,
            })
            .unwrap();
            corrupt_leaves(&pki);

            rt_now_tick().await.unwrap();

            let n = notifications::dismissable::get(NOTIFY_KEY)
                .unwrap()
                .expect("notification row kept");
            assert_eq!(n.state, notifications::dismissable::State::Dismissed);
        });
    }

    // Tiny wrapper so the two CA-cleanup tests can `.await tick()` inside the
    // async body without re-block_on-ing (with_home_db already drives the rt).
    async fn rt_now_tick() -> Result<()> {
        tick().await
    }
}
