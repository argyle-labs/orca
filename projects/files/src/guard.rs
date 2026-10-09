//! Key-material denylist for every `files.*` verb.
//!
//! `files.read` is a `read`-role tool that takes any absolute path, so without
//! this a read token could pull `.db_key` (which decrypts orca.db), the mesh
//! PKI, or the daemon user's SSH keys.

use anyhow::Result;
use std::path::{Component, Path, PathBuf};

/// Symlink hops followed before a path is treated as a loop and refused.
const MAX_LINK_HOPS: usize = 40;

/// Directories under the daemon user's home that hold private keys.
const HOME_KEY_DIRS: &[&str] = &[".ssh", ".gnupg"];

/// System directories that hold private keys. The `/etc/orca` pair mirrors
/// `storage::SECRET_FILE_DIR` / `LEGACY_SECRET_FILE_DIR` (mount credentials).
const SYSTEM_KEY_DIRS: &[&str] = &[
    "/etc/orca/secret-files",
    "/etc/orca/smb-creds",
    "/etc/ssh",
    "/etc/ssl/private",
    "/etc/pve/priv",
];

fn denied_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = SYSTEM_KEY_DIRS.iter().map(PathBuf::from).collect();
    if let Ok(state) = contract::config::state_dir() {
        dirs.push(state);
    }
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        let home = PathBuf::from(home);
        dirs.extend(HOME_KEY_DIRS.iter().map(|d| home.join(d)));
    }
    // Resolved the same way as the request, so a symlinked home or `/etc`
    // (macOS `/etc` → `/private/etc`) still compares equal.
    dirs.iter().filter_map(|d| resolve_real(d, 0)).collect()
}

/// Refuse `path` when it resolves — through symlinks and `..` — to a denied
/// directory or anything beneath it.
pub fn deny_key_material(path: &Path) -> Result<()> {
    let refused = || {
        contract::OrcaError::forbidden(format!(
            "files: {} is key material and cannot be accessed through files.*",
            path.display()
        ))
    };
    // `/.vol/<dev>/<ino>` opens any file by id, past every path comparison.
    if cfg!(target_os = "macos") && path.starts_with("/.vol") {
        return Err(refused().into());
    }
    let Some(real) = resolve_real(path, 0) else {
        return Err(refused().into());
    };
    if cfg!(target_os = "macos") && real.starts_with("/.vol") {
        return Err(refused().into());
    }
    let denied = denied_dirs();
    if denied.iter().any(|d| path_under(&real, d)) || under_by_identity(&real, &denied) {
        return Err(refused().into());
    }
    Ok(())
}

/// `path` equals or is beneath `dir`. Case-folded on macOS, whose default
/// filesystems are case-insensitive, so `~/.GNUPG/x` cannot slip past
/// `~/.gnupg` before either exists.
fn path_under(path: &Path, dir: &Path) -> bool {
    if cfg!(target_os = "macos") {
        let fold = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        fold(path).starts_with(fold(dir))
    } else {
        path.starts_with(dir)
    }
}

/// The same check by `(st_dev, st_ino)`, which catches a second spelling of
/// one directory that string comparison misses — the macOS firmlink
/// `/System/Volumes/Data/Users/...` for `/Users/...`. Each denied dir is
/// anchored at its deepest existing ancestor plus the not-yet-existing tail,
/// and `path` is refused when one of its existing ancestors is that anchor
/// and the rest of `path` falls under the tail.
#[cfg(unix)]
fn under_by_identity(path: &Path, denied: &[PathBuf]) -> bool {
    use std::os::unix::fs::MetadataExt;
    let id = |p: &Path| std::fs::metadata(p).ok().map(|m| (m.dev(), m.ino()));
    let anchors: Vec<((u64, u64), &Path)> = denied
        .iter()
        .filter_map(|d| {
            d.ancestors()
                .find_map(|a| Some((id(a)?, d.strip_prefix(a).ok()?)))
        })
        .collect();
    path.ancestors().any(|a| {
        let Some(a_id) = id(a) else {
            return false;
        };
        anchors.iter().any(|(anchor, tail)| {
            *anchor == a_id
                && path
                    .strip_prefix(a)
                    .is_ok_and(|rest| path_under(rest, tail))
        })
    })
}

#[cfg(not(unix))]
fn under_by_identity(_path: &Path, _denied: &[PathBuf]) -> bool {
    false
}

