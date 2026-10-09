//! `SystemId` — the typed argument for every tool field that names a system.
//!
//! A system's id is the UUID persisted to `<app_dir>/machine_id`. Hostnames,
//! display names and addresses are rejected at parse time, on every surface
//! (clap for the CLI, serde for REST/MCP), so no verb ever resolves a name where
//! an id was asked for. Parsing never reads the roster; [`SystemIdHint`], which a
//! host registers on its `ToolCtx`, extends a refusal with the id of the one
//! system the rejected text names, or every candidate when it names several.

use utils::id::Id;

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct SystemId(Id);

impl SystemId {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// Leads every [`InvalidSystemId`] message, followed by the rejected input in
/// backticks. [`SystemIdHint`] finds the input by it.
const INVALID_PREFIX: &str = "expected a system id (UUID), got `";

/// A non-UUID passed where a system id was required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSystemId {
    input: String,
}

impl std::fmt::Display for InvalidSystemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{INVALID_PREFIX}{}` — `orca system list` shows each system's id",
            self.input
        )
    }
}

impl std::error::Error for InvalidSystemId {}

/// What the rejected text names among the known systems.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Hint {
    One { kind: &'static str, id: String },
    Several(Vec<Candidate>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    id: String,
    last_seen_at: Option<i64>,
}

impl std::fmt::Display for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self
            .last_seen_at
            .and_then(utils::time::Timestamp::from_unix_seconds)
        {
            Some(t) => write!(f, "{} (last seen {})", self.id, t.to_rfc3339()),
            None => write!(f, "{} (this system)", self.id),
        }
    }
}

fn hint_text(input: &str, hint: &Hint) -> String {
    match hint {
        Hint::One { kind, id } => format!("`{input}` is {kind} — its id is {id}"),
        Hint::Several(candidates) => {
            let ids: Vec<String> = candidates.iter().map(Candidate::to_string).collect();
            format!(
                "`{input}` names {} systems — pass one of: {}",
                candidates.len(),
                ids.join(", ")
            )
        }
    }
}

const HOSTNAME: &str = "a hostname";
const DISPLAY_NAME: &str = "a display name";
const ADDRESS: &str = "an address";

/// A system as far as name matching is concerned.
struct KnownSystem {
    id: String,
    /// `(kind, value)`, in the order a match is reported.
    labels: Vec<(&'static str, String)>,
    /// `None` for this system.
    last_seen_at: Option<i64>,
}

/// Every system this host knows of: itself plus its non-departed mesh peers.
/// The roster is read only through an already-open process pool: without one,
/// `Db::process()` would open and migrate a database from inside a failed
/// parse, so such a process matches only this system's hostname. A roster that
/// cannot be read just means no hint. Rows whose id is not a UUID are skipped:
/// naming one in a hint would point at an id the caller cannot pass.
fn known_systems() -> Vec<KnownSystem> {
    let db = db::pool::Db::is_process_initialized().then(db::pool::Db::process);
    let mut known = Vec::new();
    if let Some(id) = crate::host_identity::try_machine_id() {
        let mut labels = vec![(HOSTNAME, crate::host_identity::hostname().to_string())];
        if let Some(db) = &db
            && let Ok(rows) = db.read(db::host_addressing::list_host_addressing)
        {
            labels.extend(
                rows.into_iter()
                    .filter(|r| r.kind == "display_name")
                    .map(|r| (DISPLAY_NAME, r.value)),
            );
        }
        known.push(KnownSystem {
            id: id.to_string(),
            labels,
            last_seen_at: None,
        });
    }
    let peers = db.as_ref().map(|db| {
        db.read(|c| {
            let mut out = Vec::new();
            for p in db::mesh::list_peers(c)? {
                if p.departed_at.is_some() {
                    continue;
                }
                let mut labels = vec![(HOSTNAME, p.peer_hostname)];
                for addr in db::host_addressing::list_peer_addresses(c, &p.peer_id)? {
                    labels.push((ADDRESS, addr.value));
                }
                labels.push((ADDRESS, p.peer_addr));
                out.push(KnownSystem {
                    id: p.peer_id,
                    labels,
                    last_seen_at: Some(p.last_seen_at),
                });
            }
            Ok(out)
        })
    });
    if let Some(Ok(peers)) = peers {
        known.extend(peers);
    }
    known.retain(|s| s.id.parse::<Id>().is_ok());
    known
}

/// The system(s) `input` names by hostname, display name or address. Several
/// rows for one system (same id) count as one.
fn name_match(input: &str, known: &[KnownSystem]) -> Option<Hint> {
    let mut matches: Vec<(&'static str, &KnownSystem)> = known
        .iter()
        .filter_map(|s| {
            s.labels
                .iter()
                .find(|(_, v)| !v.is_empty() && v.eq_ignore_ascii_case(input))
                .map(|(kind, _)| (*kind, s))
        })
        .collect();
    matches.sort_by(|a, b| {
        a.1.id
            .to_ascii_lowercase()
            .cmp(&b.1.id.to_ascii_lowercase())
            .then(b.1.last_seen_at.cmp(&a.1.last_seen_at))
    });
    matches.dedup_by(|a, b| a.1.id.eq_ignore_ascii_case(&b.1.id));
    match matches.as_slice() {
        [] => None,
        [(kind, s)] => Some(Hint::One {
            kind,
            id: s.id.clone(),
        }),
        several => Some(Hint::Several(
            several
                .iter()
                .map(|(_, s)| Candidate {
                    id: s.id.clone(),
                    last_seen_at: s.last_seen_at,
                })
                .collect(),
        )),
    }
}

/// The input an [`InvalidSystemId`] rejected, found in a refusal message.
fn rejected_input(message: &str) -> Option<&str> {
    let rest = &message[message.find(INVALID_PREFIX)? + INVALID_PREFIX.len()..];
    rest.find('`').map(|end| &rest[..end])
}

