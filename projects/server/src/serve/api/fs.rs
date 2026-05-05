use super::prelude::*;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use utoipa::ToSchema;

use orca_fs::fs::expand_tilde;

#[derive(Deserialize, ToSchema)]
pub struct FsBrowseQuery {
    /// Directory path to list. Supports ~/ expansion.
    pub path: Option<String>,
    /// Request unrestricted browsing — only honoured when fs.allow_unrestricted is enabled in orca.db.
    #[serde(rename = "allowAll", default)]
    pub allow_all: bool,
}

#[derive(Serialize, ToSchema)]
pub struct FsEntry {
    pub name: String,
    pub path: String,
}

#[derive(Serialize, ToSchema)]
pub struct FsBrowseResponse {
    /// Resolved absolute path that was listed.
    pub path: String,
    /// Parent directory path (absent when at root).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub entries: Vec<FsEntry>,
    /// Whether unrestricted browsing is currently enabled in orca.db.
    #[serde(rename = "unrestrictedEnabled")]
    pub unrestricted_enabled: bool,
}

// ── Core logic (extracted for testability) ────────────────────────────────────

#[derive(Debug)]
pub enum BrowseError {
    NotFound(String),
    NotADirectory,
    NeedsAllowAll,
    UnrestrictedDisabled,
    ReadError(String),
}

pub struct BrowseResult {
    pub path: PathBuf,
    pub parent: Option<PathBuf>,
    pub entries: Vec<FsEntry>,
}

/// Pure path-resolution and access-check logic, separated from I/O for testing.
pub fn check_browse_access(
    canonical: &Path,
    home: &Path,
    allow_all: bool,
    unrestricted_enabled: bool,
) -> Result<(), BrowseError> {
    let home_canonical = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    if !canonical.starts_with(&home_canonical) {
        if !allow_all {
            return Err(BrowseError::NeedsAllowAll);
        }
        if !unrestricted_enabled {
            return Err(BrowseError::UnrestrictedDisabled);
        }
    }
    Ok(())
}

