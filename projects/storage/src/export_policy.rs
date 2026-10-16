use std::net::IpAddr;

use crate::{ExportEntry, StorageError};

const SAFE_OPTIONS: &[&str] = &[
    "ro",
    "rw",
    "sync",
    "async",
    "no_subtree_check",
    "subtree_check",
    "root_squash",
    "all_squash",
    "secure",
];
const UNSAFE_OPTIONS: &[&str] = &["no_root_squash", "insecure", "no_all_squash"];
const SEC_FLAVORS: &[&str] = &["sys", "krb5", "krb5i", "krb5p"];
const MIN_V4_PREFIX: u8 = 16;
const MIN_V6_PREFIX: u8 = 48;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportPolicy {
    pub allow_unsafe: bool,
}

impl ExportPolicy {
    pub fn validate(&self, entry: &ExportEntry) -> Result<(), StorageError> {
        validate_path(&entry.path)?;
        if entry.allowed_clients.is_empty() {
            return Err(invalid("at least one client is required"));
        }
        for c in &entry.allowed_clients {
            validate_client(c)?;
        }
        for o in &entry.options {
            self.validate_option(o)?;
        }
        if let Some(fsid) = &entry.fsid
            && (fsid.is_empty() || !fsid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        {
            return Err(invalid(format!(
                "fsid `{}` is not a plain id",
                fsid.escape_debug()
            )));
        }
        Ok(())
    }

    fn validate_option(&self, opt: &str) -> Result<(), StorageError> {
        check_token(opt)?;
        if SAFE_OPTIONS.contains(&opt) {
            return Ok(());
        }
        if UNSAFE_OPTIONS.contains(&opt) {
            return if self.allow_unsafe {
                Ok(())
            } else {
                Err(invalid(format!("option `{opt}` requires allowUnsafe")))
            };
        }
        if let Some(n) = opt
            .strip_prefix("anonuid=")
            .or_else(|| opt.strip_prefix("anongid="))
        {
            return n
                .parse::<u32>()
                .map(|_| ())
                .map_err(|_| invalid(format!("option `{opt}` needs a numeric id")));
        }
        if let Some(flavors) = opt.strip_prefix("sec=") {
            return if !flavors.is_empty() && flavors.split(':').all(|f| SEC_FLAVORS.contains(&f)) {
                Ok(())
            } else {
                Err(invalid(format!("option `{opt}` has an unknown sec flavor")))
            };
        }
        Err(invalid(format!("option `{opt}` is not allowed")))
    }
}

fn invalid(msg: impl Into<String>) -> StorageError {
    StorageError::Other(format!("invalid export: {}", msg.into()))
}

fn check_token(t: &str) -> Result<(), StorageError> {
    if t.is_empty() {
        return Err(invalid("empty token"));
    }
    if let Some(c) = t
        .chars()
        .find(|c| c.is_control() || c.is_whitespace() || "\"'`(),\\".contains(*c))
    {
        return Err(invalid(format!(
            "token `{}` contains forbidden character {:?}",
            t.escape_debug(),
            c
        )));
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), StorageError> {
    check_token(path)?;
    if !path.starts_with('/') {
        return Err(invalid(format!("path `{path}` must be absolute")));
    }
    if path.split('/').any(|c| c == "." || c == "..") {
        return Err(invalid(format!(
            "path `{path}` must not contain `.` or `..`"
        )));
    }
    Ok(())
}

fn validate_client(client: &str) -> Result<(), StorageError> {
    check_token(client)?;
    let (addr, prefix) = match client.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (client, None),
    };
    let ip: IpAddr = addr
        .parse()
        .map_err(|_| invalid(format!("client `{client}` is not an IP address or CIDR")))?;
    let Some(p) = prefix else {
        return Ok(());
    };
    let p: u8 = p
        .parse()
        .map_err(|_| invalid(format!("client `{client}` has an invalid prefix")))?;
    let (min, max) = match ip {
        IpAddr::V4(_) => (MIN_V4_PREFIX, 32),
        IpAddr::V6(_) => (MIN_V6_PREFIX, 128),
    };
    if p > max {
        return Err(invalid(format!("client `{client}` has an invalid prefix")));
    }
    if p < min {
        return Err(invalid(format!("client `{client}` is wider than /{min}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(clients: &[&str], options: &[&str]) -> ExportEntry {
        ExportEntry {
            path: "/mnt/user/backups".into(),
            allowed_clients: clients.iter().map(|s| s.to_string()).collect(),
            options: options.iter().map(|s| s.to_string()).collect(),
            fsid: Some("106".into()),
        }
    }

    fn ok(e: &ExportEntry) -> bool {
        ExportPolicy::default().validate(e).is_ok()
    }

    #[test]
    fn accepts_locked_down_export() {
        assert!(ok(&entry(
            &["10.0.0.13", "10.0.0.0/24", "fd00::1", "fd00:1:2::/48"],
            &[
                "rw",
                "sync",
                "no_subtree_check",
                "all_squash",
                "anonuid=99",
                "anongid=100",
                "sec=krb5:sys"
            ],
        )));
    }

    #[test]
    fn rejects_bad_clients() {
        for c in [
            "*",
            "10.0.*",
            "host.lan",
            "0.0.0.0/0",
            "10.0.0.0/8",
            "10.0.0.0/33",
            "fd00::/32",
            "10.0.0.1/x",
            "",
            "10.0.0.1 10.0.0.2",
        ] {
            assert!(!ok(&entry(&[c], &["ro"])), "{c}");
        }
        assert!(!ok(&entry(&[], &["ro"])));
    }

    #[test]
    fn rejects_unknown_and_malformed_options() {
        for o in [
            "crossmnt",
            "anonuid=x",
            "sec=none",
            "sec=",
            "rw,no_root_squash",
            "rw)",
            "rw\n",
            "\"rw\"",
            "r w",
        ] {
            assert!(!ok(&entry(&["10.0.0.13"], &[o])), "{o:?}");
        }
    }

    #[test]
    fn unsafe_options_need_allow_unsafe() {
        for o in UNSAFE_OPTIONS {
            let e = entry(&["10.0.0.13"], &[o]);
            assert!(!ok(&e), "{o}");
            assert!(
                ExportPolicy { allow_unsafe: true }.validate(&e).is_ok(),
                "{o}"
            );
        }
    }

    #[test]
    fn allow_unsafe_does_not_relax_clients() {
        let e = entry(&["*"], &["rw"]);
        assert!(ExportPolicy { allow_unsafe: true }.validate(&e).is_err());
    }

    #[test]
    fn rejects_bad_paths_and_fsid() {
        for p in [
            "relative",
            "/a/../b",
            "/a/./b",
            "/a b",
            "/a\n10.0.0.0/8(rw)",
            "",
        ] {
            let mut e = entry(&["10.0.0.13"], &["ro"]);
            e.path = p.into();
            assert!(!ok(&e), "{p:?}");
        }
        let mut e = entry(&["10.0.0.13"], &["ro"]);
        e.fsid = Some("1\n".into());
        assert!(!ok(&e));
    }
}
