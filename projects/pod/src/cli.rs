// JSON-RPC envelopes are inherently opaque at the wire boundary; mirroring
// the allow in projects/sdk/rust/src/jsonrpc.rs.
#![allow(clippy::disallowed_types)]

//! CLI handlers for `orca pod {discover,pending,accept,connect,offer,list,
//! trust,self-secure,leave}`. Init lives in main.rs; ping lives in pod::ping.

use anyhow::{Context, Result, bail};
use db::ports::mesh_port;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use utils::framing::{read_frame, write_frame};
use utils::jsonrpc::{Message, Request, Response};
use utils::pki::PeerRole;

use crate::pki_dir;
use db::pod as pdb;

// ── pod discover ─────────────────────────────────────────────────────────────

pub fn cmd_pod_discover() -> Result<()> {
    let conn = db::open_default()?;
    let rows = pdb::list_discovery(&conn)?;
    if rows.is_empty() {
        println!("(no peers discovered yet — mDNS browse runs in the daemon; ensure it's up)");
        return Ok(());
    }
    println!(
        "{:<32} {:<16} {:<22} {:<10} can_invite",
        "pubkey_fp", "hostname", "addr:port", "state"
    );
    for r in rows {
        println!(
            "{:<32} {:<16} {:<22} {:<10} {}",
            r.pubkey_fp,
            r.hostname,
            format!("{}:{}", r.addr, r.port),
            r.state,
            r.can_invite
        );
    }
    Ok(())
}

// ── pod pending ──────────────────────────────────────────────────────────────

pub fn cmd_pod_pending() -> Result<()> {
    let conn = db::open_default()?;
    let rows = pdb::list_pending_offers(&conn, "in")?;
    if rows.is_empty() {
        println!(
            "(no pending offers — secure peers on the network will push offers automatically)"
        );
        return Ok(());
    }
    println!("Pending pod-membership offers:");
    for r in rows {
        let id = r.inviter_peer_id.as_deref().unwrap_or("?");
        let pod = r.pod_id.as_deref().unwrap_or("?");
        println!(
            "  • from {} ({id}, pod {pod}) at {}:{} (fp {})",
            r.peer_hostname, r.peer_addr, r.peer_port, r.peer_pubkey_fp
        );
        println!(
            "    expires in {}s — run `orca pod accept <code>` once you have the 6-char pairing code from the inviter",
            (r.expires_at - now_secs()).max(0)
        );
    }
    Ok(())
}

// ── pod accept ───────────────────────────────────────────────────────────────

/// Result of looking up an inbound pending offer by pairing code. Lets the
/// CLI tell the user *why* an accept failed instead of dumping the same
/// "no offer matches that code" line for every failure mode (the symptom
/// flagged in `project_pod_join_ux.md`).
#[derive(Debug)]
pub enum AcceptLookup {
    /// Live offer, ready to dial. Boxed to keep the enum lean — `PendingOffer`
    /// is ~240 bytes while the other variants are tiny.
    Active(Box<pdb::PendingOffer>),
    /// Code matched but the offer's TTL elapsed; `expired_secs_ago` is the
    /// gap from the offer's `expires_at` to `now`.
    Expired { expired_secs_ago: i64 },
    /// No row whose `code_hash` matches the typed code.
    NotFound,
}

pub fn classify_accept_lookup(maybe_offer: Option<pdb::PendingOffer>, now: i64) -> AcceptLookup {
    match maybe_offer {
        None => AcceptLookup::NotFound,
        Some(o) if o.expires_at >= now => AcceptLookup::Active(Box::new(o)),
        Some(o) => AcceptLookup::Expired {
            expired_secs_ago: now - o.expires_at,
        },
    }
}

