//! Unraid GraphQL client — typed facade over [`unraid_generated`].
//!
//! Transport (HTTP, headers, retries) goes through [`graphql::Client`]. Each
//! public method picks the right generated `GraphQLQuery` impl and routes
//! through [`graphql::Client::query_typed`], which round-trips a typed
//! `Response<ResponseData>` over the wire — no opaque JSON intermediate
//! (see [[feedback-no-serde-json-value]]).
//!
//! Slice A: only Unraid 7.3.1 wired. Slice B adds runtime version probe +
//! schema drift detection.

use graphql::{Client as GraphQlClient, GraphQlErrors};
use std::collections::HashMap;
use thiserror::Error;
use unraid_generated::v7_3_1::{
    AddPlugin, ArrayStatus, InstalledPlugins, ParityHistory, RemovePlugin, Shares, add_plugin,
    array_status, installed_plugins, parity_history, remove_plugin, shares,
};

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub token: String,
    pub insecure: bool,
}

impl Config {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
            insecure: false,
        }
    }

    pub fn insecure(mut self, on: bool) -> Self {
        self.insecure = on;
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/graphql", self.base_url.trim_end_matches('/'))
    }
}

#[derive(Debug, Error)]
pub enum UnraidError {
    #[error(transparent)]
    GraphQl(#[from] GraphQlErrors),
}

#[derive(Clone)]
pub struct Client {
    endpoint: String,
    headers: HashMap<String, String>,
    insecure: bool,
    gql: GraphQlClient,
}

impl Client {
    pub fn new(cfg: Config) -> Self {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {}", cfg.token));
        Self {
            endpoint: cfg.endpoint(),
            headers,
            insecure: cfg.insecure,
            gql: GraphQlClient::new(),
        }
    }

    pub async fn installed_plugins(&self) -> Result<installed_plugins::ResponseData, UnraidError> {
        self.run::<InstalledPlugins>(installed_plugins::Variables)
            .await
    }

    pub async fn array(&self) -> Result<array_status::ResponseData, UnraidError> {
        self.run::<ArrayStatus>(array_status::Variables).await
    }

    pub async fn shares(&self) -> Result<shares::ResponseData, UnraidError> {
        self.run::<Shares>(shares::Variables).await
    }

    pub async fn parity_history(&self) -> Result<parity_history::ResponseData, UnraidError> {
        self.run::<ParityHistory>(parity_history::Variables).await
    }

    pub async fn add_plugin(
        &self,
        input: add_plugin::PluginManagementInput,
    ) -> Result<add_plugin::ResponseData, UnraidError> {
        self.run::<AddPlugin>(add_plugin::Variables { input }).await
    }

    pub async fn remove_plugin(
        &self,
        input: remove_plugin::PluginManagementInput,
    ) -> Result<remove_plugin::ResponseData, UnraidError> {
        self.run::<RemovePlugin>(remove_plugin::Variables { input })
            .await
    }

    async fn run<Q>(&self, variables: Q::Variables) -> Result<Q::ResponseData, UnraidError>
    where
        Q: graphql_client::GraphQLQuery,
    {
        Ok(self
            .gql
            .query_typed::<Q>(
                &self.endpoint,
                variables,
                Some(&self.headers),
                self.insecure,
            )
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg(uri: String) -> Config {
        Config::new(uri, "tok")
    }

    #[tokio::test]
    async fn installed_plugins_sends_bearer_and_parses_scalar() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "installedUnraidPlugins": ["foo", "bar"] }
            })))
            .mount(&server)
            .await;
        let r = Client::new(cfg(server.uri()))
            .installed_plugins()
            .await
            .unwrap();
        assert_eq!(r.installed_unraid_plugins, vec!["foo", "bar"]);
    }

    #[tokio::test]
    async fn add_plugin_serializes_input() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_partial_json(
                json!({"variables": {"input": {"names": ["ca.cleanup.appdata.plg"]}}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "addPlugin": true }
            })))
            .mount(&server)
            .await;
        let out = Client::new(cfg(server.uri()))
            .add_plugin(add_plugin::PluginManagementInput {
                names: vec!["ca.cleanup.appdata.plg".into()],
                bundled: false,
                restart: false,
            })
            .await
            .unwrap();
        assert!(out.add_plugin);
    }

    #[tokio::test]
    async fn graphql_errors_propagate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": null,
                "errors": [{"message": "array offline"}]
            })))
            .mount(&server)
            .await;
        let err = Client::new(cfg(server.uri())).array().await.unwrap_err();
        assert!(matches!(err, UnraidError::GraphQl(_)));
    }

    #[test]
    fn endpoint_trims_trailing_slash() {
        let c = Config::new("http://srv/", "tok").insecure(true);
        assert_eq!(c.endpoint(), "http://srv/graphql");
        assert!(c.insecure);
    }
}
