//! Host directory roots a plugin may manage, and paths kept inside them.
//!
//! A root is accepted only when it is absolute, has no `.` or `..`, is an
//! existing directory whose canonical path is the path as given (no symlinked
//! component can redirect it later), and sits strictly inside one of the
//! allowed parents: [`DEFAULT_ALLOWED_PARENTS`] or the plugin's own list.
//! Stored roots are re-checked on read with [`valid_roots`], which drops and
//! reports any that no longer pass.

use std::path::{Component, Path, PathBuf};

/// Parents a managed root may sit inside by default.
pub const DEFAULT_ALLOWED_PARENTS: &[&str] = &["/opt", "/srv", "/mnt", "/home", "/data", "/media"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RootError {
    #[error("{kind} '{root}' is not an absolute path")]
    NotAbsolute { kind: String, root: String },
    #[error("{kind} '{root}' contains '.' or '..'")]
    Dotted { kind: String, root: String },
    #[error("{kind} '{root}' is not an existing directory")]
    Missing { kind: String, root: String },
    #[error("{kind} '{root}' resolves through a symlink to '{canonical}'")]
    Symlinked {
        kind: String,
        root: String,
        canonical: String,
    },
    #[error("{kind} '{root}' is not inside any allowed parent ({allowed})")]
    NotAllowed {
        kind: String,
        root: String,
        allowed: String,
    },
}

fn lexically_clean(path: &Path, raw: &str) -> bool {
    !path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        && !raw.split('/').any(|seg| seg == "." || seg == "..")
}

/// `root`, canonical, when it is a directory strictly inside one of
/// `allowed` (each canonicalized; missing ones are skipped) and reached
/// through no symlink. `kind` names the setting in errors.
pub fn validate_root(kind: &str, root: &str, allowed: &[&Path]) -> Result<PathBuf, RootError> {
    let e = || (kind.to_string(), root.to_string());
    let path = Path::new(root);
    if !path.is_absolute() {
        let (kind, root) = e();
        return Err(RootError::NotAbsolute { kind, root });
    }
    if !lexically_clean(path, root) {
        let (kind, root) = e();
        return Err(RootError::Dotted { kind, root });
    }
    let canonical = match path.canonicalize() {
        Ok(c) if c.is_dir() => c,
        _ => {
            let (kind, root) = e();
            return Err(RootError::Missing { kind, root });
        }
    };
    let given: PathBuf = path.components().collect();
    if canonical != given {
        let (kind, root) = e();
        return Err(RootError::Symlinked {
            kind,
            root,
            canonical: canonical.display().to_string(),
        });
    }
    let parents: Vec<PathBuf> = allowed
        .iter()
        .filter_map(|a| a.canonicalize().ok())
        .collect();
    if !parents
        .iter()
        .any(|p| canonical.starts_with(p) && &canonical != p)
    {
        let (kind, root) = e();
        return Err(RootError::NotAllowed {
            kind,
            root,
            allowed: allowed
                .iter()
                .map(|a| a.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        });
    }
    Ok(canonical)
}

/// [`DEFAULT_ALLOWED_PARENTS`] followed by `extra`.
pub fn allowed_parents(extra: &[PathBuf]) -> Vec<PathBuf> {
    DEFAULT_ALLOWED_PARENTS
        .iter()
        .map(PathBuf::from)
        .chain(extra.iter().cloned())
        .collect()
}

/// The roots among `roots` that pass [`validate_root`], with the errors of
/// those that do not.
pub fn valid_roots(
    kind: &str,
    roots: &[String],
    allowed: &[&Path],
) -> (Vec<String>, Vec<RootError>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for r in roots {
        match validate_root(kind, r, allowed) {
            Ok(_) => ok.push(r.clone()),
            Err(e) => bad.push(e),
        }
    }
    (ok, bad)
}

/// `dir`, canonical, when it is an absolute directory without `..` strictly
/// inside one of `roots` (each canonicalized).
pub fn canonical_in_roots(dir: &str, roots: &[String]) -> Option<PathBuf> {
    let path = Path::new(dir);
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return None;
    }
    let real = path.canonicalize().ok().filter(|p| p.is_dir())?;
    roots
        .iter()
        .filter_map(|r| Path::new(r).canonicalize().ok())
        .any(|r| {
            real.strip_prefix(&r)
                .is_ok_and(|rel| rel.components().next().is_some())
        })
        .then_some(real)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().canonicalize().unwrap();
        (t, p)
    }

    #[test]
    fn accepts_a_real_dir_inside_an_allowed_parent() {
        let (_t, base) = tmp();
        let root = base.join("stacks");
        std::fs::create_dir(&root).unwrap();
        let got = validate_root("root", &root.to_string_lossy(), &[&base]).unwrap();
        assert_eq!(got, root);
    }

    #[test]
    fn refuses_slash_relative_dotted_missing_and_the_parent_itself() {
        let (_t, base) = tmp();
        let allowed: &[&Path] = &[&base];
        assert!(matches!(
            validate_root("root", "/", allowed),
            Err(RootError::NotAllowed { .. })
        ));
        assert!(matches!(
            validate_root("root", "rel", allowed),
            Err(RootError::NotAbsolute { .. })
        ));
        let dotted = format!("{}/a/../a", base.display());
        assert!(matches!(
            validate_root("root", &dotted, allowed),
            Err(RootError::Dotted { .. })
        ));
        let missing = format!("{}/nope", base.display());
        assert!(matches!(
            validate_root("root", &missing, allowed),
            Err(RootError::Missing { .. })
        ));
        assert!(matches!(
            validate_root("root", &base.to_string_lossy(), allowed),
            Err(RootError::NotAllowed { .. })
        ));
    }

    #[test]
    fn refuses_system_dirs_outside_the_allowlist() {
        for sys in ["/etc", "/usr", "/var", "/proc"] {
            let parents = allowed_parents(&[]);
            let allowed: Vec<&Path> = parents.iter().map(PathBuf::as_path).collect();
            assert!(validate_root("root", sys, &allowed).is_err(), "{sys}");
        }
    }

    #[test]
    fn refuses_a_symlink_into_the_allowlist_or_out_of_it() {
        let (_t, base) = tmp();
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        let link_in = base.join("link");
        std::os::unix::fs::symlink(&real, &link_in).unwrap();
        let link_out = base.join("etc");
        std::os::unix::fs::symlink("/etc", &link_out).unwrap();
        for l in [&link_in, &link_out] {
            assert!(
                matches!(
                    validate_root("root", &l.to_string_lossy(), &[&base]),
                    Err(RootError::Symlinked { .. })
                ),
                "{}",
                l.display()
            );
        }
    }

    #[test]
    fn valid_roots_drops_and_reports_bad_entries() {
        let (_t, base) = tmp();
        let good = base.join("g");
        std::fs::create_dir(&good).unwrap();
        let roots = vec![good.to_string_lossy().into_owned(), "/var".to_string()];
        let (ok, bad) = valid_roots("discovery_root", &roots, &[&base]);
        assert_eq!(ok, [good.to_string_lossy().into_owned()]);
        assert_eq!(bad.len(), 1);
        assert!(bad[0].to_string().contains("discovery_root '/var'"));
    }

    #[test]
    fn canonical_in_roots_needs_a_dir_strictly_inside() {
        let (_t, base) = tmp();
        let inner = base.join("web");
        std::fs::create_dir(&inner).unwrap();
        let roots = [base.to_string_lossy().into_owned()];
        assert_eq!(
            canonical_in_roots(&inner.to_string_lossy(), &roots),
            Some(inner.clone())
        );
        assert_eq!(canonical_in_roots(&base.to_string_lossy(), &roots), None);
        assert_eq!(canonical_in_roots("relative", &roots), None);
        assert_eq!(
            canonical_in_roots(&format!("{}/../x", inner.display()), &roots),
            None
        );
    }
}