pub async fn cmd_pod_accept(code: &str) -> Result<()> {
    let conn = db::open_default()?;
    let maybe = pdb::find_pending_offer_by_code_any_expiry(&conn, code)?;
    let offer = match classify_accept_lookup(maybe, now_secs()) {
        AcceptLookup::Active(o) => *o,
        AcceptLookup::NotFound => bail!(
            "pairing code not recognized — double-check the 6 chars the inviter showed (no matching offer on this host)"
        ),
        AcceptLookup::Expired { expired_secs_ago } => bail!(
            "this offer expired {expired_secs_ago}s ago — ask the inviter to push a fresh one (offer TTL is 600s)"
        ),
    };
    drop(conn);

    let pki_d = pki_dir();
    std::fs::create_dir_all(utils::pki::mesh_dir(&pki_d))?;
    let ca_pem = offer
        .mesh_ca_cert_pem
        .as_deref()
        .context("offer has no mesh CA cert")?;
    std::fs::write(utils::pki::mesh_ca_cert_path(&pki_d), ca_pem.as_bytes())?;

    // Cert CN = stable machine_id (not the display hostname, which on macOS
    // mutates on mDNS conflicts and would force a re-issue on every flap).
    // `peer_cn` is the CN material that goes into the CSR + wire
    // `joiner_hostname`; `display_name` is the human label that lands in
    // `pod_peers.peer_hostname` on the inviter side via the new
    // `joiner_display_name` wire field.
    let peer_cn = system::host_identity::machine_id().to_string();
    let display_name = system::host_identity::display_hostname().to_string();
    let (csr_client_pem, client_key_pem) = utils::pki::build_peer_csr(&peer_cn, PeerRole::Client)?;
    let (csr_server_pem, server_key_pem) = utils::pki::build_peer_csr(&peer_cn, PeerRole::Server)?;

    // Dial inviter's bootstrap SNI, pinned to the fp stored on the offer row.
    let signing = utils::pki::load_or_init_bootstrap_key(&pki_d)?;
    #[derive(serde::Serialize)]
    struct ConfirmBody<'a> {
        code: &'a str,
        joiner_hostname: &'a str,
        csr_client_pem: &'a str,
        csr_server_pem: &'a str,
        joiner_display_name: &'a str,
    }
    let body = ConfirmBody {
        code,
        joiner_hostname: &peer_cn,
        csr_client_pem: &csr_client_pem,
        csr_server_pem: &csr_server_pem,
        joiner_display_name: &display_name,
    };
    let env = utils::pki::sign_envelope(&signing, &body)?;

    // Try each candidate address the inviter advertised (falling back to the
    // single stored addr for pre-candidate-addr offers), pinned to the
    // inviter's bootstrap fp. The pin is the security anchor: a candidate that
    // points at the wrong host fails the handshake and we move on, so re-pair
    // succeeds as long as ANY advertised address reaches the real inviter —
    // robust to the offer-push source IP being a tunnel address.
    let params = serde_json::to_value(&env)?;
    let candidates: Vec<String> = if offer.candidate_addrs.is_empty() {
        vec![offer.peer_addr.clone()]
    } else {
        offer.candidate_addrs.clone()
    };
    let mut resp_value = None;
    let mut dialed_addr = offer.peer_addr.clone();
    let mut last_err: Option<anyhow::Error> = None;
    for addr in &candidates {
        match dial_bootstrap(
            addr,
            offer.peer_port,
            &offer.peer_pubkey_fp,
            "pod/join-confirm",
            params.clone(),
        )
        .await
        {
            Ok(v) => {
                dialed_addr = addr.clone();
                resp_value = Some(v);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let resp_value = resp_value.ok_or_else(|| {
        last_err
            .unwrap_or_else(|| anyhow::anyhow!("no candidate addresses to dial"))
            .context(format!(
                "pod/join-confirm over bootstrap channel failed (tried {} address(es): {})",
                candidates.len(),
                candidates.join(", ")
            ))
    })?;

    #[derive(serde::Deserialize)]
    struct Result_ {
        client_cert_pem: String,
        server_cert_pem: String,
        ca_cert_pem: String,
        inviter_peer_id: String,
        pod_id: String,
    }
    let r: Result_ = serde_json::from_value(resp_value)?;

    // Persist signed certs alongside the locally-generated keys.
    let server_dir = utils::pki::mesh_dir(&pki_d).join("server");
    let client_dir = utils::pki::mesh_dir(&pki_d).join("client");
    std::fs::create_dir_all(&server_dir)?;
    std::fs::create_dir_all(&client_dir)?;
    std::fs::write(
        utils::pki::mesh_server_cert_path(&pki_d),
        &r.server_cert_pem,
    )?;
    std::fs::write(utils::pki::mesh_server_key_path(&pki_d), &server_key_pem)?;
    std::fs::write(
        utils::pki::mesh_client_cert_path(&pki_d),
        &r.client_cert_pem,
    )?;
    std::fs::write(utils::pki::mesh_client_key_path(&pki_d), &client_key_pem)?;

    let conn = db::open_default()?;
    pdb::set_self_secure(&conn, false)?;
    pdb::set_pod_id(&conn, &r.pod_id)?;
    pdb::upsert_peer(
        &conn,
        &r.inviter_peer_id,
        &offer.peer_hostname,
        &dialed_addr,
        offer.peer_port,
        Some(&offer.peer_pubkey_fp),
        &r.ca_cert_pem,
    )?;
    // Accepting an offer IS the local trust signal — without flipping this
    // the new pairing stays untrusted forever and roster-sync can't see it
    // as a usable source.
    pdb::set_trust(&conn, &r.inviter_peer_id, Some(true), None)?;
    // Same legacy `"unknown"` stub cleanup as the inviter side: drop the
    // pre-rc.25 row at this peer's addr so the host_status puller stops
    // chasing a peer_id that means nothing.
    pdb::cleanup_unknown_stub_at(&conn, &offer.peer_addr)?;
    pdb::delete_pending_offer(&conn, &offer.offer_id)?;
    drop(conn);

    // Notify the inviter so THEIR peer_secure for us flips to true. Without
    // this the inviter sees mutual=false forever after auto-accept — the
    // initial DB write is local-only and roster-sync only carries identity,
    // not trust state.
    if let Err(e) = call_pod_method_pub(
        &offer.peer_addr,
        offer.peer_port,
        "pod/notify-trust",
        serde_json::json!({ "trust": true }),
    )
    .await
    {
        tracing::warn!(
            "pod/notify-trust to {}:{} after accept failed: {e:#} — inviter will see peer_secure=false until next manual `orca system peer update <peer> true`",
            offer.peer_addr,
            offer.peer_port
        );
    }

    println!(
        "✓ joined pod {} via {} ({}, {}:{})",
        r.pod_id, offer.peer_hostname, r.inviter_peer_id, offer.peer_addr, offer.peer_port
    );
    println!(
        "  self_secure is OFF — run `orca pod self-secure on` to enable secrets writes on this host."
    );
    Ok(())
}

// ── pod connect (manual fallback when mDNS is blocked) ───────────────────────

/// Joiner-initiated bootstrap (`pod connect` / `pod join`). Dials the inviter's
/// bootstrap SNI with a TOFU verifier, sends a signed `pod/request-offer`,
/// validates the inviter's pubkey echo against the captured TLS fp, and lands
/// the resulting offer as an inbound pending row so `pod accept <code>` can
/// finish the handshake.
pub async fn cmd_pod_connect(addr: &str) -> Result<()> {
    cmd_pod_join(addr).await
}

pub async fn cmd_pod_join(addr: &str) -> Result<()> {
    let (host, port) = utils::pki::parse_peer_addr(addr, mesh_port())?;

    let pki_d = pki_dir();
    let signing = utils::pki::load_or_init_bootstrap_key(&pki_d)?;
    let joiner_fp = utils::pki::bootstrap_pubkey_fingerprint(&signing.verifying_key());
    let joiner_peer_id = system::host_identity::machine_id().to_string();
    let joiner_hostname = system::host_identity::machine_id().to_string();
    let joiner_display_name = system::host_identity::display_hostname().to_string();

    #[derive(serde::Serialize)]
    struct RequestBody<'a> {
        joiner_peer_id: &'a str,
        joiner_hostname: &'a str,
        joiner_pubkey_fp: &'a str,
        joiner_display_name: &'a str,
    }
    let body = RequestBody {
        joiner_peer_id: &joiner_peer_id,
        joiner_hostname: &joiner_hostname,
        joiner_pubkey_fp: &joiner_fp,
        joiner_display_name: &joiner_display_name,
    };
    let env = utils::pki::sign_envelope(&signing, &body)?;

    // TOFU dial — capture the inviter's bootstrap fp; we'll cross-check it
    // against the signed echo below before persisting anything.
    let captured: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let verifier = utils::pki::capturing_bootstrap_verifier(captured.clone());
    let client_config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));
    let target = format!("{host}:{port}");
    let tcp = TcpStream::connect(&target)
        .await
        .with_context(|| format!("connect {target}"))?;
    let sni = ServerName::try_from(utils::pki::POD_BOOTSTRAP_SAN)?.to_owned();
    let mut tls = connector
        .connect(sni, tcp)
        .await
        .context("bootstrap TLS handshake (is the inviter's daemon running?)")?;

    write_frame(
        &mut tls,
        &serde_json::to_vec(&Request::new(
            1,
            "pod/request-offer",
            Some(serde_json::to_value(&env)?),
        ))?,
    )
    .await?;
    let raw = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut tls))
        .await
        .context("pod/request-offer timed out (inviter unreachable or wrong port?)")??;
    let resp_value = parse_resp(&raw)?;

    #[derive(serde::Deserialize)]
    struct Resp {
        inviter_pubkey_fp: String,
        inviter_peer_id: String,
        inviter_hostname: String,
        inviter_port: u16,
        mesh_ca_cert_pem: String,
        pod_id: String,
        code_hash: String,
        expires_at: i64,
        #[serde(default)]
        inviter_display_name: Option<String>,
        #[serde(default)]
        code_hint: Option<String>,
        #[serde(default)]
        code_plain: Option<String>,
        #[serde(default)]
        inviter_addrs: Vec<String>,
    }
    let r: Resp = serde_json::from_value(resp_value)?;

    // The signed response says "I'm fp X". The TOFU TLS layer captured "this
    // cert has fp Y". X must equal Y, otherwise we're talking to a MITM that
    // presented its own cert and forwarded the envelope along.
    let observed_fp = captured
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .context("TLS verifier did not capture a server cert fp")?;
    if observed_fp != r.inviter_pubkey_fp {
        bail!(
            "TOFU fp mismatch: TLS cert fp {observed_fp} != signed response fp {} (possible MITM)",
            r.inviter_pubkey_fp
        );
    }

    let label = r
        .inviter_display_name
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| r.inviter_hostname.clone());

    let conn = db::open_default()?;
    let offer_id = utils::id::new();
    let ttl = r.expires_at - now_secs();
    if ttl <= 0 {
        bail!("inviter returned an already-expired offer (clock skew between hosts?)");
    }
    // Candidates for join-confirm: the inviter's self-advertised addresses,
    // then `host` — the address we just dialed to reach it (by definition
    // reachable). Deduped, order-preserving; each is tried pinned to the fp.
    let mut candidate_addrs: Vec<String> = Vec::new();
    for a in r
        .inviter_addrs
        .iter()
        .map(String::as_str)
        .chain([host.as_str()])
    {
        let a = a.trim();
        if !a.is_empty() && !candidate_addrs.iter().any(|c| c == a) {
            candidate_addrs.push(a.to_string());
        }
    }
    pdb::insert_pending_offer(
        &conn,
        &offer_id,
        "in",
        &r.inviter_pubkey_fp,
        &label,
        &host,
        r.inviter_port,
        &r.code_hash,
        Some(&r.mesh_ca_cert_pem),
        Some(&r.inviter_peer_id),
        Some(&r.pod_id),
        ttl,
        r.code_plain.as_deref(),
        &candidate_addrs,
    )?;

    println!(
        "✓ requested offer from {label} ({}, fp {})",
        r.inviter_peer_id, r.inviter_pubkey_fp
    );

    // S1 auto-accept: the inviter embeds `code_plain` in the request-offer
    // response on joiner-initiated handshakes. Security unchanged — the
    // TOFU pubkey pin + signed envelope already authenticated this exchange,
    // and the code's only purpose was operator transcription. Skip straight
    // to `pod accept <code>` so one command finishes the join.
    if let Some(code) = r.code_plain.as_deref() {
        println!("  auto-accepting via offer-embedded code…");
        return cmd_pod_accept(code).await;
    }

    if let Some(hint) = &r.code_hint {
        println!("  inviter will print a 6-char code starting with: {hint}");
    }
    println!(
        "  run `orca pod accept <code>` within {}s, using the code from the inviter's CLI",
        ttl
    );
    Ok(())
}

