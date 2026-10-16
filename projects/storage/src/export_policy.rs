use std::collections::HashSet;
use std::net::{IpAddr, Ipv6Addr};

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
const CONFLICTS: &[(&str, &str)] = &[
    ("ro", "rw"),
    ("sync", "async"),
    ("root_squash", "no_root_squash"),
    ("all_squash", "no_all_squash"),
    ("secure", "insecure"),
    ("subtree_check", "no_subtree_check"),
];
const MIN_V4_PREFIX: u8 = 24;
const MIN_V6_PREFIX: u8 = 64;

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
            self.validate_client(c)?;
        }
        let mut keys = HashSet::new();
        for o in &entry.options {
            self.validate_option(o)?;
            let key = o.split_once('=').map_or(o.as_str(), |(k, _)| k);
            if !keys.insert(key) {
                return Err(invalid(format!("option `{key}` is repeated")));
            }
        }
        if let Some((a, b)) = CONFLICTS
            .iter()
            .find(|(a, b)| keys.contains(a) && keys.contains(b))
        {
            return Err(invalid(format!("options `{a}` and `{b}` conflict")));
        }
        if let Some(fsid) = &entry.fsid
            && (fsid.is_empty() || !fsid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        {
            return Err(invalid(format!(
                "fsid `{}` is not a plain id",
                fsid.escape_debug()
            )));
        }
        if let Some(fsid) = &entry.fsid
            && (fsid == "0" || fsid == "root")
            && !self.allow_unsafe
        {
            return Err(invalid(format!("fsid `{fsid}` requires allowUnsafe")));
        }
        Ok(())
    }

    fn validate_client(&self, client: &str) -> Result<(), StorageError> {
        check_token(client)?;
        let (addr, prefix) = match client.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (client, None),
        };
        let bad = || {
            invalid(format!(
                "client `{client}` is not an allowed IP address or CIDR"
            ))
        };
        let ip: IpAddr = addr.parse().map_err(|_| bad())?;
        let p = match prefix {
            None => None,
            Some(p)
                if !p.is_empty()
                    && p.bytes().all(|b| b.is_ascii_digit())
                    && !(p.len() > 1 && p.starts_with('0')) =>
            {
                Some(p.parse::<u8>().map_err(|_| bad())?)
            }
            Some(_) => return Err(bad()),
        };
        let (v4, p) = match ip {
            IpAddr::V4(v4) => (v4, p.unwrap_or(32)),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => match p.unwrap_or(128).checked_sub(96) {
                    Some(p) => (v4, p),
                    None => return Err(bad()),
                },
                None => return self.check_v6(client, v6, p.unwrap_or(128)),
            },
        };
        if p > 32 || v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast() {
            return Err(bad());
        }
        if p < 32 && u32::from(v4) & (u32::MAX >> p) != 0 {
            return Err(invalid(format!("client `{client}` has host bits set")));
        }
        self.check_width(client, p, MIN_V4_PREFIX)
    }

    fn check_v6(&self, client: &str, v6: Ipv6Addr, p: u8) -> Result<(), StorageError> {
        let v4_compatible = v6.octets()[..12].iter().all(|b| *b == 0) && !v6.is_loopback();
        if p > 128 || v6.is_unspecified() || v6.is_multicast() || v4_compatible {
            return Err(invalid(format!(
                "client `{client}` is not an allowed IP address or CIDR"
            )));
        }
        if p < 128 && u128::from(v6) & (u128::MAX >> p) != 0 {
            return Err(invalid(format!("client `{client}` has host bits set")));
        }
        self.check_width(client, p, MIN_V6_PREFIX)
    }

    fn check_width(&self, client: &str, p: u8, min: u8) -> Result<(), StorageError> {
        if p < min && !self.allow_unsafe {
            return Err(invalid(format!(
                "client `{client}` is wider than /{min} and requires allowUnsafe"
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

pub fn validate_path(path: &str) -> Result<(), StorageError> {
    check_token(path)?;
    if !path.starts_with('/') {
        return Err(invalid(format!("path `{path}` must be absolute")));
    }
    if path.len() == 1 || path.ends_with('/') || path.contains("//") {
        return Err(invalid(format!(
            "path `{path}` must not be `/`, end in `/` or contain `//`"
        )));
    }
    if path.split('/').any(|c| c == "." || c == "..") {
        return Err(invalid(format!(
            "path `{path}` must not contain `.` or `..`"
        )));
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
            &["10.0.0.13", "10.0.0.0/24", "fd00::1", "fd00:1:2:3::/64"],
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
            "10.0.0.0/16",
            "10.0.0.0/33",
            "fd00::/32",
            "fd00:1:2::/48",
            "10.0.0.1/x",
            "10.0.0.0/+24",
            "10.0.0.0/024",
            "10.0.0.0/",
            "10.0.0.1/24",
            "fd00::1/64",
            "::ffff:0.0.0.0/96",
            "::ffff:10.0.0.0/104",
            "::ffff:10.0.0.0/64",
            "::/48",
            "::",
            "0.0.0.0",
            "0.0.0.0/16",
            "255.255.255.255",
            "224.0.0.1",
            "ff02::1",
            "::10.0.0.1",
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
    fn accepts_mapped_v4() {
        assert!(ok(&entry(
            &["::ffff:10.0.0.0/120", "::ffff:10.0.0.13"],
            &["ro"]
        )));
    }

    #[test]
    fn allow_unsafe_relaxes_width_only() {
        let p = ExportPolicy { allow_unsafe: true };
        assert!(
            p.validate(&entry(&["10.0.0.0/16", "fd00::/48"], &["rw"]))
                .is_ok()
        );
        for c in ["*", "0.0.0.0/16", "::/48", "::ffff:0.0.0.0/96"] {
            assert!(p.validate(&entry(&[c], &["rw"])).is_err(), "{c}");
        }
    }

    #[test]
    fn rejects_duplicate_and_conflicting_options() {
        for opts in [
            &["ro", "ro"][..],
            &["anonuid=1", "anonuid=2"],
            &["sec=sys", "sec=krb5"],
            &["ro", "rw"],
            &["sync", "async"],
            &["root_squash", "no_root_squash"],
            &["all_squash", "no_all_squash"],
            &["secure", "insecure"],
            &["subtree_check", "no_subtree_check"],
        ] {
            let e = entry(&["10.0.0.13"], opts);
            assert!(
                ExportPolicy { allow_unsafe: true }.validate(&e).is_err(),
                "{opts:?}"
            );
        }
    }

    #[test]
    fn root_fsid_needs_allow_unsafe() {
        for f in ["0", "root"] {
            let mut e = entry(&["10.0.0.13"], &["ro"]);
            e.fsid = Some(f.into());
            assert!(!ok(&e), "{f}");
            assert!(
                ExportPolicy { allow_unsafe: true }.validate(&e).is_ok(),
                "{f}"
            );
        }
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
            "/",
            "/a/",
            "/a//b",
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
