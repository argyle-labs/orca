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

/// Caps one candidate's whole exchange (connect, TLS handshake, request,
/// response): a peer that accepts TCP and then stalls must cost one slot in
/// the candidate list, not the tick.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Caps one renewal across every candidate so a long candidate list still
/// leaves the hourly retry cadence intact.
const RENEW_TIMEOUT: Duration = Duration::from_secs(10 * 60);

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

    let server_pem = read_leaf(&utils::pki::mesh_server_cert_path(&pki_d));
    let client_pem = read_leaf(&utils::pki::mesh_client_cert_path(&pki_d));
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
        // Leaves renewed out of band (manual reissue, re-join) never pass
        // through the success arm below.
        clear_renewal_notification();
        return Ok(());
    }

    let renewed =
        match tokio::time::timeout(RENEW_TIMEOUT, renew(&pki_d, need_server, need_client)).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!(
                "renewal did not finish within {}s",
                RENEW_TIMEOUT.as_secs()
            )),
        };
    match &renewed {
        Ok(()) => clear_renewal_notification(),
        Err(e) => report_renewal_failure(&server_pem, &client_pem, e),
    }
    renewed
}

/// An unreadable leaf reads as empty, which parses as due-for-renewal: a cert
/// this host cannot read cannot serve a handshake either, so it must renew
/// (and raise the notification if that fails) rather than end the tick quietly.
fn read_leaf(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| {
        warn!(
            "[cert-rotation] cannot read {}: {e} — treating it as due for renewal",
            path.display()
        );
        String::new()
    })
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
        refresh_via_peer(DialPolicy::live()).await
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

fn renewal_title(host: &str, days_left: i64) -> String {
    if days_left < 0 {
        format!(
            "Mesh cert renewal failing on {host}: expired {} days ago",
            -days_left
        )
    } else {
        format!("Mesh cert renewal failing on {host}: {days_left} days left")
    }
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
        title: renewal_title(host, days_left),
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

/// Dismiss only an active notification: dismissing a suppressed one would
/// undo the operator's "ignore permanently".
fn clear_renewal_notification() {
    let active = match notifications::dismissable::get(NOTIFY_KEY) {
        Ok(n) => n.is_some_and(|n| n.state == notifications::dismissable::State::Active),
        Err(e) => {
            warn!("[cert-rotation] notify lookup failed: {e:#}");
            return;
        }
    };
    if active && let Err(e) = notifications::dismissable::dismiss(NOTIFY_KEY) {
        warn!("[cert-rotation] notify dismiss failed: {e:#}");
    }
}

/// How refresh candidates are dialed.
#[derive(Debug, Clone, Copy)]
struct DialPolicy {
    /// The port every mesh listener binds by default; tried after a roster's
    /// stored port when the two differ.
    canonical_port: u16,
    exchange_timeout: Duration,
}

impl DialPolicy {
    fn live() -> Self {
        Self {
            canonical_port: db::ports::mesh_port(),
            exchange_timeout: EXCHANGE_TIMEOUT,
        }
    }
}

/// One (peer, address, port) to ask for a refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    peer_id: String,
    addr: String,
    port: u16,
    /// Bootstrap pubkey fp to pin; unused on the mTLS path.
    pinned_fp: Option<String>,
}

impl std::fmt::Display for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} @ {}:{}", self.peer_id, self.addr, self.port)
    }
}

/// Each dial target on the roster's stored port, then on the canonical port.
/// Roster rows can carry a dead ephemeral port while the peer listens on the
/// canonical one; the stored port still goes first because a host configured
/// with a non-default mesh port is reachable only there.
fn peer_candidates(p: &pdb::PeerRow, targets: &[String], canonical_port: u16) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for addr in targets {
        for port in [p.peer_port, canonical_port] {
            if !out.iter().any(|c| c.addr == *addr && c.port == port) {
                out.push(Candidate {
                    peer_id: p.peer_id.clone(),
                    addr: addr.clone(),
                    port,
                    pinned_fp: p.pubkey_fp.clone(),
                });
            }
        }
    }
    out
}

