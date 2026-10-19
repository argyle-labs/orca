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

const QUOTED_KEYWORDS: &[&str] = &[
    "'self'",
    "'unsafe-inline'",
    "'unsafe-eval'",
    "'wasm-unsafe-eval'",
    "'unsafe-hashes'",
    "'strict-dynamic'",
];

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
                anyhow::ensure!(valid_source(s), "invalid {directive} source: {s:?}");
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

fn valid_source(s: &str) -> bool {
    if s.is_empty() || s.len() > 256 {
        return false;
    }
    if s.starts_with('\'') {
        if QUOTED_KEYWORDS.contains(&s) {
            return true;
        }
        let Some(inner) = s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) else {
            return false;
        };
        return ["sha256-", "sha384-", "sha512-"].iter().any(|p| {
            inner.strip_prefix(p).is_some_and(|b64| {
                !b64.is_empty()
                    && b64
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
            })
        });
    }
    s.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '.' | '*' | ':' | '/' | '-' | '_' | '%' | '~' | '+' | '[' | ']' | '@'
            )
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_baseline_by_default() {
        assert_eq!(
            compose(&[], &Relaxations::default()),
            "default-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"
        );
    }

    #[test]
    fn relaxations_add_sources() {
        let r = Relaxations::parse(r#"{"script-src":["https://cdn.example"],"img-src":["data:"]}"#)
            .unwrap();
        let p = compose(&[("script-src", &["https://x.example/a.js"])], &r);
        assert!(p.contains("script-src 'self' https://x.example/a.js https://cdn.example"));
        assert!(p.contains("img-src 'self' data:"));
        assert!(p.contains("frame-ancestors 'none'"));
    }

    #[test]
    fn rejects_injection_and_unknown_directives() {
        assert!(Relaxations::parse(r#"{"script-src":["a; frame-ancestors *"]}"#).is_err());
        assert!(Relaxations::parse(r#"{"script-src":["'none'"]}"#).is_err());
        assert!(Relaxations::parse(r#"{"script-src":["a b"]}"#).is_err());
        assert!(Relaxations::parse(r#"{"frame-ancestors":["*"]}"#).is_err());
        assert!(Relaxations::parse("not json").is_err());
        assert!(Relaxations::parse(r#"{"script-src":["'sha256-abc='"]}"#).is_ok());
    }
}
