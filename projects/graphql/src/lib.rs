//! Native GraphQL client primitive. Replaces the former `graphql` plugin.
//!
//! Stateless: every call carries the endpoint, headers, query, and variables.
//! Bug fixes to GraphQL transport land here once and propagate to every
//! consumer (e.g. unraid). Composes with [`orca_http`] underneath.

use orca_http::{Client as HttpClient, HttpError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use std::collections::HashMap;

/// Standard GraphQL response envelope: `data` + optional `errors`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphQlResponse {
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub errors: Vec<GraphQlError>,
    #[serde(default)]
    pub extensions: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphQlError {
    pub message: String,
    #[serde(default)]
    pub path: Option<Value>,
    #[serde(default)]
    pub locations: Option<Value>,
    #[serde(default)]
    pub extensions: Option<Value>,
}

#[derive(Debug, Error)]
pub enum GraphQlErrors {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("graphql server returned errors: {summary}")]
    ServerErrors {
        summary: String,
        response: Box<GraphQlResponse>,
    },
}

#[derive(Clone, Default)]
pub struct Client {
    http: HttpClient,
}

impl Client {
    pub fn new() -> Self {
        Self {
            http: HttpClient::new(),
        }
    }

    pub fn with_http(http: HttpClient) -> Self {
        Self { http }
    }

    /// Execute a GraphQL query (or mutation — wire shape is identical).
    /// Returns the parsed envelope. If `errors[]` is non-empty, returns
    /// `GraphQlErrors::ServerErrors` so callers can choose to inspect or
    /// short-circuit.
    pub async fn query(&self, req: QueryRequest<'_>) -> Result<GraphQlResponse, GraphQlErrors> {
        let mut payload = json!({
            "query": req.query,
        });
        if let Some(vars) = req.variables {
            payload["variables"] = vars;
        }
        if let Some(name) = req.operation_name {
            payload["operationName"] = Value::String(name.to_string());
        }

        let mut builder = self.http.post(req.endpoint).json(payload);
        if let Some(h) = req.headers {
            builder = builder.headers(h.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        if req.insecure {
            builder = builder.insecure(true);
        }

        let resp = builder.send().await?;
        let envelope: GraphQlResponse = resp.json()?;
        if !envelope.errors.is_empty() {
            let summary = envelope
                .errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GraphQlErrors::ServerErrors {
                summary,
                response: Box::new(envelope),
            });
        }
        Ok(envelope)
    }

    /// Run the standard `__schema` introspection query.
    pub async fn introspect(
        &self,
        endpoint: &str,
        headers: Option<&HashMap<String, String>>,
        insecure: bool,
    ) -> Result<GraphQlResponse, GraphQlErrors> {
        self.query(QueryRequest {
            endpoint,
            query: INTROSPECTION_QUERY,
            variables: None,
            operation_name: Some("IntrospectionQuery"),
            headers,
            insecure,
        })
        .await
    }
}

#[derive(Debug, Clone)]
pub struct QueryRequest<'a> {
    pub endpoint: &'a str,
    pub query: &'a str,
    pub variables: Option<Value>,
    pub operation_name: Option<&'a str>,
    pub headers: Option<&'a HashMap<String, String>>,
    pub insecure: bool,
}

impl<'a> QueryRequest<'a> {
    pub fn new(endpoint: &'a str, query: &'a str) -> Self {
        Self {
            endpoint,
            query,
            variables: None,
            operation_name: None,
            headers: None,
            insecure: false,
        }
    }
    pub fn variables(mut self, v: Value) -> Self {
        self.variables = Some(v);
        self
    }
    pub fn headers(mut self, h: &'a HashMap<String, String>) -> Self {
        self.headers = Some(h);
        self
    }
}

/// Subset of the canonical GraphQL introspection query. Sufficient to
/// recover types, fields, and arguments — which is what every consumer
/// of `introspect` actually needs.
pub const INTROSPECTION_QUERY: &str = r#"
query IntrospectionQuery {
  __schema {
    queryType { name }
    mutationType { name }
    subscriptionType { name }
    types {
      kind
      name
      description
      fields(includeDeprecated: true) {
        name
        description
        args { name description type { kind name ofType { kind name } } defaultValue }
        type { kind name ofType { kind name } }
      }
      enumValues(includeDeprecated: true) { name description }
    }
  }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn query_returns_data() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("Content-Type", "application/json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": {"ok": true}})),
            )
            .mount(&server)
            .await;
        let endpoint = format!("{}/graphql", server.uri());
        let resp = Client::new()
            .query(QueryRequest::new(&endpoint, "{ ok }"))
            .await
            .unwrap();
        assert_eq!(resp.data["ok"], true);
        assert!(resp.errors.is_empty());
    }

    #[tokio::test]
    async fn server_errors_surface() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"errors": [{"message": "boom"}]})),
            )
            .mount(&server)
            .await;
        let endpoint = format!("{}/graphql", server.uri());
        let err = Client::new()
            .query(QueryRequest::new(&endpoint, "{ x }"))
            .await
            .unwrap_err();
        match err {
            GraphQlErrors::ServerErrors { summary, .. } => assert_eq!(summary, "boom"),
            other => panic!("expected ServerErrors, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_5xx_propagates_as_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let endpoint = format!("{}/graphql", server.uri());
        let err = Client::new()
            .query(QueryRequest::new(&endpoint, "{ x }"))
            .await
            .unwrap_err();
        match err {
            GraphQlErrors::Http(HttpError::Status { status, .. }) => assert_eq!(status, 503),
            other => panic!("expected Http(Status), got {other:?}"),
        }
    }
}
