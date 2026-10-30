//! Core diagnostics provider: DNS resolver configuration drift.
//!
//! orca's internal convention is a `nameserver` pointing at the internal
//! gateway/resolver, with the internal domain in `search`, so every host can
//! resolve internal-only hostnames. A host whose `/etc/resolv.conf` points at
//! a public resolver instead (DHCP override, manual edit, cloud-init default,
//! …) can reach the internet fine and still fail every call that names an
//! internal host — which is exactly what happened on `loki`: every
//! plugin-update check failed with DNS resolution errors because its resolver
//! couldn't see the internal Gitea host.
//!
//! Like [`crate::capacity_diagnostics`] and [`crate::mount_diagnostics`], this
//! provider only inspects the host it runs on; a controller gets the whole
//! fleet's verdict by fanning `diagnostics.diagnose` over the mesh.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use contract::BoxFuture;
use contract::diagnostics::{
    DiagnoseArgs, DiagnosticsProvider, Finding, RepairArgs, RepairOutcome, Severity,
};

/// Registry name. Stable — operators type it as `--provider`.
pub const PROVIDER_NAME: &str = "dns-resolver";

/// Internal domain that makes internal-only hostnames resolve. Not a secret
/// (it's orca's public canonical endpoint domain), so it is a plain default —
/// unlike the gateway IP below.
const DEFAULT_SEARCH_DOMAIN: &str = "scottkey.me";

const RESOLV_CONF_PATH: &str = "/etc/resolv.conf";

/// Internal gateway/resolver every managed host is expected to point at,
/// sourced from `ORCA_DNS_EXPECTED_NAMESERVER` so the real address never ships
/// in source (same convention as `hook::opnsense_patterns`'s
/// `ORCA_ROUTER_GUARD_IP`). `None` when unset — the nameserver check is then
/// skipped entirely, rather than compared against a placeholder that would
/// never match any real host's actual resolver.
fn expected_nameserver() -> Option<String> {
    std::env::var("ORCA_DNS_EXPECTED_NAMESERVER").ok()
}

/// Internal search domain, sourced from `ORCA_DNS_EXPECTED_SEARCH_DOMAIN` so a
/// deployment can override it without a rebuild.
fn expected_search_domain() -> String {
    std::env::var("ORCA_DNS_EXPECTED_SEARCH_DOMAIN")
        .unwrap_or_else(|_| DEFAULT_SEARCH_DOMAIN.to_string())
}

/// What's wrong with a host's resolver configuration, or nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Drift {
    nameservers: Vec<String>,
    /// Vacuously `true` when no nameserver is expected (env var unset), so it
    /// never contributes a finding on its own.
    has_internal_nameserver: bool,
    has_internal_search: bool,
}

/// Parse `resolv.conf` text into its `nameserver`/`search` (or `domain`,
/// resolver(5)'s single-entry synonym for `search`) lines and judge whether
/// it matches the internal convention.
///
/// Only the last `search`/`domain` line counts, per resolver(5) — a later
/// line overrides, not adds to, earlier ones. `expected_nameserver` of `None`
/// skips the nameserver check entirely (treated as a match).
///
/// Split from the provider so the judgement is unit-testable without a
/// filesystem — the read is the untestable part, the comparison is not.
fn parse(text: &str, expected_nameserver: Option<&str>, expected_search_domain: &str) -> Drift {
    let mut nameservers = Vec::new();
    let mut search_domains: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("nameserver") {
            if let Some(ns) = rest.split_whitespace().next() {
                nameservers.push(ns.to_string());
            }
        } else if let Some(rest) = line
            .strip_prefix("search")
            .or_else(|| line.strip_prefix("domain"))
        {
            search_domains = rest.split_whitespace().map(str::to_string).collect();
        }
    }
    let has_internal_nameserver = match expected_nameserver {
        Some(ns) => nameservers.iter().any(|n| n == ns),
        None => true,
    };
    let has_internal_search = search_domains.iter().any(|d| d == expected_search_domain);
    Drift {
        nameservers,
        has_internal_nameserver,
        has_internal_search,
    }
}

