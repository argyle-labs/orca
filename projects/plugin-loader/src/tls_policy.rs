use anyhow::{Result, anyhow, bail};
use rusqlite::Connection;
use tokio_tungstenite::tungstenite::http::Uri;
use utils::http::redact_url;
use utils::route::Route;

pub fn effective_insecure(principal: &str, url: &str, requested: bool) -> Result<bool> {
    if !requested {
        return Ok(false);
    }
    let marked = db::pool::with_pooled_or_open(|conn| marked_insecure(conn, principal, url))?;
    if !marked {
        bail!(
            "plugin '{principal}' may not skip TLS verification for '{}': no admin-marked insecure endpoint matches it",
            redact_url(url)
        );
    }
    Ok(true)
}

fn host_port(url: &str) -> Result<(String, u16)> {
    let uri: Uri = url
        .parse()
        .map_err(|e| anyhow!("bad URL '{}': {e}", redact_url(url)))?;
    let host = uri
        .host()
        .ok_or_else(|| anyhow!("'{}' has no host", redact_url(url)))?
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    let port = match uri.port_u16() {
        Some(p) => p,
        None => match uri.scheme_str() {
            Some("https" | "wss") => 443,
            Some("http" | "ws") => 80,
            _ => bail!("'{}' has no port", redact_url(url)),
        },
    };
    Ok((host, port))
}

pub(crate) fn marked_insecure(conn: &Connection, provider: &str, url: &str) -> Result<bool> {
    let (host, port) = host_port(url)?;
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
            .any(|r| {
                r.enabled
                    && r.value.trim_matches(['[', ']']).eq_ignore_ascii_case(&host)
                    && r.port.is_none_or(|p| p == port)
            })
    }))
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
    fn portless_route_matches_any_port() {
        let conn = conn_with(
            "svc",
            r#"[{"kind":"fqdn","scheme":"https","value":"Svc.Lan"}]"#,
            1,
            1,
        );
        assert!(marked_insecure(&conn, "svc", "https://svc.lan/").unwrap());
        assert!(marked_insecure(&conn, "svc", "https://svc.lan:5001/").unwrap());
    }

    #[test]
    fn strict_request_needs_no_endpoint() {
        assert!(!effective_insecure("nobody", "https://10.0.0.1/", false).unwrap());
    }
}
