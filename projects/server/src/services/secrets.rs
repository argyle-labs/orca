//! Server-side `SecretsService` impl — backs the `secret.*` tools with the
//! `secrets` metadata table + the inline backend (value in encrypted DB).
//!
//! v1: `InlineBackend` only. Additional backends (op-connect, bitwarden,
//! keychain, ...) plug in via the registry in `DbSecretsService::new` once
//! their integration crates land.

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use orca_tools_def::orca_secrets::{BackendInfo, SecretEntry, SecretMutationReport, SecretSetArgs};
use orca_tools_def::services::secrets::{SecretValue, SecretsBackend, SecretsService};
use std::collections::HashMap;
use std::sync::Arc;

// ── InlineBackend ───────────────────────────────────────────────────────────

/// `inline` backend: values live in the encrypted `settings` table (via
/// `db::secrets::write_inline_value`). `ref_path` is the secret name.
pub struct InlineBackend;

#[async_trait]
impl SecretsBackend for InlineBackend {
    fn kind(&self) -> &'static str {
        "inline"
    }

    fn supports_store(&self) -> bool {
        true
    }

    async fn fetch(&self, ref_path: &str) -> Result<SecretValue> {
        let conn = db::open_default()?;
        let v = db::secrets::read_inline_value(&conn, ref_path)?
            .ok_or_else(|| anyhow!("inline secret '{ref_path}' has no stored value"))?;
        Ok(SecretValue(v))
    }

    async fn store(&self, name: &str, value: &str) -> Result<String> {
        let conn = db::open_default()?;
        db::secrets::write_inline_value(&conn, name, value)?;
        Ok(name.to_string())
    }

    async fn delete(&self, ref_path: &str) -> Result<()> {
        // Inline cleanup happens in `db::secrets::delete` already (it knows the
        // backend kind from the row). This method is a no-op for inline so the
        // SecretsService.delete path stays single-purpose.
        let _ = ref_path;
        Ok(())
    }
}

// ── DbSecretsService ────────────────────────────────────────────────────────

pub struct DbSecretsService {
    backends: HashMap<&'static str, Arc<dyn SecretsBackend>>,
}

impl DbSecretsService {
    pub fn new() -> Self {
        let mut backends: HashMap<&'static str, Arc<dyn SecretsBackend>> = HashMap::new();
        let inline: Arc<dyn SecretsBackend> = Arc::new(InlineBackend);
        backends.insert(inline.kind(), inline);
        Self { backends }
    }

    fn backend(&self, kind: &str) -> Result<&Arc<dyn SecretsBackend>> {
        self.backends
            .get(kind)
            .ok_or_else(|| anyhow!("unknown backend '{kind}' (available: {})", self.kinds()))
    }

    fn kinds(&self) -> String {
        let mut k: Vec<&&str> = self.backends.keys().collect();
        k.sort();
        k.iter().map(|s| **s).collect::<Vec<_>>().join(", ")
    }
}

impl Default for DbSecretsService {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SecretsService for DbSecretsService {
    async fn list(&self) -> Result<Vec<SecretEntry>> {
        let conn = db::open_default()?;
        let rows = db::secrets::list(&conn)?;
        Ok(rows
            .into_iter()
            .map(|r| SecretEntry {
                name: r.name,
                backend: r.backend,
                ref_path: r.ref_path,
                description: r.description,
                updated_at: r.updated_at,
            })
            .collect())
    }

    async fn get(&self, name: &str) -> Result<(String, String)> {
        let conn = db::open_default()?;
        let row =
            db::secrets::get(&conn, name)?.ok_or_else(|| anyhow!("no secret named '{name}'"))?;
        drop(conn); // release the connection before the async fetch
        let backend = self.backend(&row.backend)?;
        // For inline, the ref_path is empty in storage but the lookup key is the name.
        let ref_path = if row.backend == "inline" {
            &row.name
        } else {
            &row.ref_path
        };
        let value = backend.fetch(ref_path).await?;
        Ok((row.backend, value.into_inner()))
    }

    async fn set(&self, args: SecretSetArgs) -> Result<SecretMutationReport> {
        let backend = self.backend(&args.backend)?;

        // Validate args by backend kind before touching the DB.
        match args.backend.as_str() {
            "inline" => {
                if args.value.is_none() {
                    bail!("`value` is required for backend=inline");
                }
            }
            _ => {
                if args.ref_path.is_none() {
                    bail!(
                        "`ref_path` is required for backend={} (e.g. 'op://Vault/Item/field')",
                        args.backend
                    );
                }
            }
        }

        // Upsert metadata row first.
        let ref_path_for_storage = match args.backend.as_str() {
            "inline" => String::new(),
            _ => args.ref_path.clone().unwrap(),
        };
        let conn = db::open_default()?;
        let created = db::secrets::upsert(
            &conn,
            &args.name,
            &args.backend,
            &ref_path_for_storage,
            args.description.as_deref(),
        )?;
        drop(conn);

        // Persist value if applicable.
        if args.backend == "inline" {
            backend
                .store(&args.name, args.value.as_deref().unwrap_or(""))
                .await?;
        }

        Ok(SecretMutationReport {
            name: args.name,
            backend: args.backend,
            created,
        })
    }

    async fn delete(&self, name: &str) -> Result<bool> {
        let conn = db::open_default()?;
        // `db::secrets::delete` handles inline-value cleanup atomically with
        // the metadata row removal.
        db::secrets::delete(&conn, name)
    }

    async fn backends(&self) -> Vec<BackendInfo> {
        let mut v: Vec<_> = self
            .backends
            .values()
            .map(|b| BackendInfo {
                kind: b.kind().to_string(),
                supports_store: b.supports_store(),
            })
            .collect();
        v.sort_by(|a, b| a.kind.cmp(&b.kind));
        v
    }
}
