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
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct AccountRef {
    pub unit_id: Id,
    pub account: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct AccountInfo {
    pub unit_id: Id,
    pub name: String,
    pub kind: AccountKind,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct ListRequest {
    pub unit_id: Id,
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
    #[serde(serialize_with = "Secret::serialize_exposed")]
    #[schemars(with = "String")]
    pub secret: Secret<String>,
    #[serde(default)]
    pub dry_run: bool,
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
    pub setting: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Default)]
pub struct ConsumersRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountRef>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct ConsumerApplyRequest {
    pub holding: ConsumerDecl,
    #[serde(serialize_with = "Secret::serialize_exposed")]
    #[schemars(with = "String")]
    pub secret: Secret<String>,
    #[serde(default)]
    pub dry_run: bool,
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
}

/// `generated` carries a credential the unit minted itself (e.g. an app that
/// regenerates its own api_key), so core can write it to the secrets backend.
/// It is never placed in `detail`, which is scrubbed.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
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
pub trait AccountProvider: Send + Sync {
    fn name(&self) -> &str;

    fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>>;

    fn list(&self, _req: ListRequest) -> BoxFuture<'_, Result<Vec<AccountInfo>>> {
        unsupported(LIST_OP)
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
    Box::pin(async move { Err(anyhow::anyhow!("account op unsupported: {op}")) })
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

/// Every holding of `account` across all registered providers, paired with the
/// declaring provider's name. Holdings are declared at call time, not at
/// registration, so the one-declarer-per-`(consumer_unit_id, setting)` rule is
/// enforced here: two providers claiming the same setting on the same unit is
/// an error, since rotation could not know which one applies the value.
pub async fn consumers_of(account: &AccountRef) -> Result<Vec<(String, ConsumerDecl)>> {
    let req = ConsumersRequest {
        account: Some(account.clone()),
    };
    let mut out: Vec<(String, ConsumerDecl)> = Vec::new();
    for p in providers() {
        for held in p.consumers(req.clone()).await? {
            if &held.account != account {
                continue;
            }
            if let Some((owner, _)) = out.iter().find(|(_, d)| {
                d.consumer_unit_id == held.consumer_unit_id && d.setting == held.setting
            }) {
                if owner == p.name() {
                    continue;
                }
                anyhow::bail!(
                    "setting '{}' on unit {} is declared by both '{owner}' and '{}'",
                    held.setting,
                    held.consumer_unit_id,
                    p.name()
                );
            }
            out.push((p.name().to_string(), held));
        }
    }
    Ok(out)
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
pub const CREATE_OP: &str = "create";
pub const UPDATE_OP: &str = "update";
pub const DELETE_OP: &str = "delete";
pub const SET_SECRET_OP: &str = "set_secret";
pub const VERIFY_OP: &str = "verify";
pub const REGENERATE_OP: &str = "regenerate";
pub const CONSUMERS_OP: &str = "consumers";
pub const APPLY_CONSUMER_OP: &str = "apply_consumer";
pub const VERIFY_CONSUMER_OP: &str = "verify_consumer";

/// Pulls the plaintext out of request args before decoding, so a provider
/// error that echoes it is redacted on the way back over the wire.
fn arg_secret(args: &serde_json::Value) -> Option<String> {
    args.get("secret")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

pub async fn dispatch_op(
    provider: &dyn AccountProvider,
    op: &str,
    args: serde_json::Value,
) -> std::result::Result<serde_json::Value, serde_json::Value> {
    let secret = arg_secret(&args);
    let err = |msg: String| {
        let values: Vec<&str> = secret.as_deref().into_iter().collect();
        serde_json::Value::String(utils::scrub::redact_values(&msg, &values))
    };
    fn dec<T: serde::de::DeserializeOwned>(
        op: &str,
        args: serde_json::Value,
    ) -> std::result::Result<T, String> {
        serde_json::from_value(args).map_err(|e| format!("decode {op} args: {e}"))
    }
    fn enc<T: Serialize>(out: &T) -> std::result::Result<serde_json::Value, String> {
        serde_json::to_value(out).map_err(|e| e.to_string())
    }
    let out = async {
        match op {
            DECL_OP => enc(&provider.decl().await.map_err(|e| format!("{e:#}"))?),
            LIST_OP => enc(&provider
                .list(dec(op, args)?)
                .await
                .map_err(|e| format!("{e:#}"))?),
            CREATE_OP => enc(&provider
                .create(dec(op, args)?)
                .await
                .map_err(|e| format!("{e:#}"))?),
            UPDATE_OP => enc(&provider
                .update(dec(op, args)?)
                .await
                .map_err(|e| format!("{e:#}"))?),
            DELETE_OP => enc(&provider
                .delete(dec(op, args)?)
                .await
                .map_err(|e| format!("{e:#}"))?),
            SET_SECRET_OP => enc(&provider
                .set_secret(dec(op, args)?)
                .await
                .map_err(|e| format!("{e:#}"))?),
            VERIFY_OP => enc(&provider
                .verify(dec(op, args)?)
                .await
                .map_err(|e| format!("{e:#}"))?),
            REGENERATE_OP => enc(&provider
                .regenerate(dec(op, args)?)
                .await
                .map_err(|e| format!("{e:#}"))?),
            CONSUMERS_OP => enc(&provider
                .consumers(dec(op, args)?)
                .await
                .map_err(|e| format!("{e:#}"))?),
            APPLY_CONSUMER_OP => enc(&provider
                .apply_consumer(dec(op, args)?)
                .await
                .map_err(|e| format!("{e:#}"))?),
            VERIFY_CONSUMER_OP => enc(&provider
                .verify_consumer(dec(op, args)?)
                .await
                .map_err(|e| format!("{e:#}"))?),
            other => Err(format!("unknown account op: {other}")),
        }
    }
    .await;
    out.map_err(err)
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
impl AccountProxy {
    fn call<T: for<'de> Deserialize<'de>>(
        &self,
        op: &'static str,
        args: serde_json::Result<serde_json::Value>,
    ) -> BoxFuture<'_, Result<T>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let args =
                args.map_err(|e| anyhow::anyhow!("account '{name}' {op} encode args: {e}"))?;
            let secret = arg_secret(&args);
            let scrub = move |msg: String| {
                let values: Vec<&str> = secret.as_deref().into_iter().collect();
                anyhow::anyhow!(utils::scrub::redact_values(&msg, &values))
            };
            let call = crate::invocation_scope::capture("account", &name);
            let out = tokio::task::spawn_blocking(move || invoke(op, args, call))
                .await
                .map_err(|e| anyhow::anyhow!("account '{name}' {op} task panicked: {e}"))?
                .map_err(|e| {
                    scrub(format!(
                        "account '{name}' {op} failed: {}",
                        crate::render_invoke_error(&e)
                    ))
                })?;
            serde_json::from_value(out)
                .map_err(|e| anyhow::anyhow!("account '{name}' {op} returned invalid JSON: {e}"))
        })
    }
}

#[cfg(feature = "in-process")]
impl AccountProvider for AccountProxy {
    fn name(&self) -> &str {
        &self.name
    }

    fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>> {
        self.call(DECL_OP, Ok(serde_json::json!({})))
    }

    fn list(&self, req: ListRequest) -> BoxFuture<'_, Result<Vec<AccountInfo>>> {
        self.call(LIST_OP, serde_json::to_value(&req))
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
        let fut = self.call(SET_SECRET_OP, serde_json::to_value(&req));
        Box::pin(async move { fut.await.map(redact_outcome) })
    }

    fn verify(&self, req: VerifyRequest) -> BoxFuture<'_, Result<VerifyOutcome>> {
        let fut = self.call(VERIFY_OP, serde_json::to_value(&req));
        Box::pin(async move { fut.await.map(redact_verify) })
    }

    fn regenerate(&self, req: RegenerateRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
        let fut = self.call(REGENERATE_OP, serde_json::to_value(&req));
        Box::pin(async move { fut.await.map(redact_outcome) })
    }

    fn consumers(&self, req: ConsumersRequest) -> BoxFuture<'_, Result<Vec<ConsumerDecl>>> {
        self.call(CONSUMERS_OP, serde_json::to_value(&req))
    }

    fn apply_consumer(&self, req: ConsumerApplyRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
        let fut = self.call(APPLY_CONSUMER_OP, serde_json::to_value(&req));
        Box::pin(async move { fut.await.map(redact_outcome) })
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

    fn acct() -> AccountRef {
        AccountRef {
            unit_id: id(OWNER),
            account: "api".into(),
        }
    }

    fn holding() -> ConsumerDecl {
        ConsumerDecl {
            account: acct(),
            consumer_unit_id: id(CONSUMER),
            setting: "prowlarr_api_key".into(),
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
                })
            })
        }
        fn list(&self, req: ListRequest) -> BoxFuture<'_, Result<Vec<AccountInfo>>> {
            Box::pin(async move {
                Ok(vec![AccountInfo {
                    unit_id: req.unit_id,
                    name: "api".into(),
                    kind: AccountKind::ApiKey,
                    roles: vec![],
                    enabled: true,
                }])
            })
        }
        fn create(&self, req: CreateRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async move {
                Ok(AccountOutcome {
                    executed: !req.dry_run,
                    detail: serde_json::json!({"has_secret": req.secret.is_some()}),
                    generated: None,
                })
            })
        }
        fn update(&self, req: UpdateRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async move {
                Ok(AccountOutcome {
                    executed: !req.dry_run,
                    detail: serde_json::Value::Null,
                    generated: None,
                })
            })
        }
        fn delete(&self, req: DeleteRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async move {
                Ok(AccountOutcome {
                    executed: !req.dry_run,
                    detail: serde_json::Value::Null,
                    generated: None,
                })
            })
        }
        fn set_secret(&self, req: SetSecretRequest) -> BoxFuture<'_, Result<AccountOutcome>> {
            Box::pin(async move {
                if req.account.account == "boom" {
                    anyhow::bail!("upstream rejected {}", req.secret.expose_secret());
                }
                Ok(AccountOutcome {
                    executed: !req.dry_run,
                    detail: serde_json::Value::Null,
                    generated: None,
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
        assert!(info.roles.is_empty());
    }

    #[test]
    fn account_secret_never_debug_printed() {
        let req = SetSecretRequest {
            account: acct(),
            secret: secret(),
            dry_run: false,
        };
        let dbg = format!("{req:?}");
        assert!(!dbg.contains(VALUE), "{dbg}");
        let apply = ConsumerApplyRequest {
            holding: holding(),
            secret: secret(),
            dry_run: false,
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
            secret: secret(),
            dry_run: true,
        })
        .unwrap();
        assert_eq!(wire["secret"], VALUE);
        let back: SetSecretRequest = serde_json::from_value(wire).unwrap();
        assert_eq!(back.secret.expose_secret(), VALUE);
        assert!(back.dry_run);

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
        assert!(
            err.as_str()
                .unwrap()
                .contains("unsupported: apply_consumer")
        );
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
        assert!(err.as_str().unwrap().contains("unsupported: list"));
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
                .list(ListRequest { unit_id: id(OWNER) })
                .await
                .unwrap()
                .is_empty()
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
        assert_ne!(created.detail["password"], "leak");
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
                secret: secret(),
                dry_run: false,
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
                secret: secret(),
                dry_run: false,
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
                "create",
                "update",
                "delete",
                "set_secret",
                "verify",
                "consumers",
                "apply_consumer",
                "verify_consumer",
            ]
        );
        assert_eq!(calls[5].1["secret"], VALUE);
        assert_eq!(calls[8].1["holding"]["consumer_unit_id"], CONSUMER);
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

    impl AccountProvider for Claimer {
        fn name(&self) -> &str {
            self.name
        }
        fn decl(&self) -> BoxFuture<'_, Result<AccountDecl>> {
            Box::pin(async {
                Ok(AccountDecl {
                    unit_kinds: vec![],
                    account_kinds: vec![],
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
            account: "admin".into(),
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

    #[tokio::test]
    async fn account_consumers_of_enforces_one_declarer_per_setting() {
        let unit = AccountRef {
            unit_id: "0192f0a0-0000-7000-8000-0000000000a1".parse().unwrap(),
            account: "api".into(),
        };
        let held = ConsumerDecl {
            account: unit.clone(),
            consumer_unit_id: id(CONSUMER),
            setting: "key".into(),
        };
        let stray = ConsumerDecl {
            account: AccountRef {
                unit_id: id(OWNER),
                account: "other".into(),
            },
            consumer_unit_id: id(CONSUMER),
            setting: "key".into(),
        };
        register_provider(Arc::new(Claimer {
            name: "account-claim-a",
            holdings: vec![held.clone(), held.clone(), stray],
        }));
        let got = consumers_of(&unit).await.unwrap();
        assert_eq!(got, vec![("account-claim-a".to_string(), held.clone())]);

        register_provider(Arc::new(Claimer {
            name: "account-claim-b",
            holdings: vec![held],
        }));
        let err = consumers_of(&unit).await.unwrap_err().to_string();
        assert!(
            err.contains("account-claim-a") && err.contains("account-claim-b"),
            "{err}"
        );
        deregister_provider("account-claim-a");
        deregister_provider("account-claim-b");
    }
}
