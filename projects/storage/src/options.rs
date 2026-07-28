//! Generic mount-option **mechanics** — zero fstype grammar.
//!
//! Every network-share backend (nfs, smb, …) tokenizes a comma-separated option
//! string, threads unknown-but-legal options through untouched, and joins a token
//! list back into an option string. Those are the same three mechanical moves for
//! every backend; only the *values* (which keys are known, which defaults form a
//! safety floor) are grammar the owning plugin holds. This module supplies the
//! mechanics so each backend stops hand-rolling its own copy — the backend still
//! owns every fstype-specific value it passes in.
//!
//! Nothing here names `nfs`, `cifs`, a version, a port, or an option key: a
//! caller supplies its own keys/defaults. Kept free of any reactor/transport
//! dependency so a thin plugin links it.

/// One parsed mount option: a bare flag (`hard` → `value: None`) or a
/// `key=value` pair (`vers=4.2` → `value: Some("4.2")`). Borrows from the input
/// string — no allocation. Whitespace around the key and value is trimmed by
/// [`parse_option_string`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountOpt<'a> {
    pub key: &'a str,
    pub value: Option<&'a str>,
}

/// Tokenize a comma-separated mount-option string into [`MountOpt`]s. Splits on
/// `,`, trims each token, skips empties, and splits the first `=` into key +
/// value (a token with no `=` is a bare flag). Pure mechanics — it assigns no
/// meaning to any key. The one tokenizer every backend's option parser drives.
pub fn parse_option_string(raw: &str) -> impl Iterator<Item = MountOpt<'_>> {
    raw.split(',')
        .map(str::trim)
        .filter(|tok| !tok.is_empty())
        .map(|tok| match tok.split_once('=') {
            Some((k, v)) => MountOpt {
                key: k.trim(),
                value: Some(v.trim()),
            },
            None => MountOpt {
                key: tok,
                value: None,
            },
        })
}

/// Is an option with bare key `key` already present in a rendered token list?
/// Matches a bare flag (`p == key`) or the key of a `key=value` token
/// (`p.split('=').next() == Some(key)`). The membership test the safety-floor
/// mechanism uses; the *keys* it is asked about are the caller's grammar.
pub fn option_present(parts: &[String], key: &str) -> bool {
    parts
        .iter()
        .any(|p| p == key || p.split('=').next() == Some(key))
}

/// Apply a **safety floor** to an already-rendered option token list, in place.
///
/// Mechanism only — the *values* are the caller's grammar:
/// - If **any** key in `skip_if_present` is already in `parts`, do nothing (an
///   explicit opt-out was declared, so the floor must not fight it).
/// - Otherwise, for each `(key, token)` in `defaults`, push `token` when `key`
///   is absent from `parts`. Idempotent: a default the operator already declared
///   is never duplicated.
///
/// `defaults` are `(bare-key, full-token)` pairs — e.g. `("timeo", "timeo=50")`
/// so presence is tested by the bare key while the full token is what gets
/// pushed. `parts` order is preserved and pushed defaults keep the given order.
pub fn apply_option_floor(
    parts: &mut Vec<String>,
    defaults: &[(&str, &str)],
    skip_if_present: &[&str],
) {
    if skip_if_present.iter().any(|k| option_present(parts, k)) {
        return;
    }
    for (key, token) in defaults {
        if !option_present(parts, key) {
            parts.push((*token).to_string());
        }
    }
}

/// A thin builder for a comma-joined mount-option string. Appends flags, keyed
/// options, and verbatim passthrough tokens in call order, then joins on `,`.
/// Holds no grammar — the caller decides which keys/flags to add.
#[derive(Debug, Default)]
pub struct OptionBuilder {
    parts: Vec<String>,
}