/// Ask candidates in order until one returns leaves that `install` accepts.
/// Every failure — dial, refusal, timeout, or leaves that fail verification —
/// moves on to the next candidate, so one bad or stalled signer cannot end
/// the search. Returns the candidate that succeeded, or the last failure.
async fn first_installed<F, Fut>(
    candidates: Vec<Candidate>,
    exchange_timeout: Duration,
    mut fetch: F,
    install: impl Fn(&str, &str) -> Result<()>,
) -> Result<Candidate>
where
    F: FnMut(Candidate) -> Fut,
    Fut: std::future::Future<Output = Result<(String, String)>>,
{
    let mut last_err = anyhow::anyhow!("no dialable candidates");
    for c in candidates {
        let fetched = match tokio::time::timeout(exchange_timeout, fetch(c.clone())).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!(
                "exchange timed out after {}ms",
                exchange_timeout.as_millis()
            )),
        };
        match fetched.and_then(|(client, server)| install(&client, &server)) {
            Ok(()) => return Ok(c),
            Err(e) => {
                warn!("[cert-rotation] refresh via {c} failed: {e:#}");
                last_err = e.context(format!("via {c}"));
            }
        }
    }
    Err(last_err)
}

fn leaf_pair(v: &serde_json::Value) -> Result<(String, String)> {
    let field = |name: &str| {
        v.get(name)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .with_context(|| format!("refresh response missing {name}"))
    };
    Ok((field("client_cert_pem")?, field("server_cert_pem")?))
}

/// Non-secure refresh dispatcher. While our mesh client cert is still valid we
/// authenticate the refresh over mTLS (cheap, no envelope). If that fails, or
/// the leaf has already **expired** (an expired leaf can't authenticate the
/// very call that would renew it), we use the bootstrap channel, whose
/// long-lived cert is unaffected by leaf expiry. Any mTLS failure falls back,
/// so a signer reachable only over bootstrap still renews us before the leaf
/// lapses.
async fn refresh_via_peer(policy: DialPolicy) -> Result<()> {
    let pki_d = pki_dir();
    let client_valid = std::fs::read_to_string(utils::pki::mesh_client_cert_path(&pki_d))
        .ok()
        .and_then(|p| utils::pki::cert_days_remaining(&p).ok())
        .map(|days| days > 0)
        .unwrap_or(false);
    if client_valid {
        match refresh_via_peer_mtls(policy).await {
            Ok(()) => Ok(()),
            Err(mtls_err) => {
                warn!(
                    "[cert-rotation] mTLS refresh failed, falling back to the bootstrap channel: {mtls_err:#}"
                );
                refresh_via_peer_bootstrap(policy).await.with_context(|| {
                    format!("mTLS refresh failed ({mtls_err:#}); bootstrap fallback")
                })
            }
        }
    } else {
        warn!(
            "[cert-rotation] mesh client cert expired — refreshing leaves over the bootstrap channel"
        );
        refresh_via_peer_bootstrap(policy).await
    }
}

/// Every other non-departed peer is a candidate. `local_secure`/`peer_secure`
/// are secrets-tier trust flags that say nothing about CA custody, so they
/// only order candidates; the signer (`listener::handle_refresh_cert`)
/// enforces CA custody and CN identity. `local_secure` peers go first as the
/// likeliest key holders, then most-recently-seen.
async fn refresh_via_peer_mtls(policy: DialPolicy) -> Result<()> {
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
    let candidates: Vec<Candidate> = plans
        .iter()
        .flat_map(|(p, targets)| peer_candidates(p, targets, policy.canonical_port))
        .collect();

    let pki_d = pki_dir();
    let (csr_client, key_client, csr_server, key_server) = utils::pki::build_refresh_csrs(&host)?;
    let fetch = |c: Candidate| {
        let (host, csr_client, csr_server) = (&host, &csr_client, &csr_server);
        async move { call_refresh(&c.addr, c.port, host, csr_client, csr_server).await }
    };
    let install = |client: &str, server: &str| {
        utils::pki::install_refreshed_peer_certs(
            &pki_d,
            &host,
            client,
            &key_client,
            server,
            &key_server,
        )
    };
    let used = first_installed(candidates, policy.exchange_timeout, fetch, install)
        .await
        .context("all candidate peers refused refresh")?;
    info!("[cert-rotation] refreshed peer certs via {used}");
    Ok(())
}

