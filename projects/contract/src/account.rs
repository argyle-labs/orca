#![allow(
    clippy::disallowed_types,
    reason = "untyped JSON at the plugin invoke boundary"
)]

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utils::id::Id;
use utils::secret::Secret;

use crate::BoxFuture;

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    OsUser,
    AppUser,
    ApiKey,
    ServiceToken,
    SshKey,
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct AccountDecl {
    #[serde(default)]
    pub unit_kinds: Vec<String>,
    #[serde(default)]
    pub account_kinds: Vec<AccountKind>,
    #[serde(default)]
    pub capabilities: Vec<KindCapabilities>,
}

/// What a provider can do for one account kind. Core reads this before it
/// writes the secrets backend: `regenerate` picks unit-minted over
/// orca-generated values, and without `rollback_secret` a failed rotation
/// cannot be rolled back on the unit.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct KindCapabilities {
    pub kind: AccountKind,
    #[serde(default)]
    pub set_secret: bool,
    #[serde(default)]
    pub regenerate: bool,
    #[serde(default)]
    pub rollback_secret: bool,
    #[serde(default)]
    pub verify: bool,
}

impl AccountDecl {
    /// `Unknown` always yields no capabilities: several unrecognised kinds
    /// share that value, so no single entry describes it.
    pub fn capabilities_for(&self, kind: AccountKind) -> KindCapabilities {
        self.capabilities
            .iter()
            .filter(|_| kind != AccountKind::Unknown)
            .find(|c| c.kind == kind)
            .cloned()
            .unwrap_or_else(|| KindCapabilities::none(kind))
    }

    /// Each kind appears once in `account_kinds` and at most once in
    /// `capabilities`, and only kinds listed in `account_kinds` carry
    /// capabilities. `Unknown` is exempt: distinct kinds from a newer plugin
    /// all decode to it, so they would look like duplicates.
    pub fn validate(&self) -> std::result::Result<(), String> {
        for (i, k) in self.account_kinds.iter().enumerate() {
            if *k != AccountKind::Unknown && self.account_kinds[..i].contains(k) {
                return Err(format!("account kind {k:?} declared twice"));
            }
        }
        for (i, c) in self.capabilities.iter().enumerate() {
            if c.kind == AccountKind::Unknown {
                continue;
            }
            if self.capabilities[..i].iter().any(|p| p.kind == c.kind) {
                return Err(format!("capabilities for {:?} declared twice", c.kind));
            }
            if !self.account_kinds.contains(&c.kind) {
                return Err(format!(
                    "capabilities for {:?}, which is not in account_kinds",
                    c.kind
                ));
            }
        }
        Ok(())
    }
}

impl KindCapabilities {
    pub fn none(kind: AccountKind) -> Self {
        Self {
            kind,
            set_secret: false,
            regenerate: false,
            rollback_secret: false,
            verify: false,
        }
    }
}

/// The provider does not implement `op`. Survives the wire, so core can
/// `downcast_ref::<Unsupported>()` on a proxied error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    pub op: String,
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "account op unsupported: {}", self.op)
    }
}

impl std::error::Error for Unsupported {}

/// A provider's `consumers` call failed, so its holdings are unknown; core
/// reports this as drift against `provider`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerDiscoveryFailed {
    pub provider: String,
    pub cause: String,
}

impl std::fmt::Display for ConsumerDiscoveryFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "account provider '{}' consumers failed: {}",
            self.provider, self.cause
        )
    }
}

impl std::error::Error for ConsumerDiscoveryFailed {}

#[derive(Serialize, Clone, Debug, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct NonEmpty(String);

impl JsonSchema for NonEmpty {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "NonEmpty".into()
    }

    fn json_schema(_g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "minLength": 1,
        })
    }
}

impl NonEmpty {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for NonEmpty {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.is_empty() || s.trim() != s {
            return Err("value must be non-empty with no surrounding whitespace".to_string());
        }
        Ok(Self(s.to_string()))
    }
}