// ── pod offer (manual: push to a specific address) ───────────────────────────

/// Outcome of resolving a user-typed `host[:port]` to a known discovery row.
/// Manual offers need the joiner's pubkey fp for the pinned bootstrap dial;
/// we read it from the mDNS-populated discovery table rather than asking the
/// user to copy-paste a fp.
#[derive(Debug)]
pub enum OfferTargetResolution {
    Match(Box<pdb::DiscoveryRow>),
    NoMatch,
    /// Multiple discovery rows share the host (different ports / multi-homed).
    /// Caller prints the candidates so the user can re-run with `:port`.
    Ambiguous(Vec<pdb::DiscoveryRow>),
}

/// Pure resolver: given the typed `host[:port]` (already parsed via
/// `utils::pki::parse_peer_addr`) and the current discovery rows, find the
/// single row that matches by `addr` or `hostname`. Port narrows when
/// the user supplied an explicit one — without an explicit port the
/// `default_port_used` flag tells us to ignore port mismatches.
pub fn resolve_offer_target(
    rows: &[pdb::DiscoveryRow],
    host: &str,
    port: u16,
    default_port_used: bool,
) -> OfferTargetResolution {
    let host_matches = |r: &pdb::DiscoveryRow| r.addr == host || r.hostname == host;
    let port_matches = |r: &pdb::DiscoveryRow| default_port_used || r.port == port;
    let hits: Vec<pdb::DiscoveryRow> = rows
        .iter()
        .filter(|r| host_matches(r) && port_matches(r))
        .cloned()
        .collect();
    match hits.len() {
        0 => OfferTargetResolution::NoMatch,
        1 => OfferTargetResolution::Match(Box::new(hits.into_iter().next().unwrap())),
        _ => OfferTargetResolution::Ambiguous(hits),
    }
}