/// The path the OS would open for `path`, whether or not it exists yet.
/// Existing prefixes are canonicalized so `..` pops the real parent, and a
/// dangling symlink is followed to where a write would create its target.
/// `None` on a symlink loop.
fn resolve_real(path: &Path, hops: usize) -> Option<PathBuf> {
    if hops > MAX_LINK_HOPS {
        return None;
    }
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => out.push(comp),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => {
                out.push(name);
                if let Ok(canonical) = out.canonicalize() {
                    out = canonical;
                } else if let Ok(target) = std::fs::read_link(&out) {
                    out.pop();
                    out = resolve_real(&out.join(target), hops + 1)?;
                }
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn scratch(tag: &str) -> Scratch {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("orca-files-guard-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn state_key() -> PathBuf {
        let state = contract::config::state_dir().unwrap();
        fs::create_dir_all(&state).unwrap();
        let key = state.join(".db_key");
        if !key.exists() {
            fs::write(&key, "k").unwrap();
        }
        key
    }

    fn assert_refused(p: &Path) {
        let err = deny_key_material(p).expect_err(&format!("{} must be refused", p.display()));
        let oe = err
            .downcast_ref::<contract::OrcaError>()
            .unwrap_or_else(|| panic!("not an OrcaError: {err}"));
        assert_eq!(oe.kind, contract::ErrorKind::Forbidden, "{oe}");
    }

    #[test]
    fn the_state_dir_and_everything_under_it_is_refused() {
        let key = state_key();
        assert_refused(&key);
        assert_refused(key.parent().unwrap());
        assert_refused(&key.parent().unwrap().join("pki").join("ca.key"));
        assert_refused(&key.parent().unwrap().join("orca.db"));
    }

    #[test]
    fn system_and_home_key_dirs_are_refused() {
        assert_refused(Path::new("/etc/ssh/ssh_host_ed25519_key"));
        assert_refused(Path::new("/etc/orca/secret-files/mnt_media.secret"));
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_refused(&home.join(".ssh").join("id_ed25519"));
    }

    #[test]
    fn dot_dot_traversal_into_the_state_dir_is_refused() {
        let key = state_key();
        let s = scratch("dotdot");
        let mut p = s.0.clone();
        for _ in s.0.components().skip(1) {
            p.push("..");
        }
        let p = p.join(key.strip_prefix("/").unwrap());
        assert_refused(&p);
        // `..` past a component that does not exist yet still lands there.
        let state = key.parent().unwrap();
        assert_refused(&state.join("not-yet").join("..").join(".db_key"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_into_key_dirs_are_refused() {
        let key = state_key();
        let state = key.parent().unwrap().to_path_buf();
        let s = scratch("symlink");
        let to_key = s.0.join("innocent.txt");
        std::os::unix::fs::symlink(&key, &to_key).unwrap();
        assert_refused(&to_key);

        let to_dir = s.0.join("docs");
        std::os::unix::fs::symlink(&state, &to_dir).unwrap();
        assert_refused(&to_dir.join(".db_key"));
        // A link whose target sits beside the key, then `..` back up to it.
        fs::create_dir_all(state.join("memory")).unwrap();
        let to_sub = s.0.join("mem");
        std::os::unix::fs::symlink(state.join("memory"), &to_sub).unwrap();
        assert_refused(&to_sub.join("..").join(".db_key"));
        // A dangling link: a write through it would create the file in the
        // state dir.
        let dangling = s.0.join("new.txt");
        std::os::unix::fs::symlink(state.join("planted"), &dangling).unwrap();
        assert_refused(&dangling);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_is_refused() {
        let s = scratch("loop");
        let a = s.0.join("a");
        let b = s.0.join("b");
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        assert_refused(&a);
    }

    /// The firmlink spelling of `p` on macOS (`/System/Volumes/Data/...`),
    /// when the volume layout has one.
    fn data_volume_alias(p: &Path) -> Option<PathBuf> {
        let real = p.canonicalize().ok()?;
        let alias = Path::new("/System/Volumes/Data").join(real.strip_prefix("/").ok()?);
        alias.exists().then_some(alias)
    }

    #[test]
    fn a_second_spelling_of_a_key_dir_is_refused() {
        let key = state_key();
        let state = key.parent().unwrap().to_path_buf();
        if let Some(alias) = data_volume_alias(&state) {
            assert_refused(&alias.join(".db_key"));
            assert_refused(&alias.join("not-yet").join("file"));
            assert_refused(&alias);
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
            && let Some(alias) = data_volume_alias(&home)
        {
            // Whether or not these dirs exist yet.
            assert_refused(&alias.join(".ssh").join("id_ed25519"));
            assert_refused(&alias.join(".gnupg").join("private-keys-v1.d"));
        }
        let s = scratch("alias");
        let parent = s.0.join("parent");
        #[cfg(unix)]
        std::os::unix::fs::symlink(state.parent().unwrap(), &parent).unwrap();
        #[cfg(unix)]
        assert_refused(&parent.join(state.file_name().unwrap()).join(".db_key"));
    }

    #[cfg(unix)]
    #[test]
    fn identity_matches_an_alias_the_strings_do_not() {
        let s = scratch("identity");
        let real = s.0.join("real");
        fs::create_dir_all(&real).unwrap();
        let alias = s.0.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let denied = [real.canonicalize().unwrap().join("keys")];
        assert!(under_by_identity(&alias.join("keys"), &denied));
        assert!(under_by_identity(&alias.join("keys").join("x"), &denied));
        assert!(!under_by_identity(&alias.join("other"), &denied));
        fs::create_dir_all(real.join("keys")).unwrap();
        assert!(under_by_identity(&alias.join("keys").join("x"), &denied));
        assert!(!under_by_identity(&alias, &denied));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_volfs_and_case_variants_are_refused() {
        assert_refused(Path::new("/.vol/16777220/2/etc"));
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_refused(&home.join(".GNUPG").join("x"));
        assert_refused(&home.join(".Ssh").join("id_ed25519"));
        let state = state_key().parent().unwrap().to_path_buf();
        let upper = state.file_name().unwrap().to_string_lossy().to_uppercase();
        assert_refused(&state.parent().unwrap().join(upper).join("not-yet"));
    }

    #[test]
    fn ordinary_paths_pass() {
        let s = scratch("ok");
        let f = s.0.join("notes.md");
        fs::write(&f, "hi").unwrap();
        deny_key_material(&f).unwrap();
        deny_key_material(&s.0.join("not-yet.md")).unwrap();
        deny_key_material(&s.0).unwrap();
    }
}
