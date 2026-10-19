use std::sync::RwLock;

use serde::Deserialize;

pub const CONFIG_NOUN: &str = "server";
pub const CONFIG_NAME: &str = "csp";

const BASELINE: &[(&str, &str)] = &[
    ("default-src", "'self'"),
    ("object-src", "'none'"),
    ("base-uri", "'none'"),
    ("frame-ancestors", "'none'"),
    ("form-action", "'self'"),
];

const SCRIPT_DIRECTIVES: &[&str] = &["script-src", "worker-src"];

static ENFORCED: RwLock<Option<String>> = RwLock::new(None);

#[derive(Deserialize, Default, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Relaxations {
    pub script_src: Vec<String>,
    pub style_src: Vec<String>,
    pub img_src: Vec<String>,
    pub connect_src: Vec<String>,
    pub font_src: Vec<String>,
    pub media_src: Vec<String>,
    pub worker_src: Vec<String>,
    pub frame_src: Vec<String>,
    pub manifest_src: Vec<String>,
}

impl Relaxations {
    fn directives(&self) -> [(&'static str, &[String]); 9] {
        [
            ("script-src", &self.script_src),
            ("style-src", &self.style_src),
            ("img-src", &self.img_src),
            ("connect-src", &self.connect_src),
            ("font-src", &self.font_src),
            ("media-src", &self.media_src),
            ("worker-src", &self.worker_src),
            ("frame-src", &self.frame_src),
            ("manifest-src", &self.manifest_src),
        ]
    }

    pub fn parse(json: &str) -> anyhow::Result<Self> {
        let r: Self = serde_json::from_str(json)?;
        for (directive, sources) in r.directives() {
            for s in sources {
                anyhow::ensure!(
                    valid_source(directive, s),
                    "invalid {directive} source: {s:?}"
                );
            }
        }
        Ok(r)
    }

    pub fn load(conn: &db::Conn) -> Self {
        let row = match db::config_store::get(conn, CONFIG_NOUN, CONFIG_NAME) {
            Ok(Some(row)) => row,
            Ok(None) => return Self::default(),
            Err(e) => {
                tracing::warn!("csp: config read failed, using strict policy: {e:#}");
                return Self::default();
            }
        };
        Self::parse(&row.json).unwrap_or_else(|e| {
            tracing::warn!("csp: invalid server/csp config, using strict policy: {e:#}");
            Self::default()
        })
    }
}

pub fn is_hash_source(s: &str) -> bool {
    contract::web::is_csp_hash(s)
}

fn host_chars(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
}

fn path_chars(s: &str) -> bool {
    s.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_' | '~' | '%' | '@' | '+')
    })
}

fn https_host_source(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("https://") else {
        return false;
    };
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    host_chars(host) && path_chars(path)
}

pub fn valid_source(directive: &str, s: &str) -> bool {
    if s.is_empty() || s.len() > 256 {
        return false;
    }
    if is_hash_source(s) {
        return true;
    }
    if SCRIPT_DIRECTIVES.contains(&directive) {
        return s == "'self'" || s == "'wasm-unsafe-eval'" || https_host_source(s);
    }
    if s.starts_with('\'') {
        return matches!(s, "'self'" | "'unsafe-inline'" | "'unsafe-hashes'");
    }
    if s == "*" || s == "http:" || s.starts_with("http://") {
        return false;
    }
    if let Some(scheme) = s.strip_suffix(':') {
        return matches!(scheme, "https" | "data" | "blob" | "wss" | "mediastream");
    }
    let Some((scheme, rest)) = s.split_once("://") else {
        return false;
    };
    let host_end = rest.find('/').unwrap_or(rest.len());
    let (host, path) = rest.split_at(host_end);
    let host = host.strip_prefix("*.").unwrap_or(host);
    matches!(scheme, "https" | "wss") && host_chars(host) && path_chars(path)
}

pub fn valid_dev_origin(s: &str) -> bool {
    let Some((scheme, host)) = s.split_once("://") else {
        return false;
    };
    matches!(scheme, "http" | "https" | "ws" | "wss") && host_chars(host)
}

