//! `SystemId` — the typed argument for every tool field that names a system.
//!
//! A system's id is the UUID persisted to `<app_dir>/machine_id`. Hostnames,
//! display names and addresses are rejected at parse time, on every surface
//! (clap for the CLI, serde for REST/MCP), so no verb ever resolves a name where
//! an id was asked for. When the rejected text names exactly one known system,
//! the error says which id the caller meant.

use utils::id::Id;

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct SystemId(Id);

impl SystemId {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// A non-UUID passed where a system id was required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSystemId {
    input: String,
    hint: Option<NameMatch>,
}

/// The one known system a rejected name refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NameMatch {
    kind: &'static str,
    id: String,
}

impl std::fmt::Display for InvalidSystemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.hint {
            Some(m) => write!(
                f,
                "expected a system id (UUID); `{}` is {} — its id is {}",
                self.input, m.kind, m.id
            ),
            None => write!(
                f,
                "expected a system id (UUID), got `{}` — `orca system list` shows each system's id",
                self.input
            ),
        }
    }
}

impl std::error::Error for InvalidSystemId {}

/// A system as far as name matching is concerned.
struct KnownSystem {
    id: String,
    hostname: String,
    addr: Option<String>,
}

/// Every system this host knows of: itself plus its non-departed mesh peers.
/// Only consulted on the error path, so a roster that cannot be read just
/// means no hint.
fn known_systems() -> Vec<KnownSystem> {
    let mut known = Vec::new();
    if let Some(id) = crate::host_identity::try_machine_id() {
        known.push(KnownSystem {
            id: id.to_string(),
            hostname: crate::host_identity::hostname().to_string(),
            addr: None,
        });
    }
    if let Ok(peers) = db::pool::Db::process().read(db::mesh::list_peers) {
        known.extend(
            peers
                .into_iter()
                .filter(|p| p.departed_at.is_none())
                .map(|p| KnownSystem {
                    id: p.peer_id,
                    hostname: p.peer_hostname,
                    addr: Some(p.peer_addr),
                }),
        );
    }
    known
}

/// The single system `input` names by hostname or address. Several rows for
/// one system (same id) still count as one; distinct systems sharing the name
/// yield no hint, since naming either id would be a guess.
fn name_match(input: &str, known: &[KnownSystem]) -> Option<NameMatch> {
    let mut matches: Vec<NameMatch> = known
        .iter()
        .filter_map(|s| {
            if s.hostname.eq_ignore_ascii_case(input) {
                Some(NameMatch {
                    kind: "a hostname",
                    id: s.id.clone(),
                })
            } else if s
                .addr
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(input))
            {
                Some(NameMatch {
                    kind: "an address",
                    id: s.id.clone(),
                })
            } else {
                None
            }
        })
        .collect();
    matches.sort_by_key(|m| m.id.to_ascii_lowercase());
    matches.dedup_by(|a, b| a.id.eq_ignore_ascii_case(&b.id));
    match matches.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

impl std::str::FromStr for SystemId {
    type Err = InvalidSystemId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<Id>().map(Self).map_err(|_| InvalidSystemId {
            input: s.to_string(),
            hint: name_match(s.trim(), &known_systems()),
        })
    }
}

impl std::ops::Deref for SystemId {
    type Target = str;

    fn deref(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for SystemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<Id> for SystemId {
    fn from(id: Id) -> Self {
        Self(id)
    }
}

impl From<SystemId> for String {
    fn from(id: SystemId) -> Self {
        id.0.into_string()
    }
}

impl<'de> serde::Deserialize<'de> for SystemId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for SystemId {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SystemId".into()
    }

    fn json_schema(_g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        utils::id::uuid_schema()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINT: &str = "019f9f7b-1176-7e40-9e30-4987d8d12dcb";
    const FREYR: &str = "019f9f7b-2222-7e40-9e30-4987d8d12dcb";

    fn known() -> Vec<KnownSystem> {
        vec![
            KnownSystem {
                id: MINT.into(),
                hostname: "mint".into(),
                addr: None,
            },
            KnownSystem {
                id: FREYR.into(),
                hostname: "freyr".into(),
                addr: Some("10.0.0.15".into()),
            },
            // A second row for the same system (re-keyed / multi-homed) is
            // still one system.
            KnownSystem {
                id: FREYR.into(),
                hostname: "freyr".into(),
                addr: Some("10.0.0.16".into()),
            },
        ]
    }

    #[test]
    fn uuid_parses() {
        let id: SystemId = MINT.parse().unwrap();
        assert_eq!(id.as_str(), MINT);
    }

    #[test]
    fn hostname_names_its_id() {
        let err = InvalidSystemId {
            input: "MINT".into(),
            hint: name_match("MINT", &known()),
        };
        assert_eq!(
            err.to_string(),
            format!("expected a system id (UUID); `MINT` is a hostname — its id is {MINT}")
        );
    }

    #[test]
    fn duplicate_rows_for_one_system_still_hint() {
        let hint = name_match("freyr", &known()).unwrap();
        assert_eq!(hint.id, FREYR);
    }

    #[test]
    fn address_names_its_id() {
        let hint = name_match("10.0.0.15", &known()).unwrap();
        assert_eq!(hint.kind, "an address");
        assert_eq!(hint.id, FREYR);
    }

    #[test]
    fn ambiguous_or_unknown_name_has_no_hint() {
        let mut systems = known();
        systems.push(KnownSystem {
            id: "019f9f7b-3333-7e40-9e30-4987d8d12dcb".into(),
            hostname: "mint".into(),
            addr: None,
        });
        assert!(name_match("mint", &systems).is_none());
        assert!(name_match("nope", &systems).is_none());
        let err = InvalidSystemId {
            input: "nope".into(),
            hint: None,
        };
        assert!(
            err.to_string()
                .starts_with("expected a system id (UUID), got `nope`")
        );
    }

    #[test]
    fn schema_is_a_uuid_string() {
        let schema = schemars::schema_for!(Option<SystemId>);
        assert_eq!(schema.get("format").and_then(|f| f.as_str()), Some("uuid"));
    }
}
