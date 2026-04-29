use axum::{
    extract::Query,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::io::Write;
use utoipa::ToSchema;

use super::prelude::*;
use crate::serve::{
    pdf_gen::render_pdf,
    tree::{collect_all_files, get_ignored, get_roots},
};

#[derive(Deserialize, ToSchema)]
pub struct PdfQuery {
    pub root: String,
    pub path: String,
    /// merged (default) | zip
    pub output: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/pdf",
    operation_id = "downloadPdf",
    params(
        ("root" = String, Query, description = "Root name (brain | rebuy)"),
        ("path" = String, Query, description = "File path or directory path relative to root"),
        ("output" = Option<String>, Query, description = "merged (default) or zip"),
    ),
    responses(
        (status = 200, description = "PDF or ZIP download"),
        (status = 404, description = "Path not found", body = ErrorResponse),
        (status = 503, description = "PDF binary not available", body = ErrorResponse),
    ),
    tag = "docs"
)]
pub async fn pdf_handler(Query(params): Query<PdfQuery>) -> Response {
    use crate::serve::pdf_gen::pdf_bin;

    if pdf_bin().is_none() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "PDF generation requires wkhtmltopdf or chromium — install one and restart the server",
        );
    }

    let roots = get_roots();
    let Some(root_dir) = roots.get(params.root.as_str()) else {
        return err(StatusCode::BAD_REQUEST, "unknown root");
    };
    let root_dir = root_dir.canonicalize().unwrap_or_else(|_| root_dir.clone());
    let target = root_dir.join(&params.path);

    if !target.starts_with(&root_dir) {
        return err(StatusCode::FORBIDDEN, "path traversal");
    }

    let output_mode = params.output.as_deref().unwrap_or("merged");

    if target.is_file() {
        let md = match std::fs::read_to_string(&target) {
            Ok(s) => s,
            Err(_) => return err(StatusCode::NOT_FOUND, "file not found"),
        };
        let stem = target.file_stem().and_then(|s| s.to_str()).unwrap_or("document").to_string();
        match render_pdf(&[(stem.clone(), md)]) {
            Ok(bytes) => pdf_response(bytes, &format!("{stem}.pdf")),
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
        }
    } else if target.is_dir() {
        let ignored = get_ignored(&params.root);
        let tree = crate::serve::tree::build_tree_raw(&target, &root_dir, &ignored);
        let files = collect_all_files(&tree);
        let folder_name = target.file_name().and_then(|s| s.to_str()).unwrap_or("docs").to_string();

        let mut pages: Vec<(String, String)> = Vec::new();
        for f in &files {
            let full = root_dir.join(&f.path);
            if let Ok(md) = std::fs::read_to_string(&full) {
                pages.push((f.name.clone(), md));
            }
        }

        if pages.is_empty() {
            return err(StatusCode::NOT_FOUND, "no markdown files found");
        }

        match output_mode {
            "zip" => match build_zip(&pages, &folder_name) {
                Ok(bytes) => zip_response(bytes, &format!("{folder_name}-pdfs.zip")),
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
            },
            _ => match render_pdf(&pages) {
                Ok(bytes) => pdf_response(bytes, &format!("{folder_name}.pdf")),
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
            },
        }
    } else {
        err(StatusCode::NOT_FOUND, "path not found")
    }
}

fn pdf_response(bytes: Vec<u8>, filename: &str) -> Response {
    let disposition = format!("attachment; filename=\"{filename}\"");
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/pdf"),
            (header::CONTENT_DISPOSITION, disposition.as_str()),
        ],
        bytes,
    )
        .into_response()
}

fn zip_response(bytes: Vec<u8>, filename: &str) -> Response {
    let disposition = format!("attachment; filename=\"{filename}\"");
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/zip"),
            (header::CONTENT_DISPOSITION, disposition.as_str()),
        ],
        bytes,
    )
        .into_response()
}

fn build_zip(pages: &[(String, String)], _folder: &str) -> Result<Vec<u8>, String> {
    use std::io::Cursor;
    use zip::write::{SimpleFileOptions, ZipWriter};
    use zip::CompressionMethod;

    let buf = Cursor::new(Vec::new());
    let mut zip = ZipWriter::new(buf);
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    for (title, md) in pages {
        let pdf_bytes = render_pdf(&[(title.clone(), md.clone())])?;
        let safe_name = title
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect::<String>();
        zip.start_file(format!("{safe_name}.pdf"), opts).map_err(|e| e.to_string())?;
        zip.write_all(&pdf_bytes).map_err(|e| e.to_string())?;
    }

    let cursor = zip.finish().map_err(|e| e.to_string())?;
    Ok(cursor.into_inner())
}