/// Build a finding from a parsed resolver config, or `None` when it already
/// matches the internal convention.
fn finding_for(
    drift: &Drift,
    hostname: &str,
    expected_nameserver: Option<&str>,
    expected_search_domain: &str,
) -> Option<Finding> {
    if drift.has_internal_nameserver && drift.has_internal_search {
        return None;
    }
    let seen = if drift.nameservers.is_empty() {
        "no nameservers at all".to_string()
    } else {
        format!("nameserver(s) {}", drift.nameservers.join(", "))
    };
    let detail = match expected_nameserver {
        Some(ns) => format!(
            "{RESOLV_CONF_PATH} has {seen} and {} the '{expected_search_domain}' search \
             domain, instead of 'nameserver {ns}' + 'search \
             {expected_search_domain}'. This host cannot resolve internal-only \
             *.{expected_search_domain} hostnames (e.g. gitea.{expected_search_domain}) — \
             the same drift that failed every plugin-update check on loki.",
            if drift.has_internal_search {
                "has"
            } else {
                "lacks"
            }
        ),
        None => format!(
            "{RESOLV_CONF_PATH} lacks the '{expected_search_domain}' search domain (no \
             'search {expected_search_domain}' or 'domain {expected_search_domain}' line). \
             This host cannot resolve internal-only *.{expected_search_domain} hostnames \
             (e.g. gitea.{expected_search_domain}) — the same drift that failed every \
             plugin-update check on loki."
        ),
    };
    let title = match expected_nameserver {
        Some(_) => format!("{hostname}: resolv.conf doesn't point at the internal resolver"),
        None => format!("{hostname}: resolv.conf doesn't have the internal search domain"),
    };
    Some(Finding {
        id: format!("dns-resolver-drift::{hostname}"),
        provider: PROVIDER_NAME.to_string(),
        severity: Severity::Warn,
        title,
        detail,
        repair: None,
    })
}

/// Reports hosts whose resolver config has drifted from the internal
/// nameserver/search-domain convention. See the module docs for scope.
pub struct DnsDiagnostics;

impl DiagnosticsProvider for DnsDiagnostics {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn diagnose(&self, _args: DiagnoseArgs) -> BoxFuture<'_, Result<Vec<Finding>>> {
        Box::pin(async move {
            let text = tokio::fs::read_to_string(RESOLV_CONF_PATH)
                .await
                .map_err(|e| anyhow!("read {RESOLV_CONF_PATH}: {e}"))?;
            let (nameserver, search_domain) = (expected_nameserver(), expected_search_domain());
            let drift = parse(&text, nameserver.as_deref(), &search_domain);
            Ok(finding_for(
                &drift,
                crate::host_identity::hostname(),
                nameserver.as_deref(),
                &search_domain,
            )
            .into_iter()
            .collect())
        })
    }

    fn repair(&self, args: RepairArgs) -> BoxFuture<'_, Result<RepairOutcome>> {
        Box::pin(async move {
            Err(anyhow!(
                "{PROVIDER_NAME} has no automatic repair for {:?} yet: fixing resolv.conf \
                 is a separate follow-up",
                args.repair_id
            ))
        })
    }
}

/// Register this provider. Idempotent — `register_provider` replaces by name.
pub fn register() {
    contract::diagnostics::register_provider(Arc::new(DnsDiagnostics));
}

#[cfg(test)]
mod tests {
    use super::*;

    // Doc-range placeholder — matches the repo's "no real fleet IPs" convention.
    const NS: &str = "10.0.0.1";
    const DOMAIN: &str = "scottkey.me";

    const GOOD: &str = "\
nameserver 10.0.0.1
search scottkey.me
";

    const PUBLIC_RESOLVERS: &str = "\
nameserver 1.1.1.1
nameserver 8.8.8.8
";

    const INTERNAL_NAMESERVER_NO_SEARCH: &str = "\
nameserver 10.0.0.1
";

    const INTERNAL_SEARCH_PUBLIC_NAMESERVER: &str = "\
nameserver 8.8.8.8
search scottkey.me
";

    const DOMAIN_DIRECTIVE: &str = "\
nameserver 10.0.0.1
domain scottkey.me
";

    const LAST_SEARCH_LINE_WINS: &str = "\
nameserver 10.0.0.1
search example.com
search scottkey.me
";

    const LATER_DOMAIN_OVERRIDES_EARLIER_SEARCH: &str = "\
nameserver 10.0.0.1
search scottkey.me
domain example.com
";