pub fn list_dirs(dir: &Path) -> Result<Vec<FsEntry>, BrowseError> {
    let rd = std::fs::read_dir(dir)
        .map_err(|e| BrowseError::ReadError(e.to_string()))?;
    let mut entries: Vec<FsEntry> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| FsEntry {
            name: e.file_name().to_string_lossy().into_owned(),
            path: e.path().to_string_lossy().into_owned(),
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

// ── GET /api/fs/browse ────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/fs/browse",
    operation_id = "fsBrowse",
    params(
        ("path" = Option<String>, Query, description = "Directory to list (supports ~/). Defaults to home directory."),
        ("allowAll" = Option<bool>, Query, description = "Request browsing outside home directory (requires fs.allow_unrestricted = true in orca.db)."),
    ),
    responses(
        (status = 200, body = FsBrowseResponse),
        (status = 400, body = ErrorResponse),
        (status = 403, body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "fs"
)]
pub async fn fs_browse_handler(
    Query(params): Query<FsBrowseQuery>,
) -> axum::response::Response {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return err(StatusCode::INTERNAL_SERVER_ERROR, "cannot resolve home directory"),
    };

    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let unrestricted_enabled = db::fs_allow_unrestricted(&conn);

    let raw = params.path.as_deref().unwrap_or("~/");
    let expanded = expand_tilde(raw);
    let target = PathBuf::from(&expanded);

    let canonical = match target.canonicalize() {
        Ok(p) => p,
        Err(_) => return err(StatusCode::BAD_REQUEST, &format!("path not found: {expanded}")),
    };

    if !canonical.is_dir() {
        return err(StatusCode::BAD_REQUEST, "path is not a directory");
    }

    if let Err(e) = check_browse_access(&canonical, &home, params.allow_all, unrestricted_enabled) {
        return match e {
            BrowseError::NeedsAllowAll => err(
                StatusCode::FORBIDDEN,
                "path is outside home directory; pass allowAll=true to request unrestricted browsing",
            ),
            BrowseError::UnrestrictedDisabled => err(
                StatusCode::FORBIDDEN,
                "unrestricted filesystem browsing is disabled; enable it in orca settings (fs.allow_unrestricted)",
            ),
            _ => err(StatusCode::INTERNAL_SERVER_ERROR, "unexpected access error"),
        };
    }

    let entries = match list_dirs(&canonical) {
        Ok(e) => e,
        Err(BrowseError::ReadError(msg)) => return err(StatusCode::INTERNAL_SERVER_ERROR, &msg),
        Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "failed to read directory"),
    };

    let parent = canonical.parent().map(|p| p.to_string_lossy().into_owned());

    axum::Json(FsBrowseResponse {
        path: canonical.to_string_lossy().into_owned(),
        parent,
        entries,
        unrestricted_enabled,
    })
    .into_response()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    // ── check_browse_access ───────────────────────────────────────────────────

    #[test]
    fn inside_home_always_allowed() {
        let home = tmp_dir();
        let subdir = home.path().join("projects");
        fs::create_dir_all(&subdir).unwrap();
        let canonical = subdir.canonicalize().unwrap();
        assert!(check_browse_access(&canonical, home.path(), false, false).is_ok());
        assert!(check_browse_access(&canonical, home.path(), true, false).is_ok());
        assert!(check_browse_access(&canonical, home.path(), false, true).is_ok());
    }

    #[test]
    fn outside_home_without_allow_all_is_forbidden() {
        let home = tmp_dir();
        let outside = tmp_dir();
        let canonical = outside.path().canonicalize().unwrap();
        let result = check_browse_access(&canonical, home.path(), false, true);
        assert!(matches!(result, Err(BrowseError::NeedsAllowAll)));
    }

    #[test]
    fn outside_home_allow_all_but_db_disabled_is_forbidden() {
        let home = tmp_dir();
        let outside = tmp_dir();
        let canonical = outside.path().canonicalize().unwrap();
        let result = check_browse_access(&canonical, home.path(), true, false);
        assert!(matches!(result, Err(BrowseError::UnrestrictedDisabled)));
    }

    #[test]
    fn outside_home_allow_all_and_db_enabled_is_ok() {
        let home = tmp_dir();
        let outside = tmp_dir();
        let canonical = outside.path().canonicalize().unwrap();
        assert!(check_browse_access(&canonical, home.path(), true, true).is_ok());
    }

    // ── list_dirs ─────────────────────────────────────────────────────────────

    #[test]
    fn list_dirs_returns_only_directories() {
        let dir = tmp_dir();
        fs::create_dir(dir.path().join("subdir")).unwrap();
        fs::write(dir.path().join("file.txt"), "").unwrap();
        fs::write(dir.path().join("readme.md"), "").unwrap();
        let entries = list_dirs(dir.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "subdir");
    }

    #[test]
    fn list_dirs_excludes_dotdirs() {
        let dir = tmp_dir();
        fs::create_dir(dir.path().join("visible")).unwrap();
        fs::create_dir(dir.path().join(".hidden")).unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        let entries = list_dirs(dir.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "visible");
    }

    #[test]
    fn list_dirs_is_sorted() {
        let dir = tmp_dir();
        for name in &["zebra", "alpha", "mango"] {
            fs::create_dir(dir.path().join(name)).unwrap();
        }
        let entries = list_dirs(dir.path()).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "mango", "zebra"]);
    }

    #[test]
    fn list_dirs_entry_path_is_absolute() {
        let dir = tmp_dir();
        fs::create_dir(dir.path().join("child")).unwrap();
        let entries = list_dirs(dir.path()).unwrap();
        assert!(PathBuf::from(&entries[0].path).is_absolute());
    }

    #[test]
    fn list_dirs_nonexistent_returns_error() {
        let result = list_dirs(Path::new("/this/does/not/exist/ever"));
        assert!(matches!(result, Err(BrowseError::ReadError(_))));
    }
}