impl<'de> Deserialize<'de> for NonEmpty {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for NonEmpty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<&str> for NonEmpty {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct AccountRef {
    pub unit_id: Id,
    pub account: NonEmpty,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct AccountInfo {
    pub unit_id: Id,
    pub name: NonEmpty,
    pub kind: AccountKind,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// False for an account that existed before orca adopted it.
    #[serde(default)]
    pub managed: bool,
}

fn default_enabled() -> bool {
    true
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct ListRequest {
    pub unit_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<AccountKind>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct DetailRequest {
    pub account: AccountRef,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct CreateRequest {
    pub account: AccountRef,
    pub kind: AccountKind,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(
        default,
        serialize_with = "serialize_opt_secret",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<String>")]
    pub secret: Option<Secret<String>>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct UpdateRequest {
    pub account: AccountRef,
    #[serde(default)]
    pub roles: Option<Vec<String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct DeleteRequest {
    pub account: AccountRef,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct SetSecretRequest {
    pub account: AccountRef,
    #[serde(
        default,
        serialize_with = "serialize_opt_secret",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<String>")]
    pub secret: Option<Secret<String>>,
    #[serde(default)]
    pub dry_run: bool,
    /// Puts the previous value back; the outcome must set `restored`.
    #[serde(default)]
    pub rollback: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct VerifyRequest {
    pub account: AccountRef,
    #[serde(serialize_with = "Secret::serialize_exposed")]
    #[schemars(with = "String")]
    pub secret: Secret<String>,
}

/// "This plugin holds account `account` in its setting `setting` on unit
/// `consumer_unit_id`." Owner and consumer are addressed by unit id only, so a
/// consumer never names the plugin that provides the account.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct ConsumerDecl {
    pub account: AccountRef,
    pub consumer_unit_id: Id,
    pub setting: NonEmpty,
    #[serde(default)]
    pub capabilities: ConsumerCapabilities,
}

/// Declared by the consumer for its own holding. Core refuses to rotate an
/// account unless every holding [`supports_rotation`](ConsumerDecl::supports_rotation).
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ConsumerCapabilities {
    #[serde(default)]
    pub apply: bool,
    #[serde(default)]
    pub verify: bool,
    #[serde(default)]
    pub rollback: bool,
}

impl ConsumerDecl {
    /// Every rotation step is possible on this holding: apply the new value,
    /// verify it, and roll it back on failure.
    pub fn supports_rotation(&self) -> bool {
        let c = self.capabilities;
        c.apply && c.verify && c.rollback
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Default)]
pub struct ConsumersRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountRef>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct ConsumerApplyRequest {
    pub holding: ConsumerDecl,
    #[serde(
        default,
        serialize_with = "serialize_opt_secret",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<String>")]
    pub secret: Option<Secret<String>>,
    #[serde(default)]
    pub dry_run: bool,
    /// Puts the previous value back; the outcome must set `restored`.
    #[serde(default)]
    pub rollback: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct ConsumerVerifyRequest {
    pub holding: ConsumerDecl,
    #[serde(serialize_with = "Secret::serialize_exposed")]
    #[schemars(with = "String")]
    pub secret: Secret<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct RegenerateRequest {
    pub account: AccountRef,
    #[serde(default)]
    pub dry_run: bool,
    /// The provider must mint at least this many characters; never below
    /// [`MIN_SECRET_LEN`].
    #[serde(default = "min_secret_len")]
    pub min_len: usize,
}

fn min_secret_len() -> usize {
    MIN_SECRET_LEN
}

/// `regenerate` already rotated the credential on the unit, but the minted
/// value is too short to accept. `generated` is the live value, so core can
/// store it or roll the unit back; it never appears in the message.
#[derive(Debug, Clone)]
pub struct RotatedButRejected {
    pub account: AccountRef,
    pub generated: Secret<String>,
}

impl std::fmt::Display for RotatedButRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "account {}/{} was already rotated on the unit, but its generated value is \
             shorter than {MIN_SECRET_LEN} characters; store it or roll the unit back",
            self.account.unit_id, self.account.account
        )
    }
}

impl std::error::Error for RotatedButRejected {}

/// `generated` carries a credential the unit minted itself (e.g. an app that
/// regenerates its own api_key), so core can write it to the secrets backend.
/// It is never placed in `detail`, which is scrubbed.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct AccountOutcome {
    pub executed: bool,
    #[serde(default)]
    pub detail: serde_json::Value,
    #[serde(
        default,
        serialize_with = "serialize_opt_secret",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<String>")]
    pub generated: Option<Secret<String>>,
    /// Set on every `rollback` call. `Some(false)` means the previous value is
    /// not in place, so the unit and the secrets backend disagree and core
    /// escalates instead of reporting the rollback done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored: Option<bool>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct VerifyOutcome {
    pub matches: bool,
    #[serde(default)]
    pub detail: serde_json::Value,
}

fn serialize_opt_secret<S: serde::Serializer>(
    v: &Option<Secret<String>>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    match v {
        Some(secret) => Secret::serialize_exposed(secret, s),
        None => s.serialize_none(),
    }
}

/// Provider ops default to "unsupported" and `consumers` to none, so a plugin
/// that only holds other units' credentials implements just the consumer ops.
///
/// Rotation runs: write the secrets backend, `set_secret` on the owner (or
/// `regenerate`, then write the backend), `apply_consumer` on every holding,
/// then verify all of them. Rollback undoes it in reverse:
/// `apply_consumer(old, rollback: true)` on each consumer already updated,
/// `set_secret(old, rollback: true)` on the owner, then the secrets seam's
/// `restore` op ([`crate::secrets_backend::RESTORE_OP`]). Each rollback call
/// reports `restored`.
pub trait AccountProvider: Send + Sync {
    fn name(&self) -> &str;

    fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>>;

    fn list(&self, _req: ListRequest) -> BoxFuture<'_, Result<Vec<AccountInfo>>> {
        unsupported(LIST_OP)
    }

    /// `None` when the unit has no such account.
    fn detail(&self, _req: DetailRequest) -> BoxFuture<'_, Result<Option<AccountInfo>>> {
        unsupported(DETAIL_OP)
    }

    fn create(&self, _req: CreateRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
        unsupported(CREATE_OP)
    }

    fn update(&self, _req: UpdateRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
        unsupported(UPDATE_OP)
    }

    fn delete(&self, _req: DeleteRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
        unsupported(DELETE_OP)
    }

    /// Rollback is `set_secret` with the previous value and `rollback: true`.
    fn set_secret(&self, _req: SetSecretRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
        unsupported(SET_SECRET_OP)
    }

    fn verify(&self, _req: VerifyRequest) -> BoxFuture<'_, Result<VerifyOutcome>> {
        unsupported(VERIFY_OP)
    }

    /// For units that mint their own credential: the new value comes back in
    /// [`AccountOutcome::generated`].
    fn regenerate(&self, _req: RegenerateRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
        unsupported(REGENERATE_OP)
    }

    /// Holdings this plugin declares, narrowed to `req.account` when set.
    fn consumers(&self, _req: ConsumersRequest) -> BoxFuture<'_, Result<Vec<ConsumerDecl>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn apply_consumer(&self, _req: ConsumerApplyRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
        unsupported(APPLY_CONSUMER_OP)
    }

    fn verify_consumer(&self, _req: ConsumerVerifyRequest) -> BoxFuture<'_, Result<VerifyOutcome>> {
        unsupported(VERIFY_CONSUMER_OP)
    }
}

fn unsupported<T>(op: &'static str) -> BoxFuture<'static, Result<T>> {
    Box::pin(async move { Err(anyhow::Error::new(Unsupported { op: op.to_string() })) })
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn AccountProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

pub fn register_provider(provider: Arc<dyn AccountProvider>) {
    let mut g = GLOBAL.write().expect("account registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

pub fn providers() -> Vec<Arc<dyn AccountProvider>> {
    GLOBAL.read().expect("account registry poisoned").clone()
}

pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("account registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

/// Decides whether provider `name` manages unit `unit_id`. A holding counts
/// only when its declaring provider manages `consumer_unit_id`; otherwise any
/// plugin could claim a setting on a unit it does not own and receive the
/// rotated value. Core installs the unit→manager check.
pub type ConsumerAuthorizer = Arc<dyn Fn(&str, &Id) -> bool + Send + Sync>;

static AUTHORIZER: LazyLock<RwLock<Option<ConsumerAuthorizer>>> =
    LazyLock::new(|| RwLock::new(None));

pub fn set_consumer_authorizer(authorizer: Option<ConsumerAuthorizer>) {
    *AUTHORIZER.write().expect("account authorizer poisoned") = authorizer;
}

/// Holdings of one account, each paired with the declaring provider's name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConsumerSet {
    pub held: Vec<(String, ConsumerDecl)>,
    pub rejected: Vec<(String, ConsumerDecl)>,
}

/// Every holding of `account` across all registered providers, paired with
/// the declaring provider's name. With no [`ConsumerAuthorizer`] installed
/// this fails closed. Holdings on units the provider does not manage are kept
/// out of rotation and returned in [`ConsumerSet::rejected`] so core can
/// report them as drift; they take no part in conflict detection, so a rogue
/// claim cannot block rotation. Holdings are declared at call time, not at
/// registration, so the one-declaration-per-`(consumer_unit_id, setting)`
/// rule is enforced here, across all accounts: any two differing claims on
/// one setting are an error, since rotation could not know which value or
/// provider applies.
pub async fn consumers_of(account: &AccountRef) -> Result<ConsumerSet> {
    let authorizer = AUTHORIZER
        .read()
        .expect("account authorizer poisoned")
        .clone();
    consumers_in(&providers(), authorizer.as_ref(), account).await
}

/// [`consumers_of`] over an explicit provider set and authorizer.
pub async fn consumers_in(
    providers: &[Arc<dyn AccountProvider>],
    authorizer: Option<&ConsumerAuthorizer>,
    account: &AccountRef,
) -> Result<ConsumerSet> {
    // Every holding is fetched, not just this account's, so one setting
    // claimed for two different accounts is caught.
    let req = ConsumersRequest { account: None };
    let authorize =
        authorizer.ok_or_else(|| anyhow::anyhow!("no account consumer authorizer installed"))?;
    let mut out: Vec<(String, ConsumerDecl)> = Vec::new();
    let mut rejected: Vec<(String, ConsumerDecl)> = Vec::new();
    for p in providers {
        let declared = p.consumers(req.clone()).await.map_err(|e| {
            anyhow::Error::new(ConsumerDiscoveryFailed {
                provider: p.name().to_string(),
                cause: format!("{e:#}"),
            })
        })?;
        for held in declared {
            if !authorize(p.name(), &held.consumer_unit_id) {
                if &held.account == account {
                    rejected.push((p.name().to_string(), held));
                }
                continue;
            }
            if let Some((owner, prior)) = out.iter().find(|(_, d)| {
                d.consumer_unit_id == held.consumer_unit_id && d.setting == held.setting
            }) {
                if owner == p.name() && *prior == held {
                    continue;
                }
                let why = if owner != p.name() {
                    format!("declared by both '{owner}' and '{}'", p.name())
                } else if prior.account != held.account {
                    format!("claimed for two accounts by '{owner}'")
                } else {
                    format!("declared twice with different capabilities by '{owner}'")
                };
                anyhow::bail!(
                    "setting '{}' on unit {} is {why}",
                    held.setting,
                    held.consumer_unit_id
                );
            }
            out.push((p.name().to_string(), held));
        }
    }
    out.retain(|(_, d)| &d.account == account);
    Ok(ConsumerSet {
        held: out,
        rejected,
    })
}

#[cfg(feature = "in-process")]
pub type InvokeThunk = Arc<
    dyn Fn(
            &str,
            serde_json::Value,
            Option<crate::invocation_scope::BackendCall>,
        ) -> std::result::Result<serde_json::Value, serde_json::Value>
        + Send
        + Sync
        + 'static,
>;

pub const DECL_OP: &str = "decl";
pub const LIST_OP: &str = "list";
pub const DETAIL_OP: &str = "detail";
pub const CREATE_OP: &str = "create";
pub const UPDATE_OP: &str = "update";
pub const DELETE_OP: &str = "delete";
pub const SET_SECRET_OP: &str = "set_secret";
pub const VERIFY_OP: &str = "verify";
pub const REGENERATE_OP: &str = "regenerate";
pub const CONSUMERS_OP: &str = "consumers";
pub const APPLY_CONSUMER_OP: &str = "apply_consumer";
pub const VERIFY_CONSUMER_OP: &str = "verify_consumer";

/// A dry run never carries plaintext: `secret` is dropped from the args on
/// both sides of the wire. A real `set_secret`/`apply_consumer` without one is
/// refused before it reaches the provider.
fn gate_dry_run_secret(op: &str, args: &mut serde_json::Value) -> std::result::Result<(), String> {
    let dry_run = args
        .get("dry_run")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let rollback = args
        .get("rollback")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if dry_run && rollback {
        return Err(format!("a rollback cannot be a dry run ({op})"));
    }
    if dry_run {
        if let Some(obj) = args.as_object_mut() {
            obj.remove("secret");
        }
    } else if matches!(op, SET_SECRET_OP | APPLY_CONSUMER_OP)
        && args.get("secret").is_none_or(serde_json::Value::is_null)
    {
        return Err(format!("{op} requires a secret unless dry_run is set"));
    }
    if args
        .get("secret")
        .is_some_and(|v| !v.is_string() && !v.is_null())
    {
        return Err(format!("secret must be a string ({op})"));
    }
    if matches!(op, SET_SECRET_OP | APPLY_CONSUMER_OP | CREATE_OP)
        && args
            .get("secret")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|v| !scrubbable(v))
    {
        return Err(SHORT_SECRET.replace("{op}", op));
    }
    if op == REGENERATE_OP {
        let min_len = requested_min_len(args);
        if let Some(obj) = args.as_object_mut() {
            obj.insert("min_len".into(), min_len.into());
        }
    }
    Ok(())
}

const SHORT_SECRET: &str = "secret is shorter than the 4-character minimum ({op})";
const ROTATED_KEY: &str = "rotated_but_rejected";

fn requested_min_len(args: &serde_json::Value) -> usize {
    args.get("min_len")
        .and_then(serde_json::Value::as_u64)
        .map_or(MIN_SECRET_LEN, |n| (n as usize).max(MIN_SECRET_LEN))
}

/// A non-string `generated` counts as too short: it cannot be stored as a
/// secret, and its length is not a character count.
fn generated_too_short(out: &serde_json::Value, min_len: usize) -> bool {
    match out.get("generated") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::String(v)) => v.chars().count() < min_len,
        Some(_) => true,
    }
}

/// Every rendering of `v[key]` an error message could echo, so a provider or
/// decode error that repeats the value is redacted. A non-string value (a
/// number, a nested object) is covered too: serde quotes scalars verbatim.
fn secret_values(v: &serde_json::Value, key: &str) -> Vec<String> {
    fn leaves(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Null => {}
            serde_json::Value::String(s) => out.push(s.clone()),
            serde_json::Value::Array(a) => a.iter().for_each(|x| leaves(x, out)),
            serde_json::Value::Object(o) => o.values().for_each(|x| leaves(x, out)),
            other => out.push(other.to_string()),
        }
    }
    let mut out = Vec::new();
    if let Some(found) = v.get(key) {
        leaves(found, &mut out);
        if !found.is_string() && !found.is_null() {
            out.push(found.to_string());
        }
    }
    out
}

/// Key-name redaction misses a value echoed under an innocuous key
/// (`{"msg": "set key to <value>"}`), so every string leaf is also checked
/// against the request secret and any `generated` value. The top-level
/// `generated` field itself is the one place the value is meant to travel.
fn scrub_known_values(out: &mut serde_json::Value, values: &[String]) {
    fn walk(v: &mut serde_json::Value, values: &[String]) {
        match v {
            serde_json::Value::String(s) if mentions(s, values) => {
                *s = scrubbed(s, values);
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(|x| walk(x, values)),
            serde_json::Value::Object(o) => o.values_mut().for_each(|x| walk(x, values)),
            _ => {}
        }
    }
    match out {
        serde_json::Value::Object(o) => o
            .iter_mut()
            .filter(|(k, _)| k.as_str() != "generated")
            .for_each(|(_, v)| walk(v, values)),
        other => walk(other, values),
    }
}

/// Below this many characters a value matches too much ordinary text to be
/// worth redacting by substring; pattern scrubbing still applies.
pub const MIN_SECRET_LEN: usize = 4;

fn scrubbable(v: &str) -> bool {
    v.chars().count() >= MIN_SECRET_LEN
}

/// Long values are redacted wherever they appear. A short value is redacted
/// only as the whole text or a whole word, so it cannot leak by being echoed
/// alone yet does not blank every word that happens to contain it.
fn scrubbed(msg: &str, values: &[String]) -> String {
    let (long, short): (Vec<&str>, Vec<&str>) = values
        .iter()
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .partition(|v| scrubbable(v));
    // Long first: redacting a short value inside a long one would break the
    // long value's match and leave the rest of it in the text.
    let mut text = long.iter().fold(msg.to_string(), |acc, v| {
        acc.replace(v, utils::scrub::REDACTED)
    });
    for v in short {
        text = redact_whole_words(&text, v);
    }
    utils::scrub::redact_values(&text, &[])
}

fn redact_whole_words(text: &str, v: &str) -> String {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut prev: Option<char> = None;
    while let Some(i) = rest.find(v) {
        let before = rest[..i].chars().next_back().or(prev);
        let after = rest[i + v.len()..].chars().next();
        out.push_str(&rest[..i]);
        if before.is_none_or(|c| !is_word(c)) && after.is_none_or(|c| !is_word(c)) {
            out.push_str(utils::scrub::REDACTED);
        } else {
            out.push_str(v);
        }
        prev = v.chars().next_back();
        rest = &rest[i + v.len()..];
    }
    out.push_str(rest);
    out
}

fn mentions(s: &str, values: &[String]) -> bool {
    values
        .iter()
        .any(|x| !x.is_empty() && s.contains(x.as_str()))
}

const UNSUPPORTED_KEY: &str = "unsupported";
const ROLLBACK_UNREPORTED: &str = "rollback outcome must report `restored`";

fn is_rollback(op: &str, flag: &Option<serde_json::Value>) -> bool {
    matches!(op, SET_SECRET_OP | APPLY_CONSUMER_OP)
        && flag.as_ref().and_then(serde_json::Value::as_bool) == Some(true)
}

#[cfg(feature = "in-process")]
fn require_restored(rollback: bool, out: AccountOutcome) -> Result<AccountOutcome> {
    if rollback && out.restored.is_none() {
        anyhow::bail!(ROLLBACK_UNREPORTED);
    }
    Ok(out)
}

enum Fail {
    Msg(String),
    Unsupported(String),
}

impl From<String> for Fail {
    fn from(msg: String) -> Self {
        Fail::Msg(msg)
    }
}

fn fail(e: anyhow::Error) -> Fail {
    match e.downcast_ref::<Unsupported>() {
        Some(u) => Fail::Unsupported(u.op.clone()),
        None => Fail::Msg(format!("{e:#}")),
    }
}

pub async fn dispatch_op(
    provider: &dyn AccountProvider,
    op: &str,
    mut args: serde_json::Value,
) -> std::result::Result<serde_json::Value, serde_json::Value> {
    let secret = secret_values(&args, "secret");
    let err = |f: Fail| match f {
        Fail::Msg(msg) => serde_json::Value::String(scrubbed(&msg, &secret)),
        Fail::Unsupported(other) if other == op => serde_json::json!({ UNSUPPORTED_KEY: other }),
        Fail::Unsupported(other) => {
            serde_json::Value::String(scrubbed(&Unsupported { op: other }.to_string(), &secret))
        }
    };
    gate_dry_run_secret(op, &mut args).map_err(|m| err(Fail::Msg(m)))?;
    let min_len = requested_min_len(&args);
    let account_arg = args.get("account").cloned();
    let rollback = args.get("rollback").cloned();
    fn dec<T: serde::de::DeserializeOwned>(
        op: &str,
        args: serde_json::Value,
    ) -> std::result::Result<T, String> {
        serde_json::from_value(args).map_err(|e| format!("decode {op} args: {e}"))
    }
    fn enc<T: Serialize>(out: &T) -> std::result::Result<serde_json::Value, Fail> {
        serde_json::to_value(out).map_err(|e| Fail::Msg(e.to_string()))
    }
    let out = async {
        match op {
            DECL_OP => {
                let decl = provider.decl().await.map_err(fail)?;
                decl.validate()?;
                enc(&decl)
            }
            LIST_OP => enc(&provider.list(dec(op, args)?).await.map_err(fail)?),
            DETAIL_OP => enc(&provider.detail(dec(op, args)?).await.map_err(fail)?),
            CREATE_OP => enc(&provider.create(dec(op, args)?).await.map_err(fail)?),
            UPDATE_OP => enc(&provider.update(dec(op, args)?).await.map_err(fail)?),
            DELETE_OP => enc(&provider.delete(dec(op, args)?).await.map_err(fail)?),
            SET_SECRET_OP => enc(&provider.set_secret(dec(op, args)?).await.map_err(fail)?),
            VERIFY_OP => enc(&provider.verify(dec(op, args)?).await.map_err(fail)?),
            REGENERATE_OP => enc(&provider.regenerate(dec(op, args)?).await.map_err(fail)?),
            CONSUMERS_OP => enc(&provider.consumers(dec(op, args)?).await.map_err(fail)?),
            APPLY_CONSUMER_OP => enc(&provider
                .apply_consumer(dec(op, args)?)
                .await
                .map_err(fail)?),
            VERIFY_CONSUMER_OP => enc(&provider
                .verify_consumer(dec(op, args)?)
                .await
                .map_err(fail)?),
            other => Err(Fail::Msg(format!("unknown account op: {other}"))),
        }
    }
    .await;
    let mut out = out.map_err(err)?;
    if op == REGENERATE_OP && generated_too_short(&out, min_len) {
        return Err(serde_json::json!({ ROTATED_KEY: {
            "account": account_arg,
            "generated": out.get("generated"),
        }}));
    }
    if is_rollback(op, &rollback) && out.get("restored").is_none_or(serde_json::Value::is_null) {
        return Err(err(Fail::Msg(ROLLBACK_UNREPORTED.to_string())));
    }
    let mut known = secret.clone();
    known.extend(secret_values(&out, "generated"));
    scrub_known_values(&mut out, &known);
    Ok(out)
}

#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_provider(Arc::new(AccountProxy { name, invoke }));
    Ok(())
}

#[cfg(feature = "in-process")]
struct AccountProxy {
    name: String,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
fn redact_outcome(mut out: AccountOutcome) -> AccountOutcome {
    utils::scrub::redact_json_in_place(&mut out.detail);
    out
}

#[cfg(feature = "in-process")]
fn redact_verify(mut out: VerifyOutcome) -> VerifyOutcome {
    utils::scrub::redact_json_in_place(&mut out.detail);
    out
}

#[cfg(feature = "in-process")]
fn rotated_but_rejected(v: &serde_json::Value) -> Option<anyhow::Error> {
    let account: AccountRef = serde_json::from_value(v.get("account")?.clone()).ok()?;
    let generated = v.get("generated")?.as_str()?.to_string();
    Some(anyhow::Error::new(RotatedButRejected {
        account,
        generated: Secret::new(generated),
    }))
}

#[cfg(feature = "in-process")]
impl AccountProxy {
    fn call<T: for<'de> Deserialize<'de>>(
        &self,
        op: &'static str,
        args: serde_json::Result<serde_json::Value>,
    ) -> BoxFuture<'_, Result<T>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let mut args = args.map_err(|e| {
                anyhow::anyhow!(scrubbed(
                    &format!("account '{name}' {op} encode args: {e}"),
                    &[]
                ))
            })?;
            gate_dry_run_secret(op, &mut args)
                .map_err(|e| anyhow::anyhow!(scrubbed(&format!("account '{name}' {e}"), &[])))?;
            let secret = secret_values(&args, "secret");
            let scrub = |msg: String| anyhow::anyhow!(scrubbed(&msg, &secret));
            let call = crate::invocation_scope::capture("account", &name);
            let out = tokio::task::spawn_blocking(move || invoke(op, args, call))
                .await
                .map_err(|e| scrub(format!("account '{name}' {op} task panicked: {e}")))?
                .map_err(|e| match e.get(UNSUPPORTED_KEY).and_then(|v| v.as_str()) {
                    Some(returned) if returned == op => {
                        anyhow::Error::new(Unsupported { op: op.to_string() })
                    }
                    _ if op == REGENERATE_OP && e.get(ROTATED_KEY).is_some() => {
                        rotated_but_rejected(&e[ROTATED_KEY]).unwrap_or_else(|| {
                            anyhow::anyhow!(
                                "account '{name}' regenerate rotated the unit but returned a \
                                 malformed rejection"
                            )
                        })
                    }
                    _ => {
                        let mut known = secret.clone();
                        known.extend(secret_values(&e, "generated"));
                        anyhow::anyhow!(scrubbed(
                            &format!(
                                "account '{name}' {op} failed: {}",
                                crate::render_invoke_error(&e)
                            ),
                            &known
                        ))
                    }
                })?;
            let mut out = out;
            let mut leaked = secret.clone();
            leaked.extend(secret_values(&out, "generated"));
            scrub_known_values(&mut out, &leaked);
            serde_json::from_value(out).map_err(|e| {
                anyhow::anyhow!(scrubbed(
                    &format!("account '{name}' {op} returned invalid JSON: {e}"),
                    &leaked
                ))
            })
        })
    }
}