    #[test]
    fn matching_the_convention_reports_nothing() {
        let drift = parse(GOOD, Some(NS), DOMAIN);
        assert!(drift.has_internal_nameserver);
        assert!(drift.has_internal_search);
        assert!(finding_for(&drift, "loki", Some(NS), DOMAIN).is_none());
    }

    #[test]
    fn public_resolvers_are_flagged() {
        let drift = parse(PUBLIC_RESOLVERS, Some(NS), DOMAIN);
        let f = finding_for(&drift, "loki", Some(NS), DOMAIN).expect("must report");
        assert_eq!(f.id, "dns-resolver-drift::loki");
        assert_eq!(f.severity, Severity::Warn);
        assert!(f.detail.contains("1.1.1.1"));
        assert!(f.repair.is_none());
    }

    #[test]
    fn internal_nameserver_without_search_domain_is_still_flagged() {
        // The nameserver alone is not enough — without the search domain,
        // bare internal hostnames like `gitea` still won't resolve.
        let drift = parse(INTERNAL_NAMESERVER_NO_SEARCH, Some(NS), DOMAIN);
        assert!(drift.has_internal_nameserver);
        assert!(!drift.has_internal_search);
        assert!(finding_for(&drift, "loki", Some(NS), DOMAIN).is_some());
    }

    #[test]
    fn internal_search_with_a_public_nameserver_is_still_flagged() {
        let drift = parse(INTERNAL_SEARCH_PUBLIC_NAMESERVER, Some(NS), DOMAIN);
        assert!(!drift.has_internal_nameserver);
        assert!(drift.has_internal_search);
        assert!(finding_for(&drift, "loki", Some(NS), DOMAIN).is_some());
    }

    #[test]
    fn an_empty_file_is_flagged_as_no_nameservers() {
        let drift = parse("", Some(NS), DOMAIN);
        let f = finding_for(&drift, "loki", Some(NS), DOMAIN).expect("must report");
        assert!(f.detail.contains("no nameservers at all"));
    }

    #[test]
    fn nameserver_check_is_skipped_when_expectation_is_unset() {
        // No ORCA_DNS_EXPECTED_NAMESERVER configured: a "wrong" nameserver
        // present must not contribute a finding on its own.
        let drift = parse(INTERNAL_SEARCH_PUBLIC_NAMESERVER, None, DOMAIN);
        assert!(
            drift.has_internal_nameserver,
            "vacuously true with no expectation set"
        );
        assert!(drift.has_internal_search);
        assert!(finding_for(&drift, "loki", None, DOMAIN).is_none());
    }

    #[test]
    fn unset_nameserver_expectation_still_flags_a_bad_search_domain() {
        let drift = parse(PUBLIC_RESOLVERS, None, DOMAIN);
        assert!(drift.has_internal_nameserver);
        assert!(!drift.has_internal_search);
        let f = finding_for(&drift, "loki", None, DOMAIN).expect("must report");
        assert!(f.detail.contains(DOMAIN));
    }

    #[test]
    fn domain_directive_is_recognized_as_a_search_synonym() {
        let drift = parse(DOMAIN_DIRECTIVE, Some(NS), DOMAIN);
        assert!(drift.has_internal_search);
    }

    #[test]
    fn only_the_last_search_line_takes_effect() {
        let drift = parse(LAST_SEARCH_LINE_WINS, Some(NS), DOMAIN);
        assert!(drift.has_internal_search);
    }

    #[test]
    fn a_later_domain_line_overrides_an_earlier_search_line() {
        let drift = parse(LATER_DOMAIN_OVERRIDES_EARLIER_SEARCH, Some(NS), DOMAIN);
        assert!(!drift.has_internal_search);
    }

    #[tokio::test]
    async fn register_is_idempotent_and_the_provider_is_reachable() {
        register();
        register();
        let n = contract::diagnostics::providers()
            .iter()
            .filter(|p| p.name() == PROVIDER_NAME)
            .count();
        assert_eq!(n, 1, "re-register must replace, not duplicate");
    }

    #[tokio::test]
    async fn repair_is_an_honest_error_not_a_fake_success() {
        let r = DnsDiagnostics
            .repair(RepairArgs {
                provider: PROVIDER_NAME.to_string(),
                repair_id: "dns-resolver-drift::loki".to_string(),
                confirm: true,
            })
            .await;
        assert!(r.is_err(), "no repair path implemented yet");
    }
}