fn hint_for(message: &str, known: &[KnownSystem]) -> Option<String> {
    let input = rejected_input(message)?.trim();
    name_match(input, known).map(|h| hint_text(input, &h))
}

/// Names the system a refused system id's text belongs to. Peers and display
/// names come from the roster, which only a process with an open pool (the
/// daemon) reads; elsewhere only this system's hostname can be named.
pub struct SystemIdHint;

impl contract::ArgRefusalHint for SystemIdHint {
    fn hint(&self, _ctx: &contract::ToolCtx, message: &str) -> Option<String> {
        hint_for(message, &known_systems())
    }
}

impl std::str::FromStr for SystemId {
    type Err = InvalidSystemId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<Id>().map(Self).map_err(|_| InvalidSystemId {
            input: s.to_string(),
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

    fn system(id: &str, labels: &[(&'static str, &str)], last_seen_at: Option<i64>) -> KnownSystem {
        KnownSystem {
            id: id.into(),
            labels: labels.iter().map(|(k, v)| (*k, v.to_string())).collect(),
            last_seen_at,
        }
    }

    fn known() -> Vec<KnownSystem> {
        vec![
            system(
                MINT,
                &[(HOSTNAME, "mint"), (DISPLAY_NAME, "Mint Studio")],
                None,
            ),
            system(
                FREYR,
                &[
                    (HOSTNAME, "freyr"),
                    (ADDRESS, "10.0.0.15"),
                    (ADDRESS, "10.0.0.25"),
                ],
                Some(10),
            ),
            // A second row for the same system (re-keyed / multi-homed) is
            // still one system.
            system(
                FREYR,
                &[(HOSTNAME, "freyr"), (ADDRESS, "10.0.0.16")],
                Some(20),
            ),
        ]
    }

    fn one(input: &str, known: &[KnownSystem]) -> (&'static str, String) {
        match name_match(input, known) {
            Some(Hint::One { kind, id }) => (kind, id),
            other => panic!("expected one match for {input}, got {other:?}"),
        }
    }

    #[test]
    fn uuid_parses() {
        let id: SystemId = MINT.parse().unwrap();
        assert_eq!(id.as_str(), MINT);
    }

    #[test]
    fn hostname_names_its_id() {
        let hint = name_match("MINT", &known()).unwrap();
        assert_eq!(
            hint_text("MINT", &hint),
            format!("`MINT` is a hostname — its id is {MINT}")
        );
    }

    #[test]
    fn display_name_is_labelled_a_display_name() {
        assert_eq!(one("mint studio", &known()), (DISPLAY_NAME, MINT.into()));
    }

    #[test]
    fn duplicate_rows_for_one_system_still_hint() {
        assert_eq!(one("freyr", &known()).1, FREYR);
    }

    #[test]
    fn any_known_address_names_its_id() {
        for addr in ["10.0.0.15", "10.0.0.16", "10.0.0.25"] {
            assert_eq!(one(addr, &known()), (ADDRESS, FREYR.into()));
        }
    }

    #[test]
    fn shared_hostname_lists_each_candidate_with_last_seen() {
        let other = "019f9f7b-3333-7e40-9e30-4987d8d12dcb";
        let mut systems = known();
        systems.push(system(other, &[(HOSTNAME, "freyr")], Some(1_790_000_000)));
        let hint = name_match("freyr", &systems).unwrap();
        assert_eq!(
            hint_text("freyr", &hint),
            format!(
                "`freyr` names 2 systems — pass one of: \
                 {FREYR} (last seen 1970-01-01T00:00:20Z), \
                 {other} (last seen 2026-09-21T14:13:20Z)"
            )
        );
    }

    #[test]
    fn unknown_name_has_no_hint() {
        assert!(name_match("nope", &known()).is_none());
        assert!(name_match("", &[system(MINT, &[(HOSTNAME, ""), (ADDRESS, "")], None)]).is_none());
        let err = "nope".parse::<SystemId>().unwrap_err();
        assert_eq!(
            err.to_string(),
            "expected a system id (UUID), got `nope` — `orca system list` shows each system's id"
        );
    }

    #[test]
    fn the_hint_finds_the_rejected_input_in_a_refusal() {
        let err = " mint ".parse::<SystemId>().unwrap_err();
        let message = format!("invalid args for system.health: id: {err}");
        assert_eq!(rejected_input(&message), Some(" mint "));
        assert_eq!(
            rejected_input("invalid args for x: id: expected an id"),
            None
        );
    }

    #[derive(serde::Deserialize, Debug)]
    struct Args {
        id: Option<SystemId>,
    }

    #[test]
    fn omitted_optional_id_is_none() {
        for json in [r#"{}"#, r#"{"id":null}"#] {
            let args: Args = serde_json::from_str(json).unwrap();
            assert!(args.id.is_none(), "{json}");
        }
        let args: Args = serde_json::from_str(&format!(r#"{{"id":"{MINT}"}}"#)).unwrap();
        assert_eq!(args.id.unwrap().as_str(), MINT);
    }

    #[test]
    fn blank_optional_id_is_rejected() {
        for json in [r#"{"id":""}"#, r#"{"id":"   "}"#] {
            let err = serde_json::from_str::<Args>(json).unwrap_err();
            assert!(
                err.to_string().contains("expected a system id"),
                "{json}: {err}"
            );
        }
    }

    /// Runs `f` with ORCA_HOME and ORCA_DB_PATH in an empty dir and returns what
    /// it left there.
    fn files_left_in_an_empty_home(f: impl FnOnce()) -> Vec<std::ffi::OsString> {
        let home = tempfile::tempdir().expect("tempdir");
        let saved: Vec<_> = ["ORCA_HOME", "ORCA_DB_PATH"]
            .into_iter()
            .map(|k| (k, std::env::var_os(k)))
            .collect();
        // SAFETY: env-touching tests are serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", home.path());
            std::env::set_var("ORCA_DB_PATH", home.path().join("orca.db"));
        }
        f();
        let entries: Vec<_> = std::fs::read_dir(home.path())
            .expect("read home")
            .map(|e| e.expect("entry").file_name())
            .collect();
        for (k, v) in saved {
            // SAFETY: as above.
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        entries
    }

    #[test]
    #[serial_test::serial(env)]
    fn a_failed_parse_never_opens_a_database() {
        let entries = files_left_in_an_empty_home(|| {
            assert!("mint".parse::<SystemId>().is_err());
            assert!(serde_json::from_str::<Args>(r#"{"id":"mint"}"#).is_err());
        });
        assert!(entries.is_empty(), "parse touched {entries:?}");
    }

    #[test]
    #[serial_test::serial(env)]
    fn the_hint_opens_no_database_without_a_process_pool() {
        assert!(!db::pool::Db::is_process_initialized());
        let err = "freyr".parse::<SystemId>().unwrap_err();
        let message = format!("invalid args for system.health: id: {err}");
        let entries = files_left_in_an_empty_home(|| {
            assert!(hint_for(&message, &known_systems()).is_none());
        });
        assert!(entries.is_empty(), "hint touched {entries:?}");
    }

    #[test]
    fn a_refusal_naming_a_known_hostname_gets_its_id() {
        let err = "freyr".parse::<SystemId>().unwrap_err();
        let message = format!("invalid args for system.health: id: {err}");
        assert_eq!(
            hint_for(&message, &known()).as_deref(),
            Some(format!("`freyr` is a hostname — its id is {FREYR}").as_str())
        );
        assert!(hint_for("invalid args for x: id: expected an id", &known()).is_none());
    }

    #[test]
    fn schema_is_a_uuid_string() {
        let schema = schemars::schema_for!(Option<SystemId>);
        assert_eq!(schema.get("format").and_then(|f| f.as_str()), Some("uuid"));
    }
}
