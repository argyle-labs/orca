//! Serve a built web UI from a plugin and declare its route.
//!
//! [`EmbeddedSite`] answers `contract::web` render requests from a
//! [`rust_embed::RustEmbed`] dist with safe path handling, cache classes,
//! ETag/304 and a default CSP. [`web_backend`] builds the `web` `BackendDef`
//! the loader parses into a [`WebRoute`](contract::web::WebRoute).

use std::marker::PhantomData;

use contract::web::{
    CAP_CSP_SCRIPT_HASH, CAP_CSP_STYLE_ATTR_HASH, CAP_CSP_STYLE_HASH, CAP_DEV_UPSTREAM,
    CAP_SPA_FALLBACK, WebRequest, WebResponse,
};
use rust_embed::RustEmbed;

/// CSP applied to html responses unless overridden. SvelteKit/Vite emit an
/// inline boot script and inline styles, hence `'unsafe-inline'` there.
pub const DEFAULT_CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-inline'; \
style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; \
connect-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors 'self'";

const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";
const CACHE_NO_CACHE: &str = "no-cache";

/// Serves the embedded dist `E` on a claimed web prefix.
pub struct EmbeddedSite<E: RustEmbed> {
    prefix: String,
    immutable: fn(&str) -> bool,
    csp: Option<String>,
    _assets: PhantomData<E>,
}

impl<E: RustEmbed> EmbeddedSite<E> {
    /// A site mounted at `prefix` (e.g. `"/"` or `"/plugins/x/ui"`), with the
    /// default immutable-asset predicate and [`DEFAULT_CSP`].
    pub fn new(prefix: impl Into<String>) -> Self {
        let prefix = prefix.into().trim_end_matches('/').to_string();
        EmbeddedSite {
            prefix,
            immutable: is_hashed_asset,
            csp: Some(DEFAULT_CSP.to_string()),
            _assets: PhantomData,
        }
    }

    /// Replace the predicate (over the asset's dist-relative path) that marks
    /// a file as content-hashed and therefore cacheable forever.
    pub fn immutable_when(mut self, predicate: fn(&str) -> bool) -> Self {
        self.immutable = predicate;
        self
    }

    /// Replace the html CSP; `None` sends no CSP header.
    pub fn csp(mut self, csp: Option<impl Into<String>>) -> Self {
        self.csp = csp.map(Into::into);
        self
    }

    /// Answer one request. Every miss or refused path is a bare 404 so orca's
    /// SPA fallback still applies.
    pub fn serve(&self, req: &WebRequest) -> WebResponse {
        let head = req.method.eq_ignore_ascii_case("HEAD");
        // `WebRequest::default()` carries an empty method; serde defaults to GET.
        if !head && !req.method.is_empty() && !req.method.eq_ignore_ascii_case("GET") {
            return WebResponse::not_found();
        }
        let Some((rel, file)) = self.resolve(&req.path) else {
            return WebResponse::not_found();
        };

        let etag = format!(
            "\"{}\"",
            crate::hash::hex_encode(&file.metadata.sha256_hash())
        );
        let mime = mime_guess::from_path(&rel).first_or_octet_stream();
        let html = mime.essence_str() == "text/html";
        let cache = if !html && (self.immutable)(&rel) {
            CACHE_IMMUTABLE
        } else {
            CACHE_NO_CACHE
        };

        let mut headers = vec![
            ("etag".to_string(), etag.clone()),
            ("cache-control".to_string(), cache.to_string()),
        ];
        if if_none_match(req, &etag) {
            return WebResponse {
                status: 304,
                headers,
                body_b64: String::new(),
            };
        }
        headers.push(("content-type".to_string(), mime.as_ref().to_string()));
        headers.push(("x-content-type-options".to_string(), "nosniff".to_string()));
        if html && let Some(csp) = &self.csp {
            headers.push(("content-security-policy".to_string(), csp.clone()));
        }
        WebResponse {
            status: 200,
            headers,
            body_b64: if head {
                String::new()
            } else {
                utils::encoding::base64_encode(&file.data)
            },
        }
    }

    fn resolve(&self, path: &str) -> Option<(String, rust_embed::EmbeddedFile)> {
        let rest = if self.prefix.is_empty() {
            path
        } else {
            let rest = path.strip_prefix(&self.prefix)?;
            if !rest.is_empty() && !rest.starts_with('/') {
                return None;
            }
            rest
        };
        let rel = normalise(rest)?;
        if rel.is_empty() || rest.ends_with('/') {
            let index = join(&rel, "index.html");
            return E::get(&index).map(|f| (index, f));
        }
        if let Some(f) = E::get(&rel) {
            return Some((rel, f));
        }
        let index = join(&rel, "index.html");
        E::get(&index).map(|f| (index, f))
    }
}

