use anyhow::{Result, anyhow, bail};
use rusqlite::Connection;
use utils::http::redact_url;
use utils::route::Route;

pub(crate) struct Target {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

pub(crate) fn target(url: &str) -> Result<Target> {
    let parsed = url::Url::parse(url).map_err(|e| anyhow!("bad URL '{}': {e}", redact_url(url)))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("'{}' has no host", redact_url(url)))?
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow!("'{}' has no port", redact_url(url)))?;
    Ok(Target {
        scheme: parsed.scheme().to_string(),
        host,
        port,
    })
}

pub fn effective_insecure(principal: &str, url: &str, requested: bool) -> Result<bool> {
    if !requested {
        return Ok(false);
    }
    let marked = db::pool::with_read_pooled_or_open(|conn| marked_insecure(conn, principal, url))?;
    if !marked {
        bail!(
            "plugin '{principal}' may not skip TLS verification for '{}': no admin-marked insecure endpoint matches it",
            redact_url(url)
        );
    }
    Ok(true)
}

fn tls_family(scheme: &str) -> Option<bool> {
    match scheme {
        "https" | "wss" => Some(true),
        "http" | "ws" => Some(false),
        _ => None,
    }
}

fn route_matches(r: &Route, t: &Target) -> bool {
    let Some(family) = tls_family(&t.scheme) else {
        return false;
    };
    let Some(route_family) = r.scheme.as_deref().and_then(tls_family) else {
        return false;
    };
    let route_port = r.port.unwrap_or(if route_family { 443 } else { 80 });
    r.enabled
        && route_family == family
        && route_port == t.port
        && r.value
            .trim_matches(['[', ']'])
            .eq_ignore_ascii_case(&t.host)
}

pub(crate) fn marked_insecure(conn: &Connection, provider: &str, url: &str) -> Result<bool> {
    let t = target(url)?;
    let mut stmt = conn.prepare(
        "SELECT routes FROM endpoints WHERE provider = ?1 AND enabled != 0 AND insecure = 1",
    )?;
    let rows = stmt
        .query_map([provider], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.iter().any(|routes| {
        plugin_toolkit::serde_json::from_str::<Vec<Route>>(routes)
            .unwrap_or_default()
            .iter()
            .any(|r| route_matches(r, &t))
    }))
}

#[cfg(test)]
pub(crate) fn with_marked<R>(principal: &str, routes: &str, f: impl FnOnce() -> R) -> R {
    let db = tempfile::NamedTempFile::new().unwrap();
    db::with_thread_db_path(db.path(), || {
        db::pool::with_pooled_or_open(|conn| {
            conn.execute(
                "INSERT INTO endpoints (id, provider, name, routes, enabled, insecure) VALUES (?1, ?1, 'n', ?2, 1, 1)",
                rusqlite::params![principal, routes],
            )?;
            Ok(())
        })
        .unwrap();
        f()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn_with(provider: &str, routes: &str, insecure: i64, enabled: i64) -> Connection {
        let conn = db::testing::test_conn();
        conn.execute(
            "INSERT INTO endpoints (id, provider, name, routes, enabled, insecure) VALUES ('e1', ?1, 'n', ?2, ?3, ?4)",
            rusqlite::params![provider, routes, enabled, insecure],
        )
        .unwrap();
        conn
    }

    const ROUTES: &str = r#"[{"kind":"lan_v4","scheme":"https","value":"10.0.0.9","port":8006}]"#;

    #[test]
    fn matches_an_admin_marked_route() {
        let conn = conn_with("pve", ROUTES, 1, 1);
        assert!(marked_insecure(&conn, "pve", "https://10.0.0.9:8006/api2/json").unwrap());
        assert!(marked_insecure(&conn, "pve", "wss://10.0.0.9:8006/ws").unwrap());
    }

    #[test]
    fn refuses_unmarked_foreign_disabled_or_other_host() {
        let unmarked = conn_with("pve", ROUTES, 0, 1);
        assert!(!marked_insecure(&unmarked, "pve", "https://10.0.0.9:8006/").unwrap());
        let marked = conn_with("pve", ROUTES, 1, 1);
        assert!(!marked_insecure(&marked, "other", "https://10.0.0.9:8006/").unwrap());
        assert!(!marked_insecure(&marked, "pve", "https://10.0.0.10:8006/").unwrap());
        assert!(!marked_insecure(&marked, "pve", "https://10.0.0.9:9000/").unwrap());
        assert!(!marked_insecure(&marked, "pve", "https://10.0.0.9@evil.test:8006/").unwrap());
        let disabled = conn_with("pve", ROUTES, 1, 0);
        assert!(!marked_insecure(&disabled, "pve", "https://10.0.0.9:8006/").unwrap());
    }

    #[test]
    fn portless_route_defaults_to_its_scheme_port() {
        let conn = conn_with(
            "svc",
            r#"[{"kind":"fqdn","scheme":"https","value":"Svc.Lan"}]"#,
            1,
            1,
        );
        assert!(marked_insecure(&conn, "svc", "https://svc.lan/").unwrap());
        assert!(marked_insecure(&conn, "svc", "wss://svc.lan:443/").unwrap());
        assert!(!marked_insecure(&conn, "svc", "https://svc.lan:5001/").unwrap());
    }

    #[test]
    fn explicit_default_port_matches_a_portless_url() {
        let conn = conn_with(
            "svc",
            r#"[{"kind":"fqdn","scheme":"https","value":"svc.lan","port":443}]"#,
            1,
            1,
        );
        assert!(marked_insecure(&conn, "svc", "https://svc.lan/").unwrap());
    }

    #[test]
    fn scheme_family_must_match() {
        let conn = conn_with("pve", ROUTES, 1, 1);
        assert!(!marked_insecure(&conn, "pve", "http://10.0.0.9:8006/").unwrap());
        assert!(!marked_insecure(&conn, "pve", "ws://10.0.0.9:8006/").unwrap());
    }

    #[test]
    fn ipv6_literal_route_matches_bracketed_url() {
        let conn = conn_with(
            "pve",
            r#"[{"kind":"lan_v6","scheme":"https","value":"::1","port":8006}]"#,
            1,
            1,
        );
        assert!(marked_insecure(&conn, "pve", "https://[::1]:8006/").unwrap());
        assert!(!marked_insecure(&conn, "pve", "https://[::2]:8006/").unwrap());
    }

    #[test]
    fn disabled_route_in_enabled_endpoint_is_ignored() {
        let conn = conn_with(
            "pve",
            r#"[{"kind":"lan_v4","scheme":"https","value":"10.0.0.9","port":8006,"enabled":false}]"#,
            1,
            1,
        );
        assert!(!marked_insecure(&conn, "pve", "https://10.0.0.9:8006/").unwrap());
    }

    #[test]
    fn strict_request_needs_no_endpoint() {
        assert!(!effective_insecure("nobody", "https://10.0.0.1/", false).unwrap());
    }
}