impl OptionBuilder {
    /// A new, empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an option: a `key=value` pair when `v` is `Some`, or a bare `key`
    /// flag when `v` is `None`.
    pub fn opt(&mut self, k: &str, v: Option<&str>) -> &mut Self {
        match v {
            Some(v) => self.parts.push(format!("{k}={v}")),
            None => self.parts.push(k.to_string()),
        }
        self
    }

    /// Append a bare flag `name` only when `on` is true.
    pub fn flag(&mut self, name: &str, on: bool) -> &mut Self {
        if on {
            self.parts.push(name.to_string());
        }
        self
    }

    /// Append passthrough tokens verbatim, in order (unknown-but-legal options a
    /// backend preserved during parsing).
    pub fn extra(&mut self, items: Vec<String>) -> &mut Self {
        self.parts.extend(items);
        self
    }

    /// Consume the builder and join its tokens with `,`.
    pub fn finish(self) -> String {
        self.parts.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_option_string_splits_flags_and_pairs() {
        let opts: Vec<MountOpt> = parse_option_string("vers=4.2,hard,nconnect=4").collect();
        assert_eq!(
            opts,
            vec![
                MountOpt {
                    key: "vers",
                    value: Some("4.2")
                },
                MountOpt {
                    key: "hard",
                    value: None
                },
                MountOpt {
                    key: "nconnect",
                    value: Some("4")
                },
            ]
        );
    }

    #[test]
    fn parse_option_string_trims_and_skips_empties() {
        let opts: Vec<MountOpt> = parse_option_string("  vers = 4.2 , , hard ,").collect();
        assert_eq!(
            opts,
            vec![
                MountOpt {
                    key: "vers",
                    value: Some("4.2")
                },
                MountOpt {
                    key: "hard",
                    value: None
                },
            ]
        );
    }

    #[test]
    fn parse_option_string_empty_yields_nothing() {
        assert_eq!(parse_option_string("").count(), 0);
        assert_eq!(parse_option_string("  , ,").count(), 0);
    }

    #[test]
    fn option_present_matches_flag_and_keyed() {
        let parts = vec!["vers=4.2".to_string(), "hard".to_string()];
        assert!(option_present(&parts, "vers"));
        assert!(option_present(&parts, "hard"));
        assert!(!option_present(&parts, "soft"));
        assert!(!option_present(&parts, "ver"));
    }

    #[test]
    fn apply_option_floor_skips_when_opt_out_present() {
        let mut parts = vec!["vers=4.2".to_string(), "hard".to_string()];
        apply_option_floor(
            &mut parts,
            &[("soft", "soft"), ("timeo", "timeo=50")],
            &["hard"],
        );
        // `hard` present → floor is a no-op.
        assert_eq!(parts, vec!["vers=4.2".to_string(), "hard".to_string()]);
    }

    #[test]
    fn apply_option_floor_adds_absent_defaults_only() {
        let mut parts = vec!["vers=4.2".to_string(), "timeo=100".to_string()];
        apply_option_floor(
            &mut parts,
            &[
                ("soft", "soft"),
                ("softreval", "softreval"),
                ("timeo", "timeo=50"),
                ("retrans", "retrans=2"),
            ],
            &["hard"],
        );
        // `timeo` already declared → not re-added; the rest are appended in order.
        assert_eq!(
            parts,
            vec![
                "vers=4.2".to_string(),
                "timeo=100".to_string(),
                "soft".to_string(),
                "softreval".to_string(),
                "retrans=2".to_string(),
            ]
        );
    }

    #[test]
    fn option_builder_composes_flags_pairs_and_extra() {
        let mut b = OptionBuilder::new();
        b.opt("vers", Some("4.2"))
            .flag("hard", true)
            .flag("soft", false)
            .opt("guest", None)
            .extra(vec!["nofail".to_string(), "ro".to_string()]);
        assert_eq!(b.finish(), "vers=4.2,hard,guest,nofail,ro");
    }

    #[test]
    fn option_builder_empty_is_empty_string() {
        assert_eq!(OptionBuilder::new().finish(), "");
    }
}
