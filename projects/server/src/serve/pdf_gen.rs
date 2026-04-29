use std::process::Command;
use pulldown_cmark::{html, Options, Parser};

static PDF_BIN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

fn detect_pdf_bin() -> Option<String> {
    for bin in ["wkhtmltopdf", "chromium", "chromium-browser", "google-chrome"] {
        if Command::new("which").arg(bin).output().map(|o| o.status.success()).unwrap_or(false) {
            return Some(bin.to_string());
        }
    }
    None
}

pub fn pdf_bin() -> Option<&'static str> {
    PDF_BIN.get_or_init(detect_pdf_bin).as_deref()
}

/// Render a slice of (title, markdown_content) pairs into a single PDF.
/// Returns the raw PDF bytes.
pub fn render_pdf(pages: &[(String, String)]) -> Result<Vec<u8>, String> {
    let bin = pdf_bin().ok_or_else(|| {
        "PDF generation requires wkhtmltopdf or chromium — install one and restart the server".to_string()
    })?;

    let html = build_html(pages);

    let tmp_dir = std::env::temp_dir();
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let tmp_html = tmp_dir.join(format!("brain-pdf-{id}.html"));
    let tmp_pdf = tmp_dir.join(format!("brain-pdf-{id}.pdf"));

    std::fs::write(&tmp_html, &html).map_err(|e| e.to_string())?;

    let output = if bin == "wkhtmltopdf" {
        Command::new(bin)
            .args(["--quiet", "--print-media-type"])
            .arg(&tmp_html)
            .arg(&tmp_pdf)
            .output()
    } else {
        Command::new(bin)
            .args([
                "--headless",
                "--disable-gpu",
                "--no-sandbox",
                "--print-to-pdf-no-header",
                &format!("--print-to-pdf={}", tmp_pdf.display()),
                &format!("file://{}", tmp_html.display()),
            ])
            .output()
    }
    .map_err(|e| e.to_string())?;

    let _ = std::fs::remove_file(&tmp_html);

    if !output.status.success() {
        let _ = std::fs::remove_file(&tmp_pdf);
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    let bytes = std::fs::read(&tmp_pdf).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&tmp_pdf);
    Ok(bytes)
}

fn build_html(pages: &[(String, String)]) -> String {
    let mut body = String::new();
    for (i, (_title, md)) in pages.iter().enumerate() {
        if i > 0 {
            body.push_str("<div style=\"page-break-before: always\"></div>\n");
        }
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_TABLES);
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_FOOTNOTES);
        let parser = Parser::new_ext(md, opts);
        let mut html_out = String::new();
        html::push_html(&mut html_out, parser);
        body.push_str(&format!("<section>\n{html_out}\n</section>\n"));
    }

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<style>
  body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
         font-size: 13px; line-height: 1.7; color: #1a1a1a;
         max-width: 860px; margin: 0 auto; padding: 2rem; }}
  h1 {{ font-size: 1.7rem; margin: 0 0 0.5rem; }}
  h2 {{ font-size: 1.2rem; margin: 2rem 0 0.5rem; border-bottom: 1px solid #e0e0e0; padding-bottom: 0.3rem; }}
  h3 {{ font-size: 1rem; margin: 1.5rem 0 0.3rem; }}
  pre {{ background: #f5f5f5; border: 1px solid #e0e0e0; border-radius: 4px;
         padding: 0.8rem 1rem; overflow-x: auto; font-size: 0.82rem; line-height: 1.5; }}
  code {{ font-family: "Fira Code", "Cascadia Code", ui-monospace, monospace;
          font-size: 0.85em; background: #f5f5f5; padding: 0.1em 0.3em; border-radius: 3px; }}
  pre code {{ background: none; padding: 0; }}
  table {{ border-collapse: collapse; width: 100%; margin: 1rem 0; font-size: 0.88rem; }}
  th, td {{ border: 1px solid #ddd; padding: 0.4rem 0.7rem; text-align: left; }}
  th {{ background: #f8f8f8; font-weight: 600; }}
  blockquote {{ border-left: 3px solid #ccc; margin: 0; padding-left: 1rem; color: #666; }}
  a {{ color: #0066cc; }}
  @media print {{ body {{ margin: 0; }} }}
</style>
</head>
<body>
{body}
</body>
</html>"#
    )
}