fn join(dir: &str, file: &str) -> String {
    if dir.is_empty() {
        file.to_string()
    } else {
        format!("{dir}/{file}")
    }
}

/// Percent-decode and validate a request path into a dist-relative key.
/// `None` for anything that could escape the dist or smuggle a separator:
/// `.`/`..` segments, NUL, backslash, a residual `%` (double encoding), or
/// non-UTF-8.
fn normalise(path: &str) -> Option<String> {
    let decoded = String::from_utf8(percent_decode(path)?).ok()?;
    if decoded.contains(['\0', '\\', '%']) {
        return None;
    }
    let mut segs = Vec::new();
    for seg in decoded.split('/') {
        match seg {
            "" => {}
            "." | ".." => return None,
            s => segs.push(s),
        }
    }
    Some(segs.join("/"))
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

fn if_none_match(req: &WebRequest, etag: &str) -> bool {
    req.headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("if-none-match"))
        .flat_map(|(_, v)| v.split(','))
        .map(str::trim)
        .any(|tag| tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag)
}

/// Default immutable predicate: SvelteKit's `_app/immutable/` tree, or a Vite
/// `assets/` file whose name carries a content hash (`index-B1c2D3e4.js`).
pub fn is_hashed_asset(rel: &str) -> bool {
    if rel.starts_with("_app/immutable/") {
        return true;
    }
    let Some(name) = rel.strip_prefix("assets/") else {
        return false;
    };
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    stem.split(['-', '.']).skip(1).any(|tok| {
        tok.len() >= 8
            && tok.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && tok.bytes().any(|b| b.is_ascii_digit())
    })
}

/// Start a typed `web` backend declaration for `name` mounted at `prefix`.
pub fn web_backend(name: impl Into<String>, prefix: impl Into<String>) -> WebBackend {
    let name = name.into();
    WebBackend {
        invoke_prefix: name.clone(),
        name,
        prefix: prefix.into(),
        spa_fallback: false,
        dev_upstream: None,
        csp_script_hashes: Vec::new(),
        csp_style_hashes: Vec::new(),
        csp_style_attr_hashes: Vec::new(),
    }
}

/// Builder for a `web`-domain [`BackendDef`](crate::abi::BackendDef).
#[derive(Clone, Debug)]
pub struct WebBackend {
    name: String,
    prefix: String,
    spa_fallback: bool,
    dev_upstream: Option<String>,
    invoke_prefix: String,
    csp_script_hashes: Vec<String>,
    csp_style_hashes: Vec<String>,
    csp_style_attr_hashes: Vec<String>,
}

impl WebBackend {
    /// Route unmatched paths to this provider (only meaningful on `"/"`).
    pub fn spa_fallback(mut self) -> Self {
        self.spa_fallback = true;
        self
    }

    /// Dev-mode upstream origin orca proxies to instead of calling `render`.
    pub fn dev_upstream(mut self, url: impl Into<String>) -> Self {
        self.dev_upstream = Some(url.into());
        self
    }

    pub fn csp_script_hash(mut self, hash: impl Into<String>) -> Self {
        self.csp_script_hashes.push(hash.into());
        self
    }

    pub fn csp_style_hash(mut self, hash: impl Into<String>) -> Self {
        self.csp_style_hashes.push(hash.into());
        self
    }

    pub fn csp_style_attr_hash(mut self, hash: impl Into<String>) -> Self {
        self.csp_style_attr_hashes.push(hash.into());
        self
    }

