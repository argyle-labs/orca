//! Service trait for the `spec_registry` domain — OpenAPI/GraphQL spec
//! discovery (filesystem + DB + plugin-declared dirs), registration of
//! URL-fetched specs, MCP-driven sync, and the GraphQL proxy/parse helpers.

use anyhow::Result;
use async_trait::async_trait;

use crate::spec_registry::{
    DbSpecRow, GraphQlInfoData, GraphqlProxyResult, RegisterSpecResult, SpecMetaRow,
    SyncMcpSpecsResult,
};

#[async_trait]
pub trait SpecRegistryService: Send + Sync {
    /// Filesystem-rooted spec list (orca's `~/.orca/openapi/` + DB rows + plugin spec dirs).
    async fn list_specs(&self) -> Result<Vec<SpecMetaRow>>;

    /// DB-backed registry view (URL-fetched + MCP-synced specs only).
    async fn list_db_specs(&self) -> Result<Vec<DbSpecRow>>;

    /// Fetch the JSON spec at `url`, store it under `name` in orca.db.
    async fn register_spec(&self, name: &str, url: &str) -> Result<RegisterSpecResult>;

    /// Re-fetch a previously-registered spec from its stored URL.
    async fn refresh_spec(&self, name: &str) -> Result<RegisterSpecResult>;

    /// Remove a spec from the DB. Returns `true` if a row was deleted.
    async fn unregister_spec(&self, name: &str) -> Result<bool>;

    /// Connect to an MCP server and pull `{prefix}_spec_list` + `{prefix}_spec_schema`
    /// for every advertised repo.
    async fn sync_mcp_specs(&self, server: &str) -> Result<SyncMcpSpecsResult>;

    /// Parse the local `<repo>.graphql` SDL into a structured `GraphQlInfo`.
    async fn graphql_info(&self, repo: &str) -> Result<GraphQlInfoData>;

    /// Proxy a GraphQL request to a Shopify shop. Returns raw upstream JSON
    /// because GraphQL response shapes are arbitrary per-query.
    /// `variables` is opaque — GraphQL variable maps are free-form per operation.
    #[allow(clippy::disallowed_types)]
    async fn proxy_graphql(
        &self,
        repo: &str,
        shop: &str,
        token: &str,
        query: &str,
        variables: Option<serde_json::Value>,
        operation_name: Option<&str>,
    ) -> Result<GraphqlProxyResult>;
}