/// Pure I/O side of pairing-as-inviter: discovery lookup → mint code → insert
/// outbound offer → push to joiner. Returns the resolved target + code so
/// callers (`cmd_pod_offer`, `cmd_pod_pair`) can choose to print and exit or
/// poll for completion.
pub async fn push_pairing_offer(addr: &str) -> Result<(pdb::DiscoveryRow, String)> {
    let default_port_used = !addr.contains(':');
    let (host, port) = utils::pki::parse_peer_addr(addr, mesh_port())?;

    let conn = db::open_default()?;
    if utils::pki::load_mesh_client(&pki_dir()).is_err() {
        bail!(
            "this host is not a pod member yet — run `orca pod init` (or accept an offer) before inviting peers"
        );
    }
    let discovery = pdb::list_discovery(&conn)?;
    let target = match resolve_offer_target(&discovery, &host, port, default_port_used) {
        OfferTargetResolution::Match(t) => *t,
        OfferTargetResolution::NoMatch => bail!(
            "no orca discovered at {host}{} — wait for `orca pod discover` to see it on mDNS, \
             then retry (cross-subnet pairing is on the roadmap)",
            if default_port_used {
                String::new()
            } else {
                format!(":{port}")
            }
        ),
        OfferTargetResolution::Ambiguous(hits) => {
            let lines: Vec<String> = hits
                .iter()
                .map(|h| format!("  • {}:{} (fp {})", h.addr, h.port, h.pubkey_fp))
                .collect();
            bail!(
                "multiple discovered orcas match {host}; specify a port:\n{}",
                lines.join("\n")
            );
        }
    };

    let pod_id = pdb::get_pod_id(&conn)?.unwrap_or_else(|| "default".to_string());
    if pdb::has_open_outbound_offer(&conn, &target.pubkey_fp)? {
        bail!(
            "an outbound offer to {} is already pending — wait for it to expire (~10 min) or \
             have the joiner accept it first",
            target.hostname
        );
    }

    let code = crate::scheduler::mint_pairing_code();
    let code_hash = pdb::hash_code(&code);
    let offer_id = utils::id::new();
    pdb::insert_pending_offer(
        &conn,
        &offer_id,
        "out",
        &target.pubkey_fp,
        &target.hostname,
        &target.addr,
        target.port,
        &code_hash,
        None,
        None,
        None,
        crate::scheduler::OFFER_TTL_SECS,
        None,
        &[], // outbound offer: the joiner dials us, not the reverse
    )?;
    drop(conn);

    crate::scheduler::push_offer(
        &target.hostname,
        &target.addr,
        target.port,
        &target.pubkey_fp,
        &code,
        &pod_id,
    )
    .await
    .context("push offer to joiner failed")?;

    Ok((target, code))
}

