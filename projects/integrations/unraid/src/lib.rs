//! Unraid GraphQL client. Transport (HTTP, headers, retry) is delegated to
//! [`orca_graphql`] so bug fixes land in one place.

use orca_graphql::{Client as GraphQlClient, GraphQlErrors, GraphQlResponse, QueryRequest};
use serde_json::{Value, json};
use std::collections::HashMap;
use thiserror::Error;

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
    #[error("missing required field: {0}")]
    Missing(&'static str),
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

    pub async fn system(&self) -> Result<Value, UnraidError> {
        self.query(QUERY_SYSTEM, None).await
    }

    pub async fn array_status(&self) -> Result<Value, UnraidError> {
        self.query(QUERY_ARRAY_STATUS, None).await
    }

    pub async fn array_start(&self) -> Result<Value, UnraidError> {
        self.query(MUTATION_ARRAY_START, None).await
    }

    pub async fn array_stop(&self) -> Result<Value, UnraidError> {
        self.query(MUTATION_ARRAY_STOP, None).await
    }

    pub async fn disks(&self) -> Result<Value, UnraidError> {
        self.query(QUERY_DISKS, None).await
    }

    pub async fn shares(&self) -> Result<Value, UnraidError> {
        self.query(QUERY_SHARES, None).await
    }

    pub async fn docker_list(&self) -> Result<Value, UnraidError> {
        self.query(QUERY_DOCKER_LIST, None).await
    }

    pub async fn docker_start(&self, name: &str) -> Result<Value, UnraidError> {
        self.named_action(MUTATION_DOCKER_START, name).await
    }

    pub async fn docker_stop(&self, name: &str) -> Result<Value, UnraidError> {
        self.named_action(MUTATION_DOCKER_STOP, name).await
    }

    pub async fn docker_restart(&self, name: &str) -> Result<Value, UnraidError> {
        self.named_action(MUTATION_DOCKER_RESTART, name).await
    }

    pub async fn vm_list(&self) -> Result<Value, UnraidError> {
        self.query(QUERY_VM_LIST, None).await
    }

    pub async fn vm_start(&self, name: &str) -> Result<Value, UnraidError> {
        self.named_action(MUTATION_VM_START, name).await
    }

    pub async fn vm_stop(&self, name: &str) -> Result<Value, UnraidError> {
        self.named_action(MUTATION_VM_STOP, name).await
    }

    pub async fn ups(&self) -> Result<Value, UnraidError> {
        self.query(QUERY_UPS, None).await
    }

    pub async fn parity(&self) -> Result<Value, UnraidError> {
        self.query(QUERY_PARITY, None).await
    }

    pub async fn notifications(&self) -> Result<Value, UnraidError> {
        self.query(QUERY_NOTIFICATIONS, None).await
    }

    /// Escape hatch — execute an arbitrary GraphQL query/mutation.
    pub async fn graphql_query(
        &self,
        query: &str,
        variables: Option<Value>,
    ) -> Result<Value, UnraidError> {
        self.query(query, variables).await
    }

    async fn named_action(&self, query: &str, name: &str) -> Result<Value, UnraidError> {
        if name.is_empty() {
            return Err(UnraidError::Missing("name"));
        }
        self.query(query, Some(json!({ "name": name }))).await
    }

    async fn query(&self, query: &str, vars: Option<Value>) -> Result<Value, UnraidError> {
        let mut req = QueryRequest::new(&self.endpoint, query).headers(&self.headers);
        if let Some(v) = vars {
            req = req.variables(v);
        }
        req.insecure = self.insecure;
        let resp: GraphQlResponse = self.gql.query(req).await?;
        Ok(resp.data)
    }
}

// ── GraphQL queries / mutations (Unraid 6.12+ schema) ────────────────────────

const QUERY_SYSTEM: &str = r#"{
    info { version name uptime }
    cpu { usage temperature }
    memory { total used free }
}"#;

const QUERY_ARRAY_STATUS: &str = r#"{
    array {
        state
        capacity { kilobytes { used free total } }
        disks { name device size temp status }
    }
}"#;

const MUTATION_ARRAY_START: &str = "mutation { startArray { state } }";
const MUTATION_ARRAY_STOP: &str = "mutation { stopArray { state } }";

const QUERY_DISKS: &str = r#"{
    disks { name id device size type temp smartStatus status rotational }
}"#;

const QUERY_SHARES: &str = r#"{
    shares { name comment allocator splitLevel size free cacheEnabled exportEnabled }
}"#;

const QUERY_DOCKER_LIST: &str = r#"{
    docker {
        containers {
            names image state status autoStart
            ports { ip privatePort publicPort type }
        }
    }
}"#;

const MUTATION_DOCKER_START: &str = r#"
mutation StartContainer($name: String!) {
    startContainer(name: $name) { state status }
}"#;

const MUTATION_DOCKER_STOP: &str = r#"
mutation StopContainer($name: String!) {
    stopContainer(name: $name) { state status }
}"#;

const MUTATION_DOCKER_RESTART: &str = r#"
mutation RestartContainer($name: String!) {
    restartContainer(name: $name) { state status }
}"#;

const QUERY_VM_LIST: &str = r#"{
    vms {
        domains { name state autostart cpuMode memory }
    }
}"#;

const MUTATION_VM_START: &str = r#"
mutation StartVM($name: String!) {
    startVM(name: $name) { state }
}"#;

const MUTATION_VM_STOP: &str = r#"
mutation StopVM($name: String!) {
    stopVM(name: $name) { state }
}"#;

const QUERY_UPS: &str = r#"{
    ups { status batteryCharge timeLeft outputVoltage inputVoltage load }
}"#;

const QUERY_PARITY: &str = r#"{
    parity { status lastChecked duration errors speed }
}"#;

const QUERY_NOTIFICATIONS: &str = r#"{
    notifications {
        list { id title description importance timestamp }
    }
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg(uri: String) -> Config {
        Config::new(uri, "tok")
    }

    #[tokio::test]
    async fn system_sends_bearer_to_graphql_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"info": {"version": "6.12.0"}}
            })))
            .mount(&server)
            .await;
        let v = Client::new(cfg(server.uri())).system().await.unwrap();
        assert_eq!(v["info"]["version"], "6.12.0");
    }

    #[tokio::test]
    async fn docker_start_sends_name_variable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_partial_json(json!({"variables": {"name": "plex"}})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"startContainer": {"state": "running", "status": "Up 1s"}}
            })))
            .mount(&server)
            .await;
        let v = Client::new(cfg(server.uri()))
            .docker_start("plex")
            .await
            .unwrap();
        assert_eq!(v["startContainer"]["state"], "running");
    }

    #[tokio::test]
    async fn empty_name_rejected_before_call() {
        let c = Client::new(Config::new("http://nope", "t"));
        assert!(matches!(
            c.docker_start("").await.unwrap_err(),
            UnraidError::Missing("name")
        ));
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
        let err = Client::new(cfg(server.uri()))
            .array_status()
            .await
            .unwrap_err();
        assert!(matches!(err, UnraidError::GraphQl(_)));
    }
}