pub fn compose(extra: &[(&str, &[&str])], relax: &Relaxations) -> String {
    let mut out: Vec<(String, Vec<String>)> = BASELINE
        .iter()
        .map(|(d, v)| (d.to_string(), vec![v.to_string()]))
        .collect();
    let mut add = |directive: &str, source: &str| {
        let entry = match out.iter_mut().position(|(d, _)| d == directive) {
            Some(i) => &mut out[i].1,
            None => {
                out.push((directive.to_string(), vec!["'self'".to_string()]));
                &mut out.last_mut().expect("just pushed").1
            }
        };
        if !entry.iter().any(|s| s == source) {
            entry.push(source.to_string());
        }
    };
    for (directive, sources) in extra {
        for s in *sources {
            add(directive, s);
        }
    }
    for (directive, sources) in relax.directives() {
        for s in sources {
            add(directive, s);
        }
    }
    out.iter()
        .map(|(d, v)| format!("{d} {}", v.join(" ")))
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn strict() -> String {
    compose(&[], &Relaxations::default())
}

pub fn set_enforced(policy: &str) {
    if let Ok(mut g) = ENFORCED.write() {
        *g = Some(policy.to_string());
    }
}

pub fn enforced() -> Option<String> {
    ENFORCED.read().ok().and_then(|g| g.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_baseline_by_default() {
        assert_eq!(
            strict(),
            "default-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"
        );
    }

    #[test]
    fn relaxations_add_sources() {
        let r = Relaxations::parse(
            r#"{"script-src":["https://cdn.example/x.js"],"img-src":["data:"]}"#,
        )
        .unwrap();
        let p = compose(&[("script-src", &["https://x.example/a.js"])], &r);
        assert!(p.contains("script-src 'self' https://x.example/a.js https://cdn.example/x.js"));
        assert!(p.contains("img-src 'self' data:"));
        assert!(p.contains("frame-ancestors 'none'"));
    }

    #[test]
    fn script_directives_refuse_unsafe_sources() {
        for d in ["script-src", "worker-src"] {
            for bad in [
                "*",
                "http:",
                "https:",
                "data:",
                "blob:",
                "filesystem:",
                "http://cdn.example",
                "'unsafe-inline'",
                "'unsafe-eval'",
                "'strict-dynamic'",
                "'unsafe-hashes'",
                "https://*.example",
                "'none'",
            ] {
                assert!(!valid_source(d, bad), "{d} {bad}");
            }
            for good in [
                "'self'",
                "'wasm-unsafe-eval'",
                "https://cdn.example",
                "https://cdn.example:8443/a/b.js",
                "'sha256-abc='",
            ] {
                assert!(valid_source(d, good), "{d} {good}");
            }
        }
    }

    #[test]
    fn other_directives_refuse_wildcard_and_http() {
        for bad in [
            "*",
            "http:",
            "http://img.example",
            "'unsafe-eval'",
            "a b",
            "a;b",
        ] {
            assert!(!valid_source("img-src", bad), "{bad}");
        }
        for good in ["data:", "blob:", "https://img.example", "'unsafe-inline'"] {
            assert!(valid_source("img-src", good), "{good}");
        }
    }

    #[test]
    fn rejects_injection_and_unknown_directives() {
        assert!(Relaxations::parse(r#"{"script-src":["a; frame-ancestors *"]}"#).is_err());
        assert!(Relaxations::parse(r#"{"frame-ancestors":["*"]}"#).is_err());
        assert!(Relaxations::parse("not json").is_err());
        assert!(Relaxations::parse(r#"{"script-src":["'sha256-abc='"]}"#).is_ok());
    }

    #[test]
    fn dev_origins_must_be_plain_origins() {
        assert!(valid_dev_origin("http://127.0.0.1:12004"));
        assert!(valid_dev_origin("ws://localhost:5173"));
        assert!(!valid_dev_origin("http://a; script-src *"));
        assert!(!valid_dev_origin("javascript://x"));
        assert!(!valid_dev_origin("http://h/path"));
    }
}