    /// Tool prefix the loader invokes `"{prefix}.render"` on; defaults to `name`.
    pub fn invoke_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.invoke_prefix = prefix.into();
        self
    }

    pub fn build(self) -> crate::abi::BackendDef {
        let mut capabilities = Vec::new();
        if self.spa_fallback {
            capabilities.push(CAP_SPA_FALLBACK.to_string());
        }
        if let Some(url) = self.dev_upstream {
            capabilities.push(format!("{CAP_DEV_UPSTREAM}{url}"));
        }
        for h in self.csp_script_hashes {
            capabilities.push(format!("{CAP_CSP_SCRIPT_HASH}{h}"));
        }
        for h in self.csp_style_hashes {
            capabilities.push(format!("{CAP_CSP_STYLE_HASH}{h}"));
        }
        for h in self.csp_style_attr_hashes {
            capabilities.push(format!("{CAP_CSP_STYLE_ATTR_HASH}{h}"));
        }
        crate::abi::BackendDef {
            domain: "web".to_string(),
            name: self.name,
            endpoint: self.prefix,
            capabilities,
            invoke_prefix: self.invoke_prefix,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::web::WebRoute;

    #[derive(RustEmbed)]
    #[folder = "tests/fixtures/ui_site/"]
    struct Fixture;

    fn get(path: &str) -> WebRequest {
        WebRequest {
            path: path.to_string(),
            ..Default::default()
        }
    }

    fn header<'a>(r: &'a WebResponse, name: &str) -> Option<&'a str> {
        r.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn body(r: &WebResponse) -> String {
        String::from_utf8(utils::encoding::base64_decode(&r.body_b64).unwrap()).unwrap()
    }

    fn site() -> EmbeddedSite<Fixture> {
        EmbeddedSite::new("/")
    }

    #[test]
    fn traversal_variants_are_bare_404() {
        for p in [
            "/../index.html",
            "/docs/../index.html",
            "/./index.html",
            "/%2e%2e/index.html",
            "/%2E%2E%2Findex.html",
            "/docs%2f..%2findex.html",
            "/%252e%252e/index.html",
            "/..%5cindex.html",
            "/docs\\..\\index.html",
            "/index.html%00",
            "/index.html\0",
            "/%ff",
            "/%zz",
            "/%",
        ] {
            let r = site().serve(&get(p));
            assert_eq!(r.status, 404, "{p}");
            assert!(r.headers.is_empty() && r.body_b64.is_empty(), "{p}");
        }
    }

    #[test]
    fn index_mapping() {
        assert!(body(&site().serve(&get("/"))).contains("<title>t</title>"));
        assert!(body(&site().serve(&get(""))).contains("<title>t</title>"));
        assert!(body(&site().serve(&get("/docs/"))).contains("docs"));
        assert!(body(&site().serve(&get("/docs"))).contains("docs"));
        assert!(body(&site().serve(&get("//docs//"))).contains("docs"));
    }

    #[test]
    fn miss_is_bare_404() {
        let r = site().serve(&get("/nope/route"));
        assert_eq!(r.status, 404);
        assert!(r.headers.is_empty());
        assert_eq!(site().serve(&get("/assets/")).status, 404);
    }

    #[test]
    fn mime_types() {
        let s = site();
        assert_eq!(
            header(&s.serve(&get("/")), "content-type"),
            Some("text/html")
        );
        assert_eq!(
            header(
                &s.serve(&get("/_app/immutable/chunks/entry.B1c2D3e4.js")),
                "content-type"
            ),
            Some("text/javascript")
        );
        assert_eq!(
            header(&s.serve(&get("/assets/plain.css")), "content-type"),
            Some("text/css")
        );
        assert_eq!(
            header(&s.serve(&get("/manifest.json")), "content-type"),
            Some("application/json")
        );
    }

    #[test]
    fn cache_classes() {
        let s = site();
        for p in [
            "/_app/immutable/chunks/entry.B1c2D3e4.js",
            "/assets/app-3f9a1c2b.css",
        ] {
            assert_eq!(
                header(&s.serve(&get(p)), "cache-control"),
                Some(CACHE_IMMUTABLE),
                "{p}"
            );
        }
        for p in ["/", "/docs/", "/assets/plain.css", "/manifest.json"] {
            assert_eq!(
                header(&s.serve(&get(p)), "cache-control"),
                Some(CACHE_NO_CACHE),
                "{p}"
            );
        }
        let custom = site().immutable_when(|rel| rel == "manifest.json");
        assert_eq!(
            header(&custom.serve(&get("/manifest.json")), "cache-control"),
            Some(CACHE_IMMUTABLE)
        );
    }

    #[test]
    fn etag_and_304() {
        let s = site();
        let first = s.serve(&get("/assets/plain.css"));
        let etag = header(&first, "etag").unwrap().to_string();
        let expected = crate::hash::sha256_hex(b"body{}\n");
        assert_eq!(etag, format!("\"{expected}\""));

        for inm in [
            etag.clone(),
            format!("W/{etag}"),
            format!("\"x\", {etag}"),
            "*".to_string(),
        ] {
            let mut req = get("/assets/plain.css");
            req.headers.push(("if-none-match".to_string(), inm.clone()));
            let r = s.serve(&req);
            assert_eq!(r.status, 304, "{inm}");
            assert!(r.body_b64.is_empty());
            assert_eq!(header(&r, "etag"), Some(etag.as_str()));
        }
        let mut stale = get("/assets/plain.css");
        stale
            .headers
            .push(("if-none-match".to_string(), "\"other\"".to_string()));
        assert_eq!(s.serve(&stale).status, 200);
    }

    #[test]
    fn csp_only_on_html() {
        let s = site();
        assert_eq!(
            header(&s.serve(&get("/")), "content-security-policy"),
            Some(DEFAULT_CSP)
        );
        assert_eq!(
            header(
                &s.serve(&get("/assets/plain.css")),
                "content-security-policy"
            ),
            None
        );
        let custom = site().csp(Some("default-src 'none'"));
        assert_eq!(
            header(&custom.serve(&get("/")), "content-security-policy"),
            Some("default-src 'none'")
        );
        let off = site().csp(None::<String>);
        assert_eq!(
            header(&off.serve(&get("/")), "content-security-policy"),
            None
        );
    }

    #[test]
    fn non_root_prefix_and_methods() {
        let s = EmbeddedSite::<Fixture>::new("/plugins/x/ui/");
        assert_eq!(s.serve(&get("/plugins/x/ui/assets/plain.css")).status, 200);
        assert!(body(&s.serve(&get("/plugins/x/ui"))).contains("<title>t</title>"));
        assert_eq!(s.serve(&get("/plugins/x/uiassets/plain.css")).status, 404);
        assert_eq!(s.serve(&get("/assets/plain.css")).status, 404);

        let mut head = get("/");
        head.method = "HEAD".to_string();
        let r = site().serve(&head);
        assert_eq!(r.status, 200);
        assert!(r.body_b64.is_empty());
        let mut post = get("/");
        post.method = "POST".to_string();
        assert_eq!(site().serve(&post).status, 404);
    }

    #[test]
    fn web_backend_round_trips_into_web_route() {
        let def = web_backend("peacock", "/")
            .spa_fallback()
            .dev_upstream("http://127.0.0.1:12004")
            .csp_script_hash("'sha256-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI='")
            .csp_script_hash("'unsafe-inline'")
            .csp_style_hash(
                "'sha384-xgelKlDcc+BYp0O+ZYzFesLnWCgGpEbXVVZWNQT6WyZYygN01tbEMygnjPXb5OPI'",
            )
            .csp_style_attr_hash("'sha256-tcbDxjMo+xKqM21aCGYbs/QAJqB7yUXC06oPWDapBgc='")
            .csp_style_attr_hash("'unsafe-hashes'")
            .invoke_prefix("pk")
            .build();
        let json = serde_json::to_string(&vec![def]).unwrap();
        let back: Vec<crate::abi::BackendDef> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[0].domain, "web");
        assert_eq!(back[0].name, "peacock");
        assert_eq!(back[0].invoke_prefix, "pk");
        assert_eq!(
            WebRoute::from_def_axes(&back[0].endpoint, &back[0].capabilities),
            WebRoute {
                prefix: "/".to_string(),
                spa_fallback: true,
                dev_upstream: Some("http://127.0.0.1:12004".to_string()),
                csp_script_hashes: vec![
                    "'sha256-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI='".to_string()
                ],
                csp_style_hashes: vec![
                    "'sha384-xgelKlDcc+BYp0O+ZYzFesLnWCgGpEbXVVZWNQT6WyZYygN01tbEMygnjPXb5OPI'"
                        .to_string()
                ],
                csp_style_attr_hashes: vec![
                    "'sha256-tcbDxjMo+xKqM21aCGYbs/QAJqB7yUXC06oPWDapBgc='".to_string()
                ],
            }
        );

        let plain = web_backend("docs", "/docs").build();
        assert_eq!(plain.invoke_prefix, "docs");
        assert_eq!(
            WebRoute::from_def_axes(&plain.endpoint, &plain.capabilities),
            WebRoute {
                prefix: "/docs".to_string(),
                spa_fallback: false,
                dev_upstream: None,
                csp_script_hashes: Vec::new(),
                csp_style_hashes: Vec::new(),
                csp_style_attr_hashes: Vec::new(),
            }
        );
    }
}
