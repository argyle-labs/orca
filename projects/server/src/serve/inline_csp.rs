#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct CspAdditions(pub Vec<(&'static str, String)>);

impl CspAdditions {
    pub fn grouped(&self) -> Vec<(&'static str, Vec<&str>)> {
        let mut out: Vec<(&'static str, Vec<&str>)> = Vec::new();
        for (d, s) in &self.0 {
            match out.iter_mut().find(|(k, _)| k == d) {
                Some((_, v)) => v.push(s),
                None => out.push((d, vec![s])),
            }
        }
        out
    }
}

fn hash_source(content: &str) -> String {
    format!(
        "'sha256-{}'",
        utils::encoding::base64_encode(&utils::hash::sha256(content.as_bytes()))
    )
}

fn inline_blocks<'a>(html: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        if !after.starts_with(['>', ' ', '\t', '\n', '\r']) {
            rest = after;
            continue;
        }
        let Some(gt) = after.find('>') else { break };
        let attrs = &after[..gt];
        let body = &after[gt + 1..];
        let Some(end) = body.find(&close) else { break };
        if !attrs.contains("src=") {
            out.push(&body[..end]);
        }
        rest = &body[end + close.len()..];
    }
    out
}

fn style_attributes(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find(" style=\"") {
        let after = &rest[i + 8..];
        let Some(end) = after.find('"') else { break };
        out.push(&after[..end]);
        rest = &after[end + 1..];
    }
    out
}

pub fn from_html(html: &str) -> CspAdditions {
    let mut add = Vec::new();
    for s in inline_blocks(html, "script") {
        add.push(("script-src", hash_source(s)));
    }
    for s in inline_blocks(html, "style") {
        add.push(("style-src", hash_source(s)));
    }
    let attrs = style_attributes(html);
    if !attrs.is_empty() {
        add.push(("style-src", "'unsafe-hashes'".to_string()));
        for s in attrs {
            add.push(("style-src", hash_source(s)));
        }
    }
    add.dedup();
    CspAdditions(add)
}

pub fn is_html(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_inline_script_style_and_style_attrs_only() {
        let html = "<html><head><script>alert(1)</script><script src=\"/a.js\"></script>\
            <style>p{}</style><scriptx>no</scriptx></head><body><div style=\"display: contents\"></div></body></html>";
        let a = from_html(html);
        assert_eq!(
            a.0,
            vec![
                ("script-src", hash_source("alert(1)")),
                ("style-src", hash_source("p{}")),
                ("style-src", "'unsafe-hashes'".to_string()),
                ("style-src", hash_source("display: contents")),
            ]
        );
        assert_eq!(
            hash_source("alert(1)"),
            "'sha256-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI='"
        );
    }

    #[test]
    fn no_inline_content_means_no_additions() {
        assert!(
            from_html("<html><script src=\"/x.js\"></script></html>")
                .0
                .is_empty()
        );
    }
}