pub async fn cmd_pod_offer(addr: &str) -> Result<()> {
    let (target, code) = push_pairing_offer(addr).await?;
    println!(
        "✓ offered pod membership to {} ({}:{})",
        target.hostname, target.addr, target.port
    );
    println!("  pairing code: {code}");
    println!(
        "  the joiner has {}s to run `orca pod accept {code}`",
        crate::scheduler::OFFER_TTL_SECS
    );
    Ok(())
}

/// One-shot pairing flow for the inviter: push the offer, print the code, then
/// poll `pod_peers` for the joiner's row to appear (which only happens after
/// the joiner runs `orca pod accept <code>` or the orca web UI accepts it).
/// Exits as soon as the peer lands or the offer's TTL elapses.
pub async fn cmd_pod_pair(addr: &str) -> Result<()> {
    let (target, code) = push_pairing_offer(addr).await?;
    println!(
        "✓ offered pod membership to {} ({}:{})",
        target.hostname, target.addr, target.port
    );
    println!("  pairing code: {code}");
    println!(
        "  waiting up to {}s for the joiner to accept (`orca pod accept {code}` on the other host, \
         or paste the code into the Orca UI)…",
        crate::scheduler::OFFER_TTL_SECS
    );

    let deadline =
        std::time::Instant::now() + Duration::from_secs(crate::scheduler::OFFER_TTL_SECS as u64);
    loop {
        if std::time::Instant::now() >= deadline {
            bail!(
                "timed out after {}s waiting for {} to accept; the code is still valid until expiry — \
                 retry `orca pod pair {}` once the joiner is ready, or run `orca pod accept {}` \
                 directly on the joiner.",
                crate::scheduler::OFFER_TTL_SECS,
                target.hostname,
                addr,
                code,
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        let conn = db::open_default()?;
        let peers = pdb::list_peers(&conn)?;
        drop(conn);
        if let Some(p) = peers
            .iter()
            .find(|p| p.pubkey_fp.as_deref() == Some(target.pubkey_fp.as_str()))
        {
            println!(
                "✓ paired with {} ({}, {}:{})",
                p.peer_hostname, p.peer_id, p.peer_addr, p.peer_port
            );
            return Ok(());
        }
    }
}

// ── pod list ─────────────────────────────────────────────────────────────────

pub fn cmd_pod_list() -> Result<()> {
    let conn = db::open_default()?;
    let peers = pdb::list_peers(&conn)?;
    if peers.is_empty() {
        println!("(no pod peers — run `orca pod discover` to see what's on the LAN)");
        return Ok(());
    }
    println!(
        "{:<28} {:<16} {:<22} {:<8} {:<8} status",
        "peer_id", "hostname", "addr:port", "local", "peer"
    );
    for p in peers {
        let status = if p.departed_at.is_some() {
            "DEPARTED"
        } else {
            "active"
        };
        println!(
            "{:<28} {:<16} {:<22} {:<8} {:<8} {}",
            p.peer_id,
            p.peer_hostname,
            format!("{}:{}", p.peer_addr, p.peer_port),
            p.local_secure,
            p.peer_secure,
            status
        );
    }
    Ok(())
}

// ── pod trust ────────────────────────────────────────────────────────────────

pub async fn cmd_pod_trust(peer_id: &str, on: bool) -> Result<()> {
    let conn = db::open_default()?;
    let peer = pdb::list_peers(&conn)?
        .into_iter()
        .find(|p| p.peer_id == peer_id)
        .with_context(|| format!("no such peer: {peer_id}"))?;
    let new = pdb::set_trust(&conn, peer_id, Some(on), None)?;
    println!(
        "✓ local trust for {peer_id} → {on} (peer side: {})",
        new.peer_secure
    );

    match call_pod_method(
        &peer.peer_addr,
        peer.peer_port,
        "pod/notify-trust",
        serde_json::json!({ "trust": on }),
    )
    .await
    {
        Ok(_) => println!("✓ notified {peer_id}"),
        Err(e) => println!("  warning: notify-trust dial failed ({e}); peer will pick it up later"),
    }

    if pdb::is_mutual_secure(new) {
        println!("→ mutual secure; replicating CA key if needed…");
        if let Err(e) = replicate_ca_key_if_needed(&peer).await {
            println!("  warning: CA-key replication: {e}");
        }
    }
    Ok(())
}

async fn replicate_ca_key_if_needed(peer: &pdb::PeerRow) -> Result<()> {
    let pki_d = pki_dir();
    let i_have_key = utils::pki::has_mesh_ca_key(&pki_d);
    let resp = call_pod_method(
        &peer.peer_addr,
        peer.peer_port,
        "pod/has-ca-key",
        serde_json::json!({}),
    )
    .await?;
    let peer_has_key = resp
        .get("has_key")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if i_have_key && !peer_has_key {
        let (cert_pem, key_pem) = utils::pki::export_mesh_ca_keypair(&pki_d)?;
        call_pod_method(
            &peer.peer_addr,
            peer.peer_port,
            "pod/push-ca-key",
            serde_json::json!({ "cert_pem": cert_pem, "key_pem": key_pem }),
        )
        .await?;
        println!("✓ pushed CA key to {}", peer.peer_id);
    } else if !i_have_key && peer_has_key {
        println!("  peer has CA key; we don't — they should push to us on their side");
    }
    Ok(())
}

// ── pod self-secure ──────────────────────────────────────────────────────────

pub fn cmd_pod_self_secure(action: SelfSecureAction) -> Result<()> {
    let conn = db::open_default()?;
    match action {
        SelfSecureAction::Show => {
            let v = db::pod::get_self_secure(&conn)?;
            println!("self_secure: {v}");
        }
        SelfSecureAction::On => {
            pdb::set_self_secure(&conn, true)?;
            println!("✓ self_secure: true (secrets writes enabled)");
        }
        SelfSecureAction::Off => {
            pdb::set_self_secure(&conn, false)?;
            println!("✓ self_secure: false (secrets writes will be refused)");
        }
    }
    Ok(())
}

pub enum SelfSecureAction {
    On,
    Off,
    Show,
}

// ── pod cert-status ──────────────────────────────────────────────────────────

pub fn cmd_pod_cert_status() -> Result<()> {
    let pki_d = pki_dir();
    if !utils::pki::mesh_ca_cert_path(&pki_d).exists() {
        println!("(not a pod member — no mesh certs to report)");
        return Ok(());
    }
    let ca_pem = std::fs::read_to_string(utils::pki::mesh_ca_cert_path(&pki_d))?;
    let server_pem = std::fs::read_to_string(utils::pki::mesh_server_cert_path(&pki_d)).ok();
    let client_pem = std::fs::read_to_string(utils::pki::mesh_client_cert_path(&pki_d)).ok();
    let bootstrap_pem = std::fs::read_to_string(utils::pki::bootstrap_cert_path(&pki_d)).ok();

    println!("{:<22} {:>14}  rotation", "cert", "days remaining");
    print_cert_row(
        "mesh CA",
        &ca_pem,
        utils::pki::PEER_REFRESH_THRESHOLD_DAYS * 6,
    );
    if let Some(p) = server_pem {
        print_cert_row("mesh server", &p, utils::pki::PEER_REFRESH_THRESHOLD_DAYS);
    }
    if let Some(p) = client_pem {
        print_cert_row("mesh client", &p, utils::pki::PEER_REFRESH_THRESHOLD_DAYS);
    }
    if let Some(p) = bootstrap_pem {
        // Bootstrap is long-lived; no auto-rotation. Show ample threshold.
        print_cert_row("bootstrap TLS", &p, 30);
    }
    println!(
        "\nLeaf certs auto-rotate when days remaining ≤ {} (daily check).",
        utils::pki::PEER_REFRESH_THRESHOLD_DAYS
    );
    Ok(())
}

fn print_cert_row(label: &str, pem: &str, threshold_days: i64) {
    match utils::pki::cert_days_remaining(pem) {
        Ok(days) => {
            let status = if days <= 0 {
                "EXPIRED"
            } else if days <= threshold_days {
                "due"
            } else {
                "ok"
            };
            println!("{label:<22} {days:>14}  {status}");
        }
        Err(e) => println!("{label:<22} {:>14}  parse-error: {e}", "?"),
    }
}

// ── pod ca-rotate ────────────────────────────────────────────────────────────

pub async fn cmd_pod_ca_rotate(overlap_days: i64) -> Result<()> {
    anyhow::ensure!(
        (1..=90).contains(&overlap_days),
        "overlap-days must be between 1 and 90"
    );
    let pki_d = pki_dir();
    anyhow::ensure!(
        utils::pki::has_mesh_ca_key(&pki_d),
        "this host does not have the mesh CA key — cannot rotate"
    );

    // Rotate: current → previous, generate fresh current.
    utils::pki::rotate_mesh_ca(&pki_d)?;
    let expires_at = now_secs() + overlap_days * 86_400;

    let conn = db::open_default()?;
    pdb::set_ca_previous_expires_at(&conn, Some(expires_at))?;

    // Reissue our own peer certs immediately under the new CA so we present
    // current-CA-signed material to peers as soon as possible.
    // CN is the stable machine_id (see pod accept).
    let host = system::host_identity::machine_id().to_string();
    utils::pki::reissue_mesh_server_cert(&pki_d)?;
    utils::pki::reissue_mesh_client_cert(&pki_d, &host)?;

    println!("✓ rotated mesh CA");
    println!(
        "  previous CA stays trusted until {} ({}d overlap)",
        utils::time::Timestamp::from_unix_seconds(expires_at)
            .map(|d| d.to_rfc3339())
            .unwrap_or_else(|| expires_at.to_string()),
        overlap_days
    );

    // Replicate to every mutual-secure peer that has the CA key.
    let cur_cert = std::fs::read_to_string(utils::pki::mesh_ca_cert_path(&pki_d))?;
    let cur_key = std::fs::read_to_string(utils::pki::mesh_ca_key_path(&pki_d))?;
    let prev_cert = std::fs::read_to_string(utils::pki::mesh_ca_previous_cert_path(&pki_d))?;
    let prev_key = std::fs::read_to_string(utils::pki::mesh_ca_previous_key_path(&pki_d))?;
    let peers = pdb::list_peers(&conn)?;
    for p in peers {
        if p.departed_at.is_some() || !p.local_secure || !p.peer_secure {
            continue;
        }
        let params = serde_json::json!({
            "current_cert_pem": cur_cert,
            "current_key_pem": cur_key,
            "previous_cert_pem": prev_cert,
            "previous_key_pem": prev_key,
            "previous_expires_at": expires_at,
        });
        match call_pod_method(&p.peer_addr, p.peer_port, "pod/push-ca-state", params).await {
            Ok(_) => println!("  ✓ replicated CA state to {}", p.peer_id),
            Err(e) => println!("  ! could not replicate to {} ({e})", p.peer_id),
        }
    }
    println!("Peer leaf certs auto-refresh on their next rotation tick (≤7d threshold).");
    Ok(())
}

// ── pod leave ────────────────────────────────────────────────────────────────

pub async fn cmd_pod_leave(wipe_secrets: bool, wipe_all: bool) -> Result<()> {
    let conn = db::open_default()?;
    let peers = pdb::list_peers(&conn)?;

    // Best-effort: notify peers we're leaving. Fire-and-forget per peer.
    for p in &peers {
        if p.departed_at.is_some() {
            continue;
        }
        if let Err(e) = call_pod_method(
            &p.peer_addr,
            p.peer_port,
            "pod/peer-leaving",
            serde_json::json!({}),
        )
        .await
        {
            println!("  warning: could not notify {} ({e})", p.peer_id);
        }
    }

    // Local wipe of pod membership state.
    pdb::wipe_pod_membership(&conn)?;

    // Optional secret/data wipes.
    if wipe_secrets || wipe_all {
        conn.execute("DELETE FROM secrets", [])?;
        println!("✓ wiped secrets table");
    }
    if wipe_all {
        for tbl in [
            "plugin_data",
            "plugin_credentials",
            "oauth_tokens",
            "profile_credentials",
        ] {
            _ = conn.execute(&format!("DELETE FROM {tbl}"), []);
        }
        println!("✓ wiped plugin_data, plugin_credentials, oauth_tokens, profile_credentials");
    }

    // Remove mesh PKI material. Bootstrap key stays (host identity persists
    // across pod re-joins).
    let pki_d = pki_dir();
    let mesh = utils::pki::mesh_dir(&pki_d);
    if mesh.exists() {
        _ = std::fs::remove_dir_all(&mesh);
        println!("✓ removed mesh PKI material at {}", mesh.display());
    }

    println!("✓ left the pod. This host can re-join via auto-discovery.");
    Ok(())
}

// ── helpers ──────────────────────────────────────────────────────────────────

use utils::time::now_secs_since_epoch as now_secs;

/// Dial a paired peer with our mesh client cert. Used by post-join methods
/// (notify-trust, has-ca-key, push-ca-key, peer-leaving).
pub async fn call_pod_method_pub(
    host: &str,
    port: u16,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    call_pod_method(host, port, method, params).await
}

pub async fn dial_bootstrap_pub(
    host: &str,
    port: u16,
    pinned_fp: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    dial_bootstrap(host, port, pinned_fp, method, params).await
}

pub async fn replicate_ca_key_if_needed_pub(peer: &pdb::PeerRow) -> Result<()> {
    replicate_ca_key_if_needed(peer).await
}

async fn call_pod_method(
    host: &str,
    port: u16,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let pki_d = pki_dir();
    let bundle = utils::pki::load_mesh_client(&pki_d)
        .context("load mesh client bundle (this host is not a pod member)")?;
    let (chain, key) = utils::pki::parse_cert_and_key(&bundle.cert_pem, &bundle.key_pem)?;
    let roots = utils::pki::ca_root_store(&bundle.ca_cert_pem)?;
    let client_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key)?;
    dial_pod_mtls(host, port, client_config, method, params).await
}

async fn dial_pod_mtls(
    host: &str,
    port: u16,
    client_config: ClientConfig,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let connector = TlsConnector::from(Arc::new(client_config));
    let target = format!("{host}:{port}");
    let tcp = TcpStream::connect(&target)
        .await
        .with_context(|| format!("connect {target}"))?;
    let sni = ServerName::try_from(utils::pki::POD_SERVER_SAN)?.to_owned();
    let mut tls = connector.connect(sni, tcp).await.context("TLS handshake")?;
    write_frame(
        &mut tls,
        &serde_json::to_vec(&Request::new(1, method, Some(params)))?,
    )
    .await?;
    let raw = tokio::time::timeout(Duration::from_secs(15), read_frame(&mut tls))
        .await
        .context("response timed out")??;
    parse_resp(&raw)
}

/// Dial a peer over the bootstrap SNI with a pinned pubkey. Used by `pod
/// accept` (join-confirm) and by the auto-offer scheduler (offer push).
async fn dial_bootstrap(
    host: &str,
    port: u16,
    pinned_fp: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let verifier = utils::pki::pinned_bootstrap_verifier(pinned_fp.to_string());
    let client_config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));
    let target = format!("{host}:{port}");
    let tcp = TcpStream::connect(&target)
        .await
        .with_context(|| format!("connect {target}"))?;
    let sni = ServerName::try_from(utils::pki::POD_BOOTSTRAP_SAN)?.to_owned();
    let mut tls = connector
        .connect(sni, tcp)
        .await
        .context("bootstrap TLS handshake (pubkey pin mismatch?)")?;
    write_frame(
        &mut tls,
        &serde_json::to_vec(&Request::new(1, method, Some(params)))?,
    )
    .await?;
    let raw = tokio::time::timeout(Duration::from_secs(15), read_frame(&mut tls))
        .await
        .context("response timed out")??;
    parse_resp(&raw)
}