/// Bootstrap-channel refresh: used when our mesh client cert has expired and
/// can no longer authenticate an mTLS refresh. We sign the CSRs with our
/// long-lived bootstrap key and dial each peer's bootstrap SNI (pinned to its
/// bootstrap fp). The peer verifies our signed envelope against its own pinned
/// record of us, then signs the CSRs. Dial targets come from the multi-address
/// dialer so a peer with a stale legacy addr is still reached.
async fn refresh_via_peer_bootstrap(policy: DialPolicy) -> Result<()> {
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
    let mut plans: Vec<(pdb::PeerRow, Vec<String>)> = db::pool::with_pooled_or_open(|conn| {
        let peers = pdb::list_peers(conn)?;
        Ok(peers
            .into_iter()
            .filter(|p| p.departed_at.is_none() && p.peer_id != host && p.pubkey_fp.is_some())
            .map(|p| {
                let targets =
                    crate::mesh::dialer::dial_targets_for_peer(conn, &p.peer_id, &p.peer_addr)
                        .unwrap_or_else(|_| vec![p.peer_addr.clone()]);
                (p, targets)
            })
            .collect())
    })?;
    if plans.is_empty() {
        anyhow::bail!("no peers with a pinned bootstrap fp available to sign a refresh");
    }
    plans.sort_by_key(|(p, _)| (!p.local_secure, Reverse(p.last_seen_at)));
    let candidates: Vec<Candidate> = plans
        .iter()
        .flat_map(|(p, targets)| peer_candidates(p, targets, policy.canonical_port))
        .collect();

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

    let fetch = |c: Candidate| {
        let params = params.clone();
        async move {
            let v = crate::mesh::cli::dial_bootstrap_pub(
                &c.addr,
                c.port,
                c.pinned_fp.as_deref().unwrap_or_default(),
                "mesh/refresh-cert-bootstrap",
                params,
            )
            .await?;
            leaf_pair(&v)
        }
    };
    let install = |client: &str, server: &str| {
        utils::pki::install_refreshed_peer_certs(
            &pki_d,
            &host,
            client,
            &key_client,
            server,
            &key_server,
        )
    };
    let used = first_installed(candidates, policy.exchange_timeout, fetch, install)
        .await
        .context("all candidate peers refused bootstrap refresh")?;
    info!("[cert-rotation] refreshed leaf certs over bootstrap via {used}");
    Ok(())
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
    leaf_pair(&resp.result.context("empty refresh result")?)
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
            let err = refresh_via_peer_mtls(test_policy()).await.unwrap_err();
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
            // be a candidate. Reaching the candidate loop (not the "no peers"
            // bail) proves candidacy only; no dial happens, since this host
            // has no mesh client bundle to dial with.
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
            let err = refresh_via_peer_mtls(test_policy()).await.unwrap_err();
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
            let err = refresh_via_peer_mtls(test_policy()).await.unwrap_err();
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
            let err = refresh_via_peer_bootstrap(test_policy()).await.unwrap_err();
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
            let err = format!("{:#}", refresh_via_peer(test_policy()).await.unwrap_err());
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
            let err = refresh_via_peer_mtls(test_policy()).await.unwrap_err();
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
            let err = refresh_via_peer_bootstrap(test_policy()).await.unwrap_err();
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
            let err = refresh_via_peer(test_policy()).await.unwrap_err();
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
            let err = format!("{:#}", refresh_via_peer(test_policy()).await.unwrap_err());
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
            let err = refresh_via_peer_bootstrap(test_policy()).await.unwrap_err();
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

    /// Port 1 is never listening, and pinning the canonical port to it keeps
    /// tests from dialing a real daemon on the default mesh port.
    fn test_policy() -> DialPolicy {
        DialPolicy {
            canonical_port: 1,
            exchange_timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn renewal_title_reports_expiry_instead_of_negative_days() {
        assert_eq!(
            renewal_title("h", 5),
            "Mesh cert renewal failing on h: 5 days left"
        );
        assert_eq!(
            renewal_title("h", 0),
            "Mesh cert renewal failing on h: 0 days left"
        );
        assert_eq!(
            renewal_title("h", -2),
            "Mesh cert renewal failing on h: expired 2 days ago"
        );
    }

    fn peer_row(port: u16) -> pdb::PeerRow {
        pdb::PeerRow {
            peer_id: utils::id::new(),
            peer_hostname: "p".to_string(),
            peer_addr: "10.0.0.1".to_string(),
            peer_port: port,
            pubkey_fp: Some("fp".to_string()),
            first_seen_at: 0,
            last_seen_at: 0,
            departed_at: None,
            local_secure: false,
            peer_secure: false,
        }
    }

    fn pairs(cs: &[Candidate]) -> Vec<(&str, u16)> {
        cs.iter().map(|c| (c.addr.as_str(), c.port)).collect()
    }

    #[test]
    fn peer_candidates_try_stored_then_canonical_port_deduped() {
        let p = peer_row(40123);
        let targets = vec![
            "10.0.0.1".to_string(),
            "box.example".to_string(),
            "10.0.0.1".to_string(),
        ];
        assert_eq!(
            pairs(&peer_candidates(&p, &targets, 12002)),
            vec![
                ("10.0.0.1", 40123),
                ("10.0.0.1", 12002),
                ("box.example", 40123),
                ("box.example", 12002),
            ]
        );
        let p = peer_row(12002);
        assert_eq!(
            pairs(&peer_candidates(&p, &targets, 12002)),
            vec![("10.0.0.1", 12002), ("box.example", 12002)]
        );
        assert!(
            peer_candidates(&p, &targets, 12002)
                .iter()
                .all(|c| c.pinned_fp.as_deref() == Some("fp"))
        );
    }

    fn candidate(peer_id: &str) -> Candidate {
        Candidate {
            peer_id: peer_id.to_string(),
            addr: "127.0.0.1".to_string(),
            port: 1,
            pinned_fp: None,
        }
    }

    #[test]
    fn bad_returned_leaf_is_rejected_and_next_candidate_tried() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        utils::pki::init_mesh_ca(pki, TEST_CN).unwrap();
        let (csr_c, key_c, csr_s, key_s) = utils::pki::build_refresh_csrs(TEST_CN).unwrap();
        let sign =
            |csr: &str, cn: &str, role| utils::pki::sign_peer_csr(pki, csr, cn, role).unwrap().0;
        let good = (
            sign(&csr_c, TEST_CN, utils::pki::PeerRole::Client),
            sign(&csr_s, TEST_CN, utils::pki::PeerRole::Server),
        );
        // A CA-signed leaf naming another host: chains fine, wrong identity.
        let bad = (
            sign(&csr_c, "someone-else", utils::pki::PeerRole::Client),
            good.1.clone(),
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let used = rt
            .block_on(first_installed(
                vec![candidate("bad"), candidate("good")],
                Duration::from_secs(5),
                |c: Candidate| {
                    let r = if c.peer_id == "bad" {
                        bad.clone()
                    } else {
                        good.clone()
                    };
                    async move { Ok(r) }
                },
                |client: &str, server: &str| {
                    utils::pki::install_refreshed_peer_certs(
                        pki, TEST_CN, client, &key_c, server, &key_s,
                    )
                },
            ))
            .unwrap();
        assert_eq!(used.peer_id, "good");
        assert_eq!(
            std::fs::read_to_string(utils::pki::mesh_client_cert_path(pki)).unwrap(),
            good.0
        );
    }

    #[test]
    fn stalled_peer_times_out_instead_of_wedging_refresh() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            crate::host_identity::init(dir.path()).unwrap();
            utils::pki::init_mesh_ca(&pki_dir(), TEST_CN).unwrap();
            // Never accepted: the kernel completes the TCP handshake from the
            // backlog, so the TLS ClientHello is simply never answered.
            let stall = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = stall.local_addr().unwrap().port();
            let conn = db::open_default().unwrap();
            pdb::upsert_peer(
                &conn,
                &utils::id::new(),
                "stall",
                "127.0.0.1",
                port,
                Some("fp"),
                "",
            )
            .unwrap();
            drop(conn);
            let policy = DialPolicy {
                canonical_port: port,
                exchange_timeout: Duration::from_millis(300),
            };
            let t0 = std::time::Instant::now();
            let err = format!("{:#}", refresh_via_peer_mtls(policy).await.unwrap_err());
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "took {:?}",
                t0.elapsed()
            );
            assert!(err.contains("timed out"), "got: {err}");
            drop(stall);
        });
    }

    #[test]
    fn mtls_refused_then_bootstrap_signs_and_installs() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            crate::host_identity::init(dir.path()).unwrap();
            let own = crate::host_identity::machine_id().to_string();
            let pki = pki_dir();
            utils::pki::init_mesh_ca(&pki, &own).unwrap();
            // A still-valid client leaf from a foreign CA: refresh takes the mTLS
            // path, and the signer's handshake rejects it.
            let rogue = tempfile::tempdir().unwrap();
            utils::pki::init_mesh_ca(rogue.path(), &own).unwrap();
            for path in [
                utils::pki::mesh_client_cert_path,
                utils::pki::mesh_client_key_path,
            ] {
                let pem = std::fs::read_to_string(path(rogue.path())).unwrap();
                utils::pki::atomic_write_pem(&path(&pki), &pem).unwrap();
            }
            let rogue_client =
                std::fs::read_to_string(utils::pki::mesh_client_cert_path(&pki)).unwrap();

            // This process is both the renewing host and the signer, so the
            // signer's view of us (own row, our bootstrap fp) and the candidate
            // row we dial share one DB and one bootstrap key.
            let key = utils::pki::load_or_init_bootstrap_key(&pki).unwrap();
            let fp = utils::pki::bootstrap_pubkey_fingerprint(&key.verifying_key());
            let acceptor = crate::mesh::mesh_listener::build_acceptor(&pki).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let conn = db::open_default().unwrap();
            pdb::upsert_peer(
                &conn,
                &utils::id::new(),
                "signer",
                "127.0.0.1",
                port,
                Some(&fp),
                "",
            )
            .unwrap();
            pdb::upsert_peer(&conn, &own, "me", "127.0.0.1", port, Some(&fp), "").unwrap();
            // The signer resolves the envelope's fp to the most-recently-seen row.
            conn.execute(
                "UPDATE mesh_peers SET last_seen_at = last_seen_at + 100 WHERE peer_id = ?",
                [&own],
            )
            .unwrap();
            drop(conn);

            let server = async {
                loop {
                    let (tcp, peer) = listener.accept().await.unwrap();
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        continue;
                    };
                    let sni = tls
                        .get_ref()
                        .1
                        .server_name()
                        .unwrap_or_default()
                        .to_string();
                    if utils::mesh_compat::accepts_bootstrap_sni(&sni) {
                        crate::mesh::handle_mesh_bootstrap_connection(tls, peer)
                            .await
                            .unwrap();
                        return;
                    }
                }
            };
            let policy = DialPolicy {
                canonical_port: port,
                exchange_timeout: Duration::from_secs(10),
            };
            let (refreshed, ()) = tokio::time::timeout(Duration::from_secs(30), async {
                tokio::join!(refresh_via_peer(policy), server)
            })
            .await
            .expect("refresh exchange finished");
            refreshed.unwrap();

            let client = std::fs::read_to_string(utils::pki::mesh_client_cert_path(&pki)).unwrap();
            assert_ne!(client, rogue_client);
            assert_eq!(utils::pki::cert_summary(&client).unwrap().cn, own);
            let server = std::fs::read_to_string(utils::pki::mesh_server_cert_path(&pki)).unwrap();
            assert_eq!(
                utils::pki::cert_summary(&server).unwrap().cn,
                "orca-mesh-server"
            );
        });
    }

    fn raise_stale_notification() {
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
    }

    #[test]
    fn tick_dismisses_stale_notification_when_certs_are_fresh() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            crate::host_identity::init(dir.path()).unwrap();
            apply_schema();
            utils::pki::init_mesh_ca(&pki_dir(), TEST_CN).unwrap();
            raise_stale_notification();

            rt_now_tick().await.unwrap();

            let n = notifications::dismissable::get(NOTIFY_KEY)
                .unwrap()
                .unwrap();
            assert_eq!(n.state, notifications::dismissable::State::Dismissed);
        });
    }

    #[test]
    fn tick_leaves_suppressed_notification_suppressed() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            crate::host_identity::init(dir.path()).unwrap();
            apply_schema();
            utils::pki::init_mesh_ca(&pki_dir(), TEST_CN).unwrap();
            raise_stale_notification();
            notifications::dismissable::suppress(NOTIFY_KEY).unwrap();

            rt_now_tick().await.unwrap();

            let n = notifications::dismissable::get(NOTIFY_KEY)
                .unwrap()
                .unwrap();
            assert_eq!(n.state, notifications::dismissable::State::Suppressed);
        });
    }

    #[test]
    fn tick_raises_notification_when_a_leaf_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        with_home_db(dir.path(), async {
            crate::host_identity::init(dir.path()).unwrap();
            apply_schema();
            let pki = pki_dir();
            utils::pki::init_mesh_ca(&pki, TEST_CN).unwrap();
            std::fs::remove_file(utils::pki::mesh_ca_key_path(&pki)).unwrap();
            std::fs::remove_file(utils::pki::mesh_client_cert_path(&pki)).unwrap();

            assert!(rt_now_tick().await.is_err());

            let n = notifications::dismissable::get(NOTIFY_KEY)
                .unwrap()
                .expect("renewal notification raised");
            assert_eq!(n.state, notifications::dismissable::State::Active);
            assert!(
                n.body
                    .unwrap_or_default()
                    .contains("client notAfter: unparseable")
            );
        });
    }
}