#[cfg(feature = "in-process")]
impl AccountProvider for AccountProxy {
    fn name(&self) -> &str {
        &self.name
    }

    fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>> {
        let name = self.name.clone();
        let fut = self.call::<AccountDecl>(DECL_OP, Ok(serde_json::json!({})));
        Box::pin(async move {
            let decl = fut.await?;
            decl.validate()
                .map_err(|e| anyhow::anyhow!("account '{name}' decl invalid: {e}"))?;
            Ok(decl)
        })
    }

    fn list(&self, req: ListRequest) -> BoxFuture<'_, Result<Vec<AccountInfo>>> {
        self.call(LIST_OP, serde_json::to_value(&req))
    }

    fn detail(&self, req: DetailRequest) -> BoxFuture<'_, Result<Option<AccountInfo>>> {
        self.call(DETAIL_OP, serde_json::to_value(&req))
    }

    fn create(&self, req: CreateRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
        let fut = self.call(CREATE_OP, serde_json::to_value(&req));
        Box::pin(async move { fut.await.map(redact_outcome) })
    }

    fn update(&self, req: UpdateRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
        let fut = self.call(UPDATE_OP, serde_json::to_value(&req));
        Box::pin(async move { fut.await.map(redact_outcome) })
    }

    fn delete(&self, req: DeleteRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
        let fut = self.call(DELETE_OP, serde_json::to_value(&req));
        Box::pin(async move { fut.await.map(redact_outcome) })
    }

    fn set_secret(&self, req: SetSecretRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
        let rollback = req.rollback;
        let fut = self.call(SET_SECRET_OP, serde_json::to_value(&req));
        Box::pin(async move { require_restored(rollback, redact_outcome(fut.await?)) })
    }

    fn verify(&self, req: VerifyRequest) -> BoxFuture<'_, Result<VerifyOutcome>> {
        let fut = self.call(VERIFY_OP, serde_json::to_value(&req));
        Box::pin(async move { fut.await.map(redact_verify) })
    }

    fn regenerate(&self, req: RegenerateRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
        let account = req.account.clone();
        let min_len = req.min_len.max(MIN_SECRET_LEN);
        let fut = self.call(REGENERATE_OP, serde_json::to_value(&req));
        Box::pin(async move {
            let mut out = redact_outcome(fut.await?);
            if out
                .generated
                .as_ref()
                .is_some_and(|g| g.expose_secret().chars().count() < min_len)
            {
                return Err(anyhow::Error::new(RotatedButRejected {
                    account,
                    generated: out.generated.take().expect("checked above"),
                }));
            }
            Ok(out)
        })
    }

    fn consumers(&self, req: ConsumersRequest) -> BoxFuture<'_, Result<Vec<ConsumerDecl>>> {
        self.call(CONSUMERS_OP, serde_json::to_value(&req))
    }

    fn apply_consumer(&self, req: ConsumerApplyRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
        let rollback = req.rollback;
        let fut = self.call(APPLY_CONSUMER_OP, serde_json::to_value(&req));
        Box::pin(async move { require_restored(rollback, redact_outcome(fut.await?)) })
    }

    fn verify_consumer(&self, req: ConsumerVerifyRequest) -> BoxFuture<'_, Result<VerifyOutcome>> {
        let fut = self.call(VERIFY_CONSUMER_OP, serde_json::to_value(&req));
        Box::pin(async move { fut.await.map(redact_verify) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "0192f0a0-0000-7000-8000-000000000001";
    const CONSUMER: &str = "0192f0a0-0000-7000-8000-000000000002";
    const VALUE: &str = "s3cr3t-rotated-value-0123456789";

    fn id(s: &str) -> Id {
        s.parse().unwrap()
    }

    fn nm(s: &str) -> NonEmpty {
        s.parse().unwrap()
    }

    fn acct() -> AccountRef {
        AccountRef {
            unit_id: id(OWNER),
            account: nm("api"),
        }
    }

    fn holding() -> ConsumerDecl {
        ConsumerDecl {
            account: acct(),
            consumer_unit_id: id(CONSUMER),
            setting: nm("prowlarr_api_key"),
            capabilities: ConsumerCapabilities {
                apply: true,
                verify: true,
                rollback: true,
            },
        }
    }

    fn info(unit_id: Id) -> AccountInfo {
        AccountInfo {
            unit_id,
            name: nm("api"),
            kind: AccountKind::ApiKey,
            roles: vec![],
            enabled: true,
            managed: true,
        }
    }

    struct FakeOwner {
        name: String,
    }

    impl AccountProvider for FakeOwner {
        fn name(&self) -> &str {
            &self.name
        }
        fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>> {
            Box::pin(async {
                Ok(AccountDecl {
                    unit_kinds: vec!["prowlarr".into()],
                    account_kinds: vec![AccountKind::ApiKey],
                    capabilities: vec![],
                })
            })
        }
        fn list(&self, req: ListRequest) -> BoxFuture<'_, Result<Vec<AccountInfo>>> {
            Box::pin(async move {
                Ok(vec![info(req.unit_id)]
                    .into_iter()
                    .filter(|a| req.kind.is_none_or(|k| a.kind == k))
                    .collect())
            })
        }
        fn detail(&self, req: DetailRequest) -> BoxFuture<'_, Result<Option<AccountInfo>>> {
            Box::pin(async move {
                Ok((req.account.account == "api").then(|| info(req.account.unit_id)))
            })
        }
        fn create(&self, req: CreateRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async move {
                Ok(AccountOutcome {
                    executed: !req.dry_run,
                    detail: serde_json::json!({"has_secret": req.secret.is_some()}),
                    generated: None,
                    restored: None,
                })
            })
        }
        fn update(&self, req: UpdateRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async move {
                Ok(AccountOutcome {
                    executed: !req.dry_run,
                    detail: serde_json::Value::Null,
                    generated: None,
                    restored: None,
                })
            })
        }
        fn delete(&self, req: DeleteRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async move {
                Ok(AccountOutcome {
                    executed: !req.dry_run,
                    detail: serde_json::Value::Null,
                    generated: None,
                    restored: None,
                })
            })
        }
        fn set_secret(&self, req: SetSecretRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async move {
                if req.account.account == "boom" {
                    anyhow::bail!(
                        "upstream rejected {}",
                        req.secret.as_ref().map_or("", |s| s.expose_secret())
                    );
                }
                Ok(AccountOutcome {
                    executed: !req.dry_run,
                    detail: serde_json::Value::Null,
                    generated: None,
                    restored: None,
                })
            })
        }
        fn verify(&self, req: VerifyRequest) -> BoxFuture<'_, Result<VerifyOutcome>> {
            Box::pin(async move {
                Ok(VerifyOutcome {
                    matches: req.secret.expose_secret() == VALUE,
                    detail: serde_json::Value::Null,
                })
            })
        }
        fn regenerate(&self, req: RegenerateRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async move {
                Ok(AccountOutcome {
                    executed: !req.dry_run,
                    detail: serde_json::json!({"account": req.account.account}),
                    generated: (!req.dry_run).then(|| Secret::new(VALUE.to_string())),
                    restored: None,
                })
            })
        }
    }

    struct FakeConsumer;

    impl AccountProvider for FakeConsumer {
        fn name(&self) -> &str {
            "account-fake-consumer"
        }
        fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>> {
            Box::pin(async {
                Ok(AccountDecl {
                    unit_kinds: vec![],
                    account_kinds: vec![],
                    capabilities: vec![],
                })
            })
        }
        fn consumers(&self, req: ConsumersRequest) -> BoxFuture<'_, Result<Vec<ConsumerDecl>>> {
            Box::pin(async move {
                Ok(vec![holding()]
                    .into_iter()
                    .filter(|h| req.account.as_ref().is_none_or(|a| &h.account == a))
                    .collect())
            })
        }
        fn apply_consumer(
            &self,
            req: ConsumerApplyRequest,
        ) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async move {
                Ok(AccountOutcome {
                    executed: !req.dry_run,
                    detail: serde_json::json!({"setting": req.holding.setting}),
                    generated: None,
                    restored: req.rollback.then_some(true),
                })
            })
        }
        fn verify_consumer(
            &self,
            req: ConsumerVerifyRequest,
        ) -> BoxFuture<'_, Result<VerifyOutcome>> {
            Box::pin(async move {
                Ok(VerifyOutcome {
                    matches: req.secret.expose_secret() == VALUE,
                    detail: serde_json::Value::Null,
                })
            })
        }
    }

    fn owner() -> FakeOwner {
        FakeOwner {
            name: "account-fake-owner".into(),
        }
    }

    fn secret() -> Secret<String> {
        Secret::new(VALUE.to_string())
    }

    fn find(name: &str) -> Option<Arc<dyn AccountProvider>> {
        providers().into_iter().find(|p| p.name() == name)
    }

    #[tokio::test]
    async fn account_register_replace_deregister() {
        let _guard = GLOBALS.lock().await;
        register_provider(Arc::new(FakeOwner {
            name: "account-test".into(),
        }));
        register_provider(Arc::new(FakeOwner {
            name: "account-test".into(),
        }));
        assert_eq!(
            providers()
                .iter()
                .filter(|p| p.name() == "account-test")
                .count(),
            1
        );
        let p = find("account-test").expect("registered");
        assert_eq!(
            p.decl().await.unwrap().account_kinds,
            vec![AccountKind::ApiKey]
        );
        assert!(deregister_provider("account-test"));
        assert!(find("account-test").is_none());
        assert!(!deregister_provider("account-test"));
    }

    #[test]
    fn account_kind_wire_names() {
        for (kind, wire) in [
            (AccountKind::OsUser, "os_user"),
            (AccountKind::AppUser, "app_user"),
            (AccountKind::ApiKey, "api_key"),
            (AccountKind::ServiceToken, "service_token"),
            (AccountKind::SshKey, "ssh_key"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::json!(wire));
            let back: AccountKind = serde_json::from_value(serde_json::json!(wire)).unwrap();
            assert_eq!(back, kind);
        }
        let future: AccountKind = serde_json::from_value(serde_json::json!("passkey")).unwrap();
        assert_eq!(future, AccountKind::Unknown);
    }

    #[test]
    fn account_refs_require_uuids() {
        assert!(
            serde_json::from_value::<AccountRef>(
                serde_json::json!({"unit_id": "prowlarr", "account": "api"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<ConsumerDecl>(serde_json::json!({
                "account": {"unit_id": OWNER, "account": "api"},
                "consumer_unit_id": "mylar3",
                "setting": "k",
            }))
            .is_err()
        );
        let info: AccountInfo = serde_json::from_value(serde_json::json!({
            "unit_id": OWNER, "name": "api", "kind": "api_key",
        }))
        .unwrap();
        assert!(info.enabled);
        assert!(!info.managed);
        assert!(info.roles.is_empty());
    }

    #[test]
    fn account_secret_never_debug_printed() {
        let req = SetSecretRequest {
            account: acct(),
            secret: Some(secret()),
            dry_run: false,
            rollback: false,
        };
        let dbg = format!("{req:?}");
        assert!(!dbg.contains(VALUE), "{dbg}");
        let apply = ConsumerApplyRequest {
            holding: holding(),
            secret: Some(secret()),
            dry_run: false,
            rollback: false,
        };
        assert!(!format!("{apply:?}").contains(VALUE));
        let create = CreateRequest {
            account: acct(),
            kind: AccountKind::ApiKey,
            roles: vec![],
            secret: Some(secret()),
            dry_run: false,
        };
        assert!(!format!("{create:?}").contains(VALUE));
    }

    #[test]
    fn account_secret_crosses_the_wire_only_in_its_field() {
        let wire = serde_json::to_value(SetSecretRequest {
            account: acct(),
            secret: Some(secret()),
            dry_run: false,
            rollback: false,
        })
        .unwrap();
        assert_eq!(wire["secret"], VALUE);
        let back: SetSecretRequest = serde_json::from_value(wire).unwrap();
        assert_eq!(back.secret.unwrap().expose_secret(), VALUE);

        let no_secret = serde_json::to_value(CreateRequest {
            account: acct(),
            kind: AccountKind::OsUser,
            roles: vec!["sudo".into()],
            secret: None,
            dry_run: false,
        })
        .unwrap();
        assert!(no_secret.get("secret").is_none());
    }

    #[tokio::test]
    async fn account_dispatch_op_routes_every_op() {
        let p = owner();
        let a = serde_json::to_value(acct()).unwrap();
        let decl = dispatch_op(&p, DECL_OP, serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(decl["account_kinds"][0], "api_key");
        let list = dispatch_op(&p, LIST_OP, serde_json::json!({"unit_id": OWNER}))
            .await
            .unwrap();
        assert_eq!(list[0]["name"], "api");
        let created = dispatch_op(
            &p,
            CREATE_OP,
            serde_json::json!({"account": a, "kind": "api_key", "secret": VALUE}),
        )
        .await
        .unwrap();
        assert_eq!(created["detail"]["has_secret"], true);
        let updated = dispatch_op(
            &p,
            UPDATE_OP,
            serde_json::json!({"account": a, "enabled": false, "dry_run": true}),
        )
        .await
        .unwrap();
        assert_eq!(updated["executed"], false);
        let deleted = dispatch_op(&p, DELETE_OP, serde_json::json!({"account": a}))
            .await
            .unwrap();
        assert_eq!(deleted["executed"], true);
        let set = dispatch_op(
            &p,
            SET_SECRET_OP,
            serde_json::json!({"account": a, "secret": VALUE}),
        )
        .await
        .unwrap();
        assert_eq!(set["executed"], true);
        let verified = dispatch_op(
            &p,
            VERIFY_OP,
            serde_json::json!({"account": a, "secret": VALUE}),
        )
        .await
        .unwrap();
        assert_eq!(verified["matches"], true);
        let none = dispatch_op(&p, CONSUMERS_OP, serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(none, serde_json::json!([]));
        let err = dispatch_op(
            &p,
            APPLY_CONSUMER_OP,
            serde_json::json!({"holding": holding(), "secret": VALUE}),
        )
        .await
        .unwrap_err();
        assert_eq!(err, serde_json::json!({"unsupported": "apply_consumer"}));
    }

    #[tokio::test]
    async fn account_consumer_ops_route_and_owner_ops_default_unsupported() {
        let c = FakeConsumer;
        let held = dispatch_op(&c, CONSUMERS_OP, serde_json::json!({}))
            .await
            .unwrap();
        let held: Vec<ConsumerDecl> = serde_json::from_value(held).unwrap();
        assert_eq!(held, vec![holding()]);
        let h = serde_json::to_value(holding()).unwrap();
        let applied = dispatch_op(
            &c,
            APPLY_CONSUMER_OP,
            serde_json::json!({"holding": h, "secret": VALUE, "dry_run": true}),
        )
        .await
        .unwrap();
        assert_eq!(applied["executed"], false);
        assert_eq!(applied["detail"]["setting"], "prowlarr_api_key");
        let verified = dispatch_op(
            &c,
            VERIFY_CONSUMER_OP,
            serde_json::json!({"holding": h, "secret": "stale"}),
        )
        .await
        .unwrap();
        assert_eq!(verified["matches"], false);
        let err = dispatch_op(&c, LIST_OP, serde_json::json!({"unit_id": OWNER}))
            .await
            .unwrap_err();
        assert_eq!(err, serde_json::json!({"unsupported": "list"}));
    }

    #[tokio::test]
    async fn account_dispatch_op_scrubs_echoed_secret() {
        let err = dispatch_op(
            &owner(),
            SET_SECRET_OP,
            serde_json::json!({
                "account": {"unit_id": OWNER, "account": "boom"},
                "secret": VALUE,
            }),
        )
        .await
        .unwrap_err();
        let msg = err.as_str().unwrap();
        assert!(!msg.contains(VALUE), "{msg}");
        assert!(msg.contains("upstream rejected"), "{msg}");
    }

    #[tokio::test]
    async fn account_dispatch_op_unknown_and_decode_errors() {
        let err = dispatch_op(&owner(), "explode", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            err.as_str()
                .unwrap()
                .contains("unknown account op: explode")
        );
        let err = dispatch_op(&owner(), LIST_OP, serde_json::json!({"unit_id": "loki"}))
            .await
            .unwrap_err();
        assert!(err.as_str().unwrap().starts_with("decode list args:"));
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_proxy_sends_every_op_and_scrubs() {
        let calls: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let rec = calls.clone();
        let invoke: InvokeThunk = Arc::new(move |op, args, _call| {
            rec.lock().unwrap().push((op.to_string(), args.clone()));
            match op {
                DECL_OP => Ok(serde_json::json!({})),
                LIST_OP | CONSUMERS_OP => Ok(serde_json::json!([])),
                DETAIL_OP => Ok(serde_json::Value::Null),
                REGENERATE_OP => Ok(serde_json::json!({"executed": true, "generated": VALUE})),
                VERIFY_OP | VERIFY_CONSUMER_OP => Ok(serde_json::json!({"matches": true})),
                SET_SECRET_OP => Err(serde_json::json!(format!(
                    "bad key {}",
                    args["secret"].as_str().unwrap()
                ))),
                _ => Ok(serde_json::json!({"executed": true, "detail": {"password": "leak"}})),
            }
        });
        let proxy = AccountProxy {
            name: "account-proxy".into(),
            invoke,
        };

        proxy.decl().await.unwrap();
        assert!(
            proxy
                .list(ListRequest {
                    unit_id: id(OWNER),
                    kind: None,
                })
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            proxy
                .detail(DetailRequest { account: acct() })
                .await
                .unwrap()
                .is_none()
        );
        let created = proxy
            .create(CreateRequest {
                account: acct(),
                kind: AccountKind::AppUser,
                roles: vec![],
                secret: None,
                dry_run: false,
            })
            .await
            .unwrap();
        assert_eq!(created.detail["password"], utils::scrub::REDACTED);
        proxy
            .update(UpdateRequest {
                account: acct(),
                roles: None,
                enabled: Some(true),
                dry_run: false,
            })
            .await
            .unwrap();
        proxy
            .delete(DeleteRequest {
                account: acct(),
                dry_run: true,
            })
            .await
            .unwrap();
        let e = proxy
            .set_secret(SetSecretRequest {
                account: acct(),
                secret: Some(secret()),
                dry_run: false,
                rollback: false,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(!e.contains(VALUE), "{e}");
        assert!(
            proxy
                .verify(VerifyRequest {
                    account: acct(),
                    secret: secret(),
                })
                .await
                .unwrap()
                .matches
        );
        let regen = proxy
            .regenerate(RegenerateRequest {
                account: acct(),
                dry_run: false,
                min_len: MIN_SECRET_LEN,
            })
            .await
            .unwrap();
        assert_eq!(regen.generated.unwrap().expose_secret(), VALUE);
        assert!(
            proxy
                .consumers(ConsumersRequest::default())
                .await
                .unwrap()
                .is_empty()
        );
        proxy
            .apply_consumer(ConsumerApplyRequest {
                holding: holding(),
                secret: Some(secret()),
                dry_run: false,
                rollback: false,
            })
            .await
            .unwrap();
        proxy
            .verify_consumer(ConsumerVerifyRequest {
                holding: holding(),
                secret: secret(),
            })
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        let ops: Vec<&str> = calls.iter().map(|(op, _)| op.as_str()).collect();
        assert_eq!(
            ops,
            vec![
                "decl",
                "list",
                "detail",
                "create",
                "update",
                "delete",
                "set_secret",
                "verify",
                "regenerate",
                "consumers",
                "apply_consumer",
                "verify_consumer",
            ]
        );
        assert_eq!(calls[6].1["secret"], VALUE);
        assert_eq!(calls[10].1["holding"]["consumer_unit_id"], CONSUMER);
    }

    #[tokio::test]
    async fn account_regenerate_returns_generated_outside_detail() {
        let out = dispatch_op(
            &owner(),
            REGENERATE_OP,
            serde_json::json!({"account": acct()}),
        )
        .await
        .unwrap();
        assert_eq!(out["generated"], VALUE);
        assert!(!out["detail"].to_string().contains(VALUE));
        let back: AccountOutcome = serde_json::from_value(out).unwrap();
        assert_eq!(back.generated.as_ref().unwrap().expose_secret(), VALUE);
        assert!(!format!("{back:?}").contains(VALUE));

        let dry = dispatch_op(
            &owner(),
            REGENERATE_OP,
            serde_json::json!({"account": acct(), "dry_run": true}),
        )
        .await
        .unwrap();
        assert!(dry.get("generated").is_none());
    }

    struct Claimer {
        name: &'static str,
        holdings: Vec<ConsumerDecl>,
    }

    struct BrokenClaimer;

    impl AccountProvider for BrokenClaimer {
        fn name(&self) -> &str {
            "account-claim-broken"
        }
        fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>> {
            Box::pin(async {
                Ok(AccountDecl {
                    unit_kinds: vec![],
                    account_kinds: vec![],
                    capabilities: vec![],
                })
            })
        }
        fn consumers(&self, _req: ConsumersRequest) -> BoxFuture<'_, Result<Vec<ConsumerDecl>>> {
            Box::pin(async { anyhow::bail!("settings file unreadable") })
        }
    }

    impl AccountProvider for Claimer {
        fn name(&self) -> &str {
            self.name
        }
        fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>> {
            Box::pin(async {
                Ok(AccountDecl {
                    unit_kinds: vec![],
                    account_kinds: vec![],
                    capabilities: vec![],
                })
            })
        }
        fn consumers(&self, _req: ConsumersRequest) -> BoxFuture<'_, Result<Vec<ConsumerDecl>>> {
            let h = self.holdings.clone();
            Box::pin(async move { Ok(h) })
        }
    }

    #[tokio::test]
    async fn account_consumers_filter_by_account() {
        let other = AccountRef {
            unit_id: id(CONSUMER),
            account: nm("admin"),
        };
        let none = dispatch_op(
            &FakeConsumer,
            CONSUMERS_OP,
            serde_json::json!({"account": other}),
        )
        .await
        .unwrap();
        assert_eq!(none, serde_json::json!([]));
        let some = dispatch_op(
            &FakeConsumer,
            CONSUMERS_OP,
            serde_json::json!({"account": acct()}),
        )
        .await
        .unwrap();
        assert_eq!(some.as_array().unwrap().len(), 1);
    }

    /// Serializes the tests that touch the process-global registry or
    /// authorizer; everything else uses [`consumers_in`] with local state.
    static GLOBALS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn claim_fixtures() -> (AccountRef, ConsumerDecl, ConsumerDecl) {
        let unit = AccountRef {
            unit_id: "0192f0a0-0000-7000-8000-0000000000a1".parse().unwrap(),
            account: nm("api"),
        };
        let held = ConsumerDecl {
            account: unit.clone(),
            consumer_unit_id: id(CONSUMER),
            setting: nm("key"),
            capabilities: ConsumerCapabilities::default(),
        };
        let stray = ConsumerDecl {
            account: AccountRef {
                unit_id: id(OWNER),
                account: nm("other"),
            },
            consumer_unit_id: id(CONSUMER),
            setting: nm("other_key"),
            capabilities: ConsumerCapabilities::default(),
        };
        (unit, held, stray)
    }

    fn deny_rogue() -> ConsumerAuthorizer {
        Arc::new(|name: &str, unit: &Id| name != "account-claim-rogue" && unit.as_str() == CONSUMER)
    }

    fn claimer(name: &'static str, holdings: Vec<ConsumerDecl>) -> Arc<dyn AccountProvider> {
        Arc::new(Claimer { name, holdings })
    }

    #[tokio::test]
    async fn account_consumers_in_enforces_one_declarer_per_setting() {
        let (unit, held, stray) = claim_fixtures();
        let a = claimer("account-claim-a", vec![held.clone(), stray]);
        let only_consumer: ConsumerAuthorizer =
            Arc::new(|_: &str, unit: &Id| unit.as_str() == CONSUMER);

        let err = consumers_in(std::slice::from_ref(&a), None, &unit)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no account consumer authorizer"), "{err}");

        let deny_a: ConsumerAuthorizer = Arc::new(|name: &str, _: &Id| name != "account-claim-a");
        let denied = consumers_in(std::slice::from_ref(&a), Some(&deny_a), &unit)
            .await
            .unwrap();
        assert!(denied.held.is_empty());
        assert_eq!(
            denied.rejected,
            vec![("account-claim-a".to_string(), held.clone())]
        );

        let rogue = claimer("account-claim-rogue", vec![held.clone()]);
        let mixed = consumers_in(&[a.clone(), rogue], Some(&deny_rogue()), &unit)
            .await
            .unwrap();
        assert_eq!(
            mixed.held,
            vec![("account-claim-a".to_string(), held.clone())]
        );
        assert_eq!(
            mixed.rejected,
            vec![("account-claim-rogue".to_string(), held.clone())]
        );

        let got = consumers_in(std::slice::from_ref(&a), Some(&only_consumer), &unit)
            .await
            .unwrap();
        assert_eq!(
            got.held,
            vec![("account-claim-a".to_string(), held.clone())]
        );
        assert!(got.rejected.is_empty());

        let twice = claimer("account-claim-a", vec![held.clone(), held.clone()]);
        let got = consumers_in(&[twice], Some(&only_consumer), &unit)
            .await
            .unwrap();
        assert_eq!(got.held.len(), 1, "{got:?}");

        let b = claimer("account-claim-b", vec![held]);
        let err = consumers_in(&[a.clone(), b], Some(&only_consumer), &unit)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("account-claim-a") && err.contains("account-claim-b"),
            "{err}"
        );

        let broken: Arc<dyn AccountProvider> = Arc::new(BrokenClaimer);
        let failed = consumers_in(&[a, broken], Some(&only_consumer), &unit)
            .await
            .unwrap_err();
        assert_eq!(
            failed
                .downcast_ref::<ConsumerDiscoveryFailed>()
                .map(|f| f.provider.as_str()),
            Some("account-claim-broken")
        );
        let err = format!("{failed:#}");
        assert!(
            err.contains("account-claim-broken") && err.contains("settings file unreadable"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn account_consumers_of_reads_the_global_registry_and_authorizer() {
        let _guard = GLOBALS.lock().await;
        let (unit, held, _) = claim_fixtures();
        register_provider(claimer("account-claim-global", vec![held.clone()]));
        set_consumer_authorizer(None);
        assert!(consumers_of(&unit).await.is_err());
        set_consumer_authorizer(Some(Arc::new(|_: &str, _: &Id| true)));
        let got = consumers_of(&unit).await.unwrap();
        assert_eq!(got.held, vec![("account-claim-global".to_string(), held)]);
        deregister_provider("account-claim-global");
        set_consumer_authorizer(None);
    }

    #[tokio::test]
    async fn account_list_kind_filter_and_detail() {
        let p = owner();
        let keys = dispatch_op(
            &p,
            LIST_OP,
            serde_json::json!({"unit_id": OWNER, "kind": "api_key"}),
        )
        .await
        .unwrap();
        assert_eq!(keys[0]["managed"], true);
        let users = dispatch_op(
            &p,
            LIST_OP,
            serde_json::json!({"unit_id": OWNER, "kind": "os_user"}),
        )
        .await
        .unwrap();
        assert_eq!(users, serde_json::json!([]));
        let found = dispatch_op(&p, DETAIL_OP, serde_json::json!({"account": acct()}))
            .await
            .unwrap();
        assert_eq!(found["name"], "api");
        let missing = dispatch_op(
            &p,
            DETAIL_OP,
            serde_json::json!({"account": {"unit_id": OWNER, "account": "nobody"}}),
        )
        .await
        .unwrap();
        assert!(missing.is_null());
    }

    #[test]
    fn account_blank_names_rejected() {
        for blank in ["", "  ", " api", "api ", "\tapi"] {
            assert!(
                serde_json::from_value::<AccountRef>(
                    serde_json::json!({"unit_id": OWNER, "account": blank})
                )
                .is_err()
            );
            assert!(
                serde_json::from_value::<ConsumerDecl>(serde_json::json!({
                    "account": {"unit_id": OWNER, "account": "api"},
                    "consumer_unit_id": CONSUMER,
                    "setting": blank,
                }))
                .is_err()
            );
            assert!(blank.parse::<NonEmpty>().is_err());
        }
        let ok: ConsumerDecl =
            serde_json::from_value(serde_json::to_value(holding()).unwrap()).unwrap();
        assert_eq!(ok.setting.as_str(), "prowlarr_api_key");
        assert!(
            serde_json::from_value::<AccountInfo>(
                serde_json::json!({"unit_id": OWNER, "name": " ", "kind": "api_key"})
            )
            .is_err()
        );
        let schema = serde_json::to_value(schemars::schema_for!(AccountRef)).unwrap();
        assert_eq!(schema["properties"]["account"]["minLength"], 1);
        let schema = serde_json::to_value(schemars::schema_for!(AccountInfo)).unwrap();
        assert_eq!(schema["properties"]["name"]["minLength"], 1);
    }

    struct SeesSecret;

    impl AccountProvider for SeesSecret {
        fn name(&self) -> &str {
            "account-sees-secret"
        }
        fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>> {
            Box::pin(async {
                Ok(AccountDecl {
                    unit_kinds: vec![],
                    account_kinds: vec![],
                    capabilities: vec![],
                })
            })
        }
        fn create(&self, req: CreateRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async move { seen(req.secret.is_some()) })
        }
        fn set_secret(&self, req: SetSecretRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async move { seen(req.secret.is_some()) })
        }
        fn apply_consumer(
            &self,
            req: ConsumerApplyRequest,
        ) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async move { seen(req.secret.is_some()) })
        }
    }

    fn seen(has_secret: bool) -> Result<AccountOutcome> {
        Ok(AccountOutcome {
            executed: false,
            detail: serde_json::json!({"has_secret": has_secret}),
            generated: None,
            restored: None,
        })
    }

    #[tokio::test]
    async fn account_dry_run_never_delivers_plaintext() {
        let a = serde_json::to_value(acct()).unwrap();
        let h = serde_json::to_value(holding()).unwrap();
        for (op, args) in [
            (
                CREATE_OP,
                serde_json::json!({"account": a, "kind": "api_key", "secret": VALUE, "dry_run": true}),
            ),
            (
                SET_SECRET_OP,
                serde_json::json!({"account": a, "secret": VALUE, "dry_run": true}),
            ),
            (
                APPLY_CONSUMER_OP,
                serde_json::json!({"holding": h, "secret": VALUE, "dry_run": true}),
            ),
        ] {
            let out = dispatch_op(&SeesSecret, op, args).await.unwrap();
            assert_eq!(out["detail"]["has_secret"], false, "{op}");
        }
        for (op, args) in [
            (SET_SECRET_OP, serde_json::json!({"account": a})),
            (APPLY_CONSUMER_OP, serde_json::json!({"holding": h})),
        ] {
            let err = dispatch_op(&SeesSecret, op, args).await.unwrap_err();
            assert!(err.as_str().unwrap().contains("requires a secret"), "{op}");
        }
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_proxy_strips_secret_on_dry_run() {
        let sent: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let rec = sent.clone();
        let invoke: InvokeThunk = Arc::new(move |_op, args, _call| {
            rec.lock().unwrap().push(args);
            Ok(serde_json::json!({"executed": false}))
        });
        let proxy = AccountProxy {
            name: "account-proxy-dry".into(),
            invoke,
        };
        proxy
            .create(CreateRequest {
                account: acct(),
                kind: AccountKind::ApiKey,
                roles: vec![],
                secret: Some(secret()),
                dry_run: true,
            })
            .await
            .unwrap();
        proxy
            .set_secret(SetSecretRequest {
                account: acct(),
                secret: Some(secret()),
                dry_run: true,
                rollback: false,
            })
            .await
            .unwrap();
        proxy
            .apply_consumer(ConsumerApplyRequest {
                holding: holding(),
                secret: Some(secret()),
                dry_run: true,
                rollback: false,
            })
            .await
            .unwrap();
        let err = proxy
            .set_secret(SetSecretRequest {
                account: acct(),
                secret: None,
                dry_run: false,
                rollback: false,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("requires a secret"));
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 3);
        for args in sent.iter() {
            assert!(args.get("secret").is_none(), "{args}");
            assert!(!args.to_string().contains(VALUE));
        }
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_proxy_scrubs_panic_and_invalid_json_errors() {
        let invoke: InvokeThunk = Arc::new(move |op, _args, _call| match op {
            SET_SECRET_OP => panic!("boom {VALUE}"),
            _ => Ok(serde_json::json!({"executed": true, "generated": 5551234987u64})),
        });
        let proxy = AccountProxy {
            name: "account-proxy-errs".into(),
            invoke,
        };
        let e = proxy
            .set_secret(SetSecretRequest {
                account: acct(),
                secret: Some(secret()),
                dry_run: false,
                rollback: false,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("task panicked"), "{e}");
        assert!(!e.contains(VALUE), "{e}");
        let e = proxy
            .regenerate(RegenerateRequest {
                account: acct(),
                dry_run: false,
                min_len: MIN_SECRET_LEN,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("returned invalid JSON"), "{e}");
        assert!(!e.contains("5551234987"), "{e}");
    }
    #[tokio::test]
    async fn account_dispatch_op_refuses_non_string_secret() {
        for bad in [
            serde_json::json!(918273645),
            serde_json::json!(true),
            serde_json::json!({"inner": "nested-s3cr3t-value"}),
        ] {
            let err = dispatch_op(
                &owner(),
                SET_SECRET_OP,
                serde_json::json!({"account": acct(), "secret": bad.clone()}),
            )
            .await
            .unwrap_err();
            let msg = err.as_str().unwrap();
            assert!(msg.contains("secret must be a string"), "{msg}");
            assert!(!msg.contains("918273645"), "{msg}");
            assert!(!msg.contains("nested-s3cr3t-value"), "{msg}");
        }
    }

    #[tokio::test]
    async fn account_consumer_rollback_reports_restored() {
        let h = serde_json::to_value(holding()).unwrap();
        let forward = dispatch_op(
            &FakeConsumer,
            APPLY_CONSUMER_OP,
            serde_json::json!({"holding": h, "secret": VALUE}),
        )
        .await
        .unwrap();
        assert!(forward.get("restored").is_none());
        let back = dispatch_op(
            &FakeConsumer,
            APPLY_CONSUMER_OP,
            serde_json::json!({"holding": h, "secret": "old-value", "rollback": true}),
        )
        .await
        .unwrap();
        assert_eq!(back["restored"], true);
        let decoded: AccountOutcome = serde_json::from_value(back).unwrap();
        assert_eq!(decoded.restored, Some(true));
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_proxy_scrubs_consumer_errors_and_verify_detail() {
        let invoke: InvokeThunk = Arc::new(move |op, args, _call| match op {
            APPLY_CONSUMER_OP | VERIFY_CONSUMER_OP => Err(serde_json::json!(format!(
                "rejected {}",
                args["secret"].as_str().unwrap()
            ))),
            _ => Ok(serde_json::json!({"matches": true, "detail": {"token": "leaky"}})),
        });
        let proxy = AccountProxy {
            name: "account-proxy-consumer".into(),
            invoke,
        };
        let e = proxy
            .apply_consumer(ConsumerApplyRequest {
                holding: holding(),
                secret: Some(secret()),
                dry_run: false,
                rollback: false,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("rejected") && !e.contains(VALUE), "{e}");
        let e = proxy
            .verify_consumer(ConsumerVerifyRequest {
                holding: holding(),
                secret: secret(),
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("rejected") && !e.contains(VALUE), "{e}");
        let v = proxy
            .verify(VerifyRequest {
                account: acct(),
                secret: secret(),
            })
            .await
            .unwrap();
        assert_eq!(v.detail["token"], utils::scrub::REDACTED);
    }

    struct LeakyOwner;

    impl AccountProvider for LeakyOwner {
        fn name(&self) -> &str {
            "account-leaky"
        }
        fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>> {
            Box::pin(async {
                Ok(AccountDecl {
                    unit_kinds: vec![],
                    account_kinds: vec![],
                    capabilities: vec![],
                })
            })
        }
        fn set_secret(&self, _req: SetSecretRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async {
                Ok(AccountOutcome {
                    executed: true,
                    detail: serde_json::json!({"password": VALUE, "id": "x"}),
                    ..Default::default()
                })
            })
        }
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_plugin_success_outcome_is_redacted_end_to_end() {
        let invoke: InvokeThunk = Arc::new(|op, args, _call| {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(dispatch_op(&LeakyOwner, op, args))
        });
        let proxy = AccountProxy {
            name: "account-proxy-e2e".into(),
            invoke,
        };
        let out = proxy
            .set_secret(SetSecretRequest {
                account: acct(),
                secret: Some(secret()),
                dry_run: false,
                rollback: false,
            })
            .await
            .unwrap();
        assert!(out.executed);
        assert_eq!(out.detail["password"], utils::scrub::REDACTED);
        assert_eq!(out.detail["id"], "x");
        assert!(!format!("{out:?}").contains(VALUE));
    }

    struct EchoOwner;

    impl AccountProvider for EchoOwner {
        fn name(&self) -> &str {
            "account-echo"
        }
        fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>> {
            Box::pin(async {
                Ok(AccountDecl {
                    unit_kinds: vec![],
                    account_kinds: vec![],
                    capabilities: vec![],
                })
            })
        }
        fn set_secret(&self, req: SetSecretRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async move {
                let v = req.secret.as_ref().unwrap().expose_secret().clone();
                Ok(AccountOutcome {
                    executed: true,
                    detail: serde_json::json!({"msg": format!("set key to {v}"), "list": [v]}),
                    ..Default::default()
                })
            })
        }
        fn regenerate(&self, _req: RegenerateRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async {
                Ok(AccountOutcome {
                    executed: true,
                    detail: serde_json::json!({"msg": format!("new key {VALUE}")}),
                    generated: Some(Secret::new(VALUE.to_string())),
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn account_dispatch_op_scrubs_echoed_value_under_innocuous_key() {
        let out = dispatch_op(
            &EchoOwner,
            SET_SECRET_OP,
            serde_json::json!({"account": acct(), "secret": VALUE}),
        )
        .await
        .unwrap();
        assert!(!out.to_string().contains(VALUE), "{out}");
        assert!(
            out["detail"]["msg"]
                .as_str()
                .unwrap()
                .starts_with("set key to")
        );
        let regen = dispatch_op(
            &EchoOwner,
            REGENERATE_OP,
            serde_json::json!({"account": acct()}),
        )
        .await
        .unwrap();
        assert_eq!(regen["generated"], VALUE);
        assert!(!regen["detail"].to_string().contains(VALUE));
    }

    #[cfg(feature = "in-process")]
    fn echo_proxy() -> AccountProxy {
        let invoke: InvokeThunk = Arc::new(|op, args, _call| {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(async {
                    let mut raw = match op {
                        SET_SECRET_OP => serde_json::json!({
                            "executed": true,
                            "detail": {"msg": format!("set key to {}", args["secret"].as_str().unwrap())},
                        }),
                        _ => dispatch_op(&EchoOwner, op, args).await?,
                    };
                    if op == REGENERATE_OP {
                        raw["detail"]["msg"] = serde_json::json!(format!("new key {VALUE}"));
                    }
                    Ok(raw)
                })
        });
        AccountProxy {
            name: "account-proxy-echo".into(),
            invoke,
        }
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_proxy_scrubs_echoed_value_under_innocuous_key() {
        let proxy = echo_proxy();
        let out = proxy
            .set_secret(SetSecretRequest {
                account: acct(),
                secret: Some(secret()),
                dry_run: false,
                rollback: false,
            })
            .await
            .unwrap();
        assert!(!out.detail.to_string().contains(VALUE), "{:?}", out.detail);
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_proxy_regenerate_keeps_generated_and_scrubs_detail() {
        let out = echo_proxy()
            .regenerate(RegenerateRequest {
                account: acct(),
                dry_run: false,
                min_len: MIN_SECRET_LEN,
            })
            .await
            .unwrap();
        assert_eq!(out.generated.as_ref().unwrap().expose_secret(), VALUE);
        assert!(!out.detail.to_string().contains(VALUE), "{:?}", out.detail);
        assert!(out.detail["msg"].as_str().unwrap().starts_with("new key"));
    }

    #[test]
    fn account_capabilities_decode_per_kind() {
        let decl: AccountDecl = serde_json::from_value(serde_json::json!({
            "account_kinds": ["api_key", "os_user"],
            "capabilities": [
                {"kind": "api_key", "regenerate": true, "verify": true},
                {"kind": "os_user", "set_secret": true, "rollback_secret": true},
            ],
        }))
        .unwrap();
        let key = decl.capabilities_for(AccountKind::ApiKey);
        assert!(key.regenerate && key.verify);
        assert!(!key.set_secret && !key.rollback_secret);
        let user = decl.capabilities_for(AccountKind::OsUser);
        assert_eq!(user.kind, AccountKind::OsUser);
        assert!(user.set_secret && user.rollback_secret && !user.verify);
        let none = decl.capabilities_for(AccountKind::SshKey);
        assert!(!none.set_secret && !none.rollback_secret);
    }

    #[test]
    fn account_unsupported_is_typed_in_process() {
        let err = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(FakeConsumer.list(ListRequest {
                unit_id: id(OWNER),
                kind: None,
            }))
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<Unsupported>(),
            Some(&Unsupported { op: "list".into() })
        );
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_unsupported_survives_the_proxy() {
        let invoke: InvokeThunk = Arc::new(|op, args, _call| {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(dispatch_op(&FakeConsumer, op, args))
        });
        let proxy = AccountProxy {
            name: "account-proxy-unsupported".into(),
            invoke,
        };
        let err = proxy
            .regenerate(RegenerateRequest {
                account: acct(),
                dry_run: false,
                min_len: MIN_SECRET_LEN,
            })
            .await
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<Unsupported>().map(|u| u.op.as_str()),
            Some("regenerate")
        );
    }

    #[tokio::test]
    async fn account_rollback_without_restored_is_refused() {
        let err = dispatch_op(
            &EchoOwner,
            SET_SECRET_OP,
            serde_json::json!({"account": acct(), "secret": "old-value", "rollback": true}),
        )
        .await
        .unwrap_err();
        assert!(
            err.as_str().unwrap().contains("must report `restored`"),
            "{err}"
        );
        let ok = dispatch_op(
            &FakeConsumer,
            APPLY_CONSUMER_OP,
            serde_json::json!({"holding": holding(), "secret": "old-value", "rollback": true}),
        )
        .await
        .unwrap();
        assert_eq!(ok["restored"], true);
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_proxy_rollback_without_restored_is_refused() {
        let invoke: InvokeThunk =
            Arc::new(|_op, _args, _call| Ok(serde_json::json!({"executed": true})));
        let proxy = AccountProxy {
            name: "account-proxy-rollback".into(),
            invoke,
        };
        for result in [
            proxy
                .set_secret(SetSecretRequest {
                    account: acct(),
                    secret: Some(secret()),
                    dry_run: false,
                    rollback: true,
                })
                .await,
            proxy
                .apply_consumer(ConsumerApplyRequest {
                    holding: holding(),
                    secret: Some(secret()),
                    dry_run: false,
                    rollback: true,
                })
                .await,
        ] {
            let e = result.unwrap_err().to_string();
            assert!(e.contains("must report `restored`"), "{e}");
        }
        assert!(
            proxy
                .set_secret(SetSecretRequest {
                    account: acct(),
                    secret: Some(secret()),
                    dry_run: false,
                    rollback: false,
                })
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn account_rollback_dry_run_is_rejected() {
        for (op, args) in [
            (
                SET_SECRET_OP,
                serde_json::json!({"account": acct(), "secret": VALUE, "dry_run": true, "rollback": true}),
            ),
            (
                APPLY_CONSUMER_OP,
                serde_json::json!({"holding": holding(), "secret": VALUE, "dry_run": true, "rollback": true}),
            ),
        ] {
            let err = dispatch_op(&FakeConsumer, op, args).await.unwrap_err();
            let msg = err.as_str().unwrap();
            assert!(msg.contains("rollback cannot be a dry run"), "{msg}");
            assert!(!msg.contains(VALUE));
        }
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_proxy_rollback_dry_run_never_reaches_the_wire() {
        let invoke: InvokeThunk =
            Arc::new(|_op, _args, _call| panic!("rollback dry run must not be sent"));
        let proxy = AccountProxy {
            name: "account-proxy-rollback-dry".into(),
            invoke,
        };
        let e = proxy
            .apply_consumer(ConsumerApplyRequest {
                holding: holding(),
                secret: Some(secret()),
                dry_run: true,
                rollback: true,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("rollback cannot be a dry run"), "{e}");
    }

    #[test]
    fn account_consumer_capabilities_are_per_holding() {
        assert!(holding().supports_rotation());
        let legacy: ConsumerDecl = serde_json::from_value(serde_json::json!({
            "account": {"unit_id": OWNER, "account": "api"},
            "consumer_unit_id": CONSUMER,
            "setting": "k",
        }))
        .unwrap();
        assert_eq!(legacy.capabilities, ConsumerCapabilities::default());
        assert!(!legacy.supports_rotation());
        let no_rollback: ConsumerDecl = serde_json::from_value(serde_json::json!({
            "account": {"unit_id": OWNER, "account": "api"},
            "consumer_unit_id": CONSUMER,
            "setting": "k",
            "capabilities": {"apply": true, "verify": true},
        }))
        .unwrap();
        assert!(no_rollback.capabilities.verify);
        assert!(!no_rollback.supports_rotation());
        let mut no_verify = holding();
        no_verify.capabilities.verify = false;
        assert!(!no_verify.supports_rotation());
    }

    struct HalfRestorer;

    impl AccountProvider for HalfRestorer {
        fn name(&self) -> &str {
            "account-half-restorer"
        }
        fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>> {
            Box::pin(async {
                Ok(AccountDecl {
                    unit_kinds: vec![],
                    account_kinds: vec![],
                    capabilities: vec![],
                })
            })
        }
        fn apply_consumer(
            &self,
            req: ConsumerApplyRequest,
        ) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async move {
                Ok(AccountOutcome {
                    executed: true,
                    restored: req.rollback.then_some(false),
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn account_rollback_restored_false_is_passed_through() {
        let out = dispatch_op(
            &HalfRestorer,
            APPLY_CONSUMER_OP,
            serde_json::json!({"holding": holding(), "secret": "old-value", "rollback": true}),
        )
        .await
        .unwrap();
        assert_eq!(out["restored"], false);
        let decoded: AccountOutcome = serde_json::from_value(out).unwrap();
        assert_eq!(decoded.restored, Some(false));
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_proxy_rollback_restored_false_reaches_core() {
        let invoke: InvokeThunk = Arc::new(|op, args, _call| {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(dispatch_op(&HalfRestorer, op, args))
        });
        let proxy = AccountProxy {
            name: "account-proxy-half".into(),
            invoke,
        };
        let out = proxy
            .apply_consumer(ConsumerApplyRequest {
                holding: holding(),
                secret: Some(secret()),
                dry_run: false,
                rollback: true,
            })
            .await
            .unwrap();
        assert_eq!(out.restored, Some(false));
    }

    struct WrongUnsupported;

    impl AccountProvider for WrongUnsupported {
        fn name(&self) -> &str {
            "account-wrong-unsupported"
        }
        fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>> {
            Box::pin(async {
                Ok(AccountDecl {
                    unit_kinds: vec![],
                    account_kinds: vec![],
                    capabilities: vec![],
                })
            })
        }
        fn set_secret(&self, _req: SetSecretRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            unsupported(VERIFY_OP)
        }
    }

    #[tokio::test]
    async fn account_dispatch_op_unsupported_must_name_the_requested_op() {
        let err = dispatch_op(
            &WrongUnsupported,
            SET_SECRET_OP,
            serde_json::json!({"account": acct(), "secret": VALUE}),
        )
        .await
        .unwrap_err();
        assert!(err.is_string(), "{err}");
        assert!(err.as_str().unwrap().contains("unsupported: verify"));
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_proxy_ignores_spoofed_unsupported() {
        let invoke: InvokeThunk =
            Arc::new(|_op, _args, _call| Err(serde_json::json!({"unsupported": "delete"})));
        let proxy = AccountProxy {
            name: "account-proxy-spoof".into(),
            invoke,
        };
        let err = proxy
            .set_secret(SetSecretRequest {
                account: acct(),
                secret: Some(secret()),
                dry_run: false,
                rollback: false,
            })
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<Unsupported>().is_none(), "{err}");
        let invoke: InvokeThunk =
            Arc::new(|_op, _args, _call| Err(serde_json::json!({"unsupported": "delete"})));
        let proxy = AccountProxy {
            name: "account-proxy-honest".into(),
            invoke,
        };
        let err = proxy
            .delete(DeleteRequest {
                account: acct(),
                dry_run: false,
            })
            .await
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<Unsupported>().map(|u| u.op.as_str()),
            Some("delete")
        );
    }

    struct DeclOf(AccountDecl);

    impl AccountProvider for DeclOf {
        fn name(&self) -> &str {
            "account-decl-of"
        }
        fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>> {
            let d = self.0.clone();
            Box::pin(async move { Ok(d) })
        }
    }

    #[tokio::test]
    async fn account_decl_is_validated() {
        let decl = |kinds: Vec<AccountKind>, caps: Vec<KindCapabilities>| AccountDecl {
            unit_kinds: vec![],
            account_kinds: kinds,
            capabilities: caps,
        };
        let ok = decl(
            vec![AccountKind::ApiKey, AccountKind::OsUser],
            vec![KindCapabilities::none(AccountKind::ApiKey)],
        );
        assert!(ok.validate().is_ok());
        assert!(
            dispatch_op(&DeclOf(ok), DECL_OP, serde_json::json!({}))
                .await
                .is_ok()
        );
        for (bad, why) in [
            (
                decl(vec![AccountKind::ApiKey, AccountKind::ApiKey], vec![]),
                "declared twice",
            ),
            (
                decl(
                    vec![AccountKind::ApiKey],
                    vec![
                        KindCapabilities::none(AccountKind::ApiKey),
                        KindCapabilities::none(AccountKind::ApiKey),
                    ],
                ),
                "declared twice",
            ),
            (
                decl(
                    vec![AccountKind::ApiKey],
                    vec![KindCapabilities::none(AccountKind::SshKey)],
                ),
                "not in account_kinds",
            ),
        ] {
            assert!(bad.validate().unwrap_err().contains(why));
            let err = dispatch_op(&DeclOf(bad), DECL_OP, serde_json::json!({}))
                .await
                .unwrap_err();
            assert!(err.as_str().unwrap().contains(why), "{err}");
        }
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_proxy_rejects_invalid_decl() {
        let invoke: InvokeThunk = Arc::new(|_op, _args, _call| {
            Ok(serde_json::json!({
                "account_kinds": ["api_key"],
                "capabilities": [{"kind": "os_user", "set_secret": true}],
            }))
        });
        let proxy = AccountProxy {
            name: "account-proxy-decl".into(),
            invoke,
        };
        let e = proxy.decl().await.unwrap_err().to_string();
        assert!(e.contains("not in account_kinds"), "{e}");
    }

    #[tokio::test]
    async fn account_decl_tolerates_unknown_wire_kinds() {
        let decl: AccountDecl = serde_json::from_value(serde_json::json!({
            "account_kinds": ["api_key", "passkey", "webauthn"],
            "capabilities": [
                {"kind": "api_key", "regenerate": true},
                {"kind": "passkey", "set_secret": true},
                {"kind": "webauthn", "verify": true},
            ],
        }))
        .unwrap();
        assert_eq!(
            decl.account_kinds,
            vec![
                AccountKind::ApiKey,
                AccountKind::Unknown,
                AccountKind::Unknown
            ]
        );
        assert!(decl.validate().is_ok());
        assert!(decl.capabilities_for(AccountKind::ApiKey).regenerate);
        assert_eq!(
            decl.capabilities_for(AccountKind::Unknown),
            KindCapabilities::none(AccountKind::Unknown)
        );
        let out = dispatch_op(&DeclOf(decl), DECL_OP, serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(out["account_kinds"][0], "api_key");
    }

    #[tokio::test]
    async fn account_consumers_in_rejects_conflicting_duplicate_holdings() {
        let (unit, held, _) = claim_fixtures();
        let allow: ConsumerAuthorizer = Arc::new(|_: &str, _: &Id| true);

        let mut richer = held.clone();
        richer.capabilities.rollback = true;
        let p = claimer("account-claim-caps", vec![held.clone(), richer]);
        let err = consumers_in(&[p], Some(&allow), &unit)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("different capabilities"), "{err}");

        let both = claimer(
            "account-claim-cross",
            vec![
                held.clone(),
                ConsumerDecl {
                    account: AccountRef {
                        unit_id: id(OWNER),
                        account: nm("admin"),
                    },
                    ..held.clone()
                },
            ],
        );
        let err = consumers_in(&[both], Some(&allow), &unit)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("claimed for two accounts"), "{err}");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_proxy_scrubs_generated_value_in_regenerate_errors() {
        const MINTED: &str = "minted-key-9f8e7d6c5b4a";
        let invoke: InvokeThunk = Arc::new(|_op, _args, _call| {
            Err(serde_json::json!({
                "message": format!("could not persist {MINTED}"),
                "generated": MINTED,
            }))
        });
        let proxy = AccountProxy {
            name: "account-proxy-regen-err".into(),
            invoke,
        };
        let e = proxy
            .regenerate(RegenerateRequest {
                account: acct(),
                dry_run: false,
                min_len: MIN_SECRET_LEN,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("could not persist"), "{e}");
        assert!(!e.contains(MINTED), "{e}");

        let invoke: InvokeThunk = Arc::new(|_op, _args, _call| {
            Err(serde_json::json!(
                "upstream said Authorization: Bearer eyJhbGciOi.eyJzdWIi.c2ln"
            ))
        });
        let proxy = AccountProxy {
            name: "account-proxy-regen-generic".into(),
            invoke,
        };
        let e = proxy
            .regenerate(RegenerateRequest {
                account: acct(),
                dry_run: false,
                min_len: MIN_SECRET_LEN,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(!e.contains("eyJhbGciOi.eyJzdWIi.c2ln"), "{e}");
    }

    #[test]
    fn account_short_values_redact_as_whole_text_or_word() {
        let short = ["ab1".to_string()];
        assert_eq!(scrubbed("ab1", &short), utils::scrub::REDACTED);
        assert_eq!(
            scrubbed("set key to ab1", &short),
            format!("set key to {}", utils::scrub::REDACTED)
        );
        assert_eq!(
            scrubbed("ab1/ab1", &short),
            format!("{0}/{0}", utils::scrub::REDACTED)
        );
        assert_eq!(scrubbed("tab1e row", &short), "tab1e row");
        let both = ["ab1".to_string(), "ab1-long-value".to_string()];
        assert_eq!(
            scrubbed("got ab1-long-value", &both),
            format!("got {}", utils::scrub::REDACTED)
        );
        assert_eq!(
            scrubbed("tab1e row", &["ab1e".to_string()]),
            format!("t{} row", utils::scrub::REDACTED)
        );
        let mut leaf =
            serde_json::json!({"detail": {"msg": "ab1", "other": "table"}, "n": ["ab1"]});
        scrub_known_values(&mut leaf, &short);
        assert_eq!(leaf["detail"]["msg"], utils::scrub::REDACTED);
        assert_eq!(leaf["detail"]["other"], "table");
        assert_eq!(leaf["n"][0], utils::scrub::REDACTED);
        let mut num = serde_json::json!({"msg": "got 7 for code 7"});
        scrub_known_values(&mut num, &["7".to_string()]);
        assert!(!num["msg"].as_str().unwrap().contains('7'), "{num}");
    }

    #[tokio::test]
    async fn account_short_secrets_are_rejected() {
        let a = serde_json::to_value(acct()).unwrap();
        let h = serde_json::to_value(holding()).unwrap();
        for (op, args) in [
            (
                SET_SECRET_OP,
                serde_json::json!({"account": a, "secret": "ab1"}),
            ),
            (
                APPLY_CONSUMER_OP,
                serde_json::json!({"holding": h, "secret": "ab1"}),
            ),
            (
                CREATE_OP,
                serde_json::json!({"account": a, "kind": "api_key", "secret": "ab1"}),
            ),
        ] {
            let err = dispatch_op(&SeesSecret, op, args).await.unwrap_err();
            let msg = err.as_str().unwrap();
            assert!(msg.contains("4-character minimum"), "{op}: {msg}");
        }
        assert!(
            dispatch_op(
                &SeesSecret,
                SET_SECRET_OP,
                serde_json::json!({"account": a, "secret": "ab1", "dry_run": true}),
            )
            .await
            .is_ok()
        );
    }

    #[derive(Default)]
    struct ShortMinter {
        current: std::sync::Mutex<String>,
        sent_min_len: std::sync::Mutex<Option<usize>>,
    }

    impl AccountProvider for ShortMinter {
        fn name(&self) -> &str {
            "account-short-minter"
        }
        fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>> {
            Box::pin(async {
                Ok(AccountDecl {
                    unit_kinds: vec![],
                    account_kinds: vec![],
                    capabilities: vec![],
                })
            })
        }
        fn regenerate(&self, req: RegenerateRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            *self.sent_min_len.lock().unwrap() = Some(req.min_len);
            *self.current.lock().unwrap() = "k9".to_string();
            Box::pin(async {
                Ok(AccountOutcome {
                    executed: true,
                    generated: Some(Secret::new("k9".to_string())),
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn account_short_generated_value_reports_the_rotation() {
        let minter = ShortMinter::default();
        let err = dispatch_op(
            &minter,
            REGENERATE_OP,
            serde_json::json!({"account": acct(), "min_len": 1}),
        )
        .await
        .unwrap_err();
        assert_eq!(*minter.sent_min_len.lock().unwrap(), Some(MIN_SECRET_LEN));
        let live = minter.current.lock().unwrap().clone();
        assert_eq!(err[ROTATED_KEY]["generated"], live);
        assert_eq!(err[ROTATED_KEY]["account"]["account"], "api");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_proxy_rejects_short_secret_and_generated() {
        let invoke: InvokeThunk = Arc::new(|op, _args, _call| match op {
            SET_SECRET_OP => panic!("short secret must not reach the wire"),
            _ => Ok(serde_json::json!({"executed": true, "generated": "k9"})),
        });
        let proxy = AccountProxy {
            name: "account-proxy-short".into(),
            invoke,
        };
        let e = proxy
            .set_secret(SetSecretRequest {
                account: acct(),
                secret: Some(Secret::new("ab1".to_string())),
                dry_run: false,
                rollback: false,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("4-character minimum") && !e.contains("ab1"),
            "{e}"
        );
        let e = proxy
            .regenerate(RegenerateRequest {
                account: acct(),
                dry_run: false,
                min_len: MIN_SECRET_LEN,
            })
            .await
            .unwrap_err();
        let rejected = e.downcast_ref::<RotatedButRejected>().expect("typed");
        assert_eq!(rejected.generated.expose_secret(), "k9");
        assert_eq!(rejected.account, acct());
        let msg = e.to_string();
        assert!(
            msg.contains("already rotated") && !msg.contains("k9"),
            "{msg}"
        );
        assert!(!format!("{e:?}").contains("k9"));
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn account_proxy_rotated_but_rejected_survives_the_wire() {
        let minter = Arc::new(ShortMinter::default());
        let m = minter.clone();
        let invoke: InvokeThunk = Arc::new(move |op, args, _call| {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(dispatch_op(m.as_ref(), op, args))
        });
        let proxy = AccountProxy {
            name: "account-proxy-rotated".into(),
            invoke,
        };
        let e = proxy
            .regenerate(RegenerateRequest {
                account: acct(),
                dry_run: false,
                min_len: 0,
            })
            .await
            .unwrap_err();
        let rejected = e.downcast_ref::<RotatedButRejected>().expect("typed");
        let live = minter.current.lock().unwrap().clone();
        assert_eq!(rejected.generated.expose_secret(), &live);
        assert_eq!(*minter.sent_min_len.lock().unwrap(), Some(MIN_SECRET_LEN));
        assert!(!e.to_string().contains(&live));
    }

    #[test]
    fn account_non_string_generated_counts_as_too_short() {
        for (generated, short) in [
            (serde_json::json!(918273645), true),
            (serde_json::json!({"k": "longvalue"}), true),
            (serde_json::json!("longvalue"), false),
            (serde_json::Value::Null, false),
        ] {
            let out = serde_json::json!({"executed": true, "generated": generated});
            assert_eq!(generated_too_short(&out, MIN_SECRET_LEN), short, "{out}");
        }
    }

    #[tokio::test]
    async fn account_verify_refuses_non_string_secret() {
        let err = dispatch_op(
            &owner(),
            VERIFY_OP,
            serde_json::json!({"account": acct(), "secret": 918273645}),
        )
        .await
        .unwrap_err();
        let msg = err.as_str().unwrap();
        assert!(msg.contains("secret must be a string") && !msg.contains("918273645"));
    }
}