fn parse_resp(raw: &[u8]) -> Result<serde_json::Value> {
    let msg: Message = serde_json::from_slice(raw)?;
    let resp: Response = match msg {
        Message::Response(r) => r,
        _ => bail!("unexpected message type"),
    };
    if let Some(err) = resp.error {
        bail!("peer returned error: {}", err.message);
    }
    resp.result.context("response had no result")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_offer(expires_at: i64) -> pdb::PendingOffer {
        pdb::PendingOffer {
            offer_id: "o1".into(),
            direction: "in".into(),
            peer_pubkey_fp: "fp".into(),
            peer_hostname: "host-g".into(),
            peer_addr: "10.0.0.1".into(),
            peer_port: 12002,
            code_hash: "h".into(),
            mesh_ca_cert_pem: None,
            inviter_peer_id: None,
            pod_id: None,
            expires_at,
            created_at: 0,
            code_plain: None,
            candidate_addrs: Vec::new(),
        }
    }

    fn mk_disc(addr: &str, hostname: &str, port: u16, fp: &str) -> pdb::DiscoveryRow {
        pdb::DiscoveryRow {
            pubkey_fp: fp.into(),
            peer_id: None,
            hostname: hostname.into(),
            addr: addr.into(),
            port,
            state: "unclaimed".into(),
            can_invite: true,
            first_seen_at: 0,
            last_seen_at: 0,
        }
    }

    #[test]
    fn resolve_no_match_when_unknown_host() {
        let rows = vec![mk_disc("10.0.0.5", "host-g", 12002, "fp1")];
        let got = resolve_offer_target(&rows, "host-h", 12002, true);
        assert!(matches!(got, OfferTargetResolution::NoMatch));
    }

    #[test]
    fn resolve_matches_by_addr() {
        let rows = vec![mk_disc("10.0.0.5", "host-g", 12002, "fp1")];
        let got = resolve_offer_target(&rows, "10.0.0.5", 12002, true);
        assert!(matches!(got, OfferTargetResolution::Match(r) if r.pubkey_fp == "fp1"));
    }

    #[test]
    fn resolve_matches_by_hostname() {
        let rows = vec![mk_disc("10.0.0.5", "host-g", 12002, "fp1")];
        let got = resolve_offer_target(&rows, "host-g", 12002, true);
        assert!(matches!(got, OfferTargetResolution::Match(_)));
    }

    #[test]
    fn resolve_ambiguous_when_multihomed_default_port() {
        let rows = vec![
            mk_disc("10.0.0.5", "host-g", 12002, "fp1"),
            mk_disc("10.0.0.5", "host-g", 12003, "fp2"),
        ];
        let got = resolve_offer_target(&rows, "host-g", 12002, /* default */ true);
        match got {
            OfferTargetResolution::Ambiguous(hits) => assert_eq!(hits.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn resolve_explicit_port_disambiguates() {
        let rows = vec![
            mk_disc("10.0.0.5", "host-g", 12002, "fp1"),
            mk_disc("10.0.0.5", "host-g", 12003, "fp2"),
        ];
        let got = resolve_offer_target(&rows, "host-g", 12003, /* explicit */ false);
        assert!(matches!(got, OfferTargetResolution::Match(r) if r.pubkey_fp == "fp2"));
    }

    #[test]
    fn classify_not_found_when_none() {
        assert!(matches!(
            classify_accept_lookup(None, 1_000),
            AcceptLookup::NotFound
        ));
    }

    #[test]
    fn classify_active_when_within_ttl() {
        let o = mk_offer(1_500);
        let got = classify_accept_lookup(Some(o), 1_000);
        assert!(matches!(got, AcceptLookup::Active(p) if p.offer_id == "o1"));
    }

    #[test]
    fn classify_active_at_exact_boundary() {
        // expires_at == now should still count as active (>= in SQL query too).
        let o = mk_offer(1_000);
        let got = classify_accept_lookup(Some(o), 1_000);
        assert!(matches!(got, AcceptLookup::Active(_)));
    }

    #[test]
    fn classify_expired_reports_seconds_ago() {
        let o = mk_offer(900);
        let got = classify_accept_lookup(Some(o), 1_042);
        assert!(matches!(
            got,
            AcceptLookup::Expired {
                expired_secs_ago: 142
            }
        ));
    }
}
