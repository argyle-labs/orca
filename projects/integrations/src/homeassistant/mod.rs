//! Home Assistant REST client.
// serde_json::Value is intentional: HA entity state attributes are
// free-form by design — each integration defines its own attribute schema.
#![allow(clippy::disallowed_types)]

use orca_utils::http::{Client as HttpClient, HttpError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub token: String,
}

impl Config {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
        }
    }
}

#[derive(Debug, Error)]
pub enum HaError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("missing 'entity_id'")]
    MissingEntityId,
    #[error("'domain' and 'service' are required")]
    MissingService,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceCall {
    pub domain: String,
    pub service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(default)]
    pub data: Map<String, Value>,
}

#[derive(Clone)]
pub struct Client {
    cfg: Config,
    http: HttpClient,
}

impl Client {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            http: HttpClient::new(),
        }
    }

    pub fn with_http(cfg: Config, http: HttpClient) -> Self {
        Self { cfg, http }
    }

    /// List all entities, optionally filtered by domain (light, sensor, …).
    pub async fn entity_list(&self, domain: Option<&str>) -> Result<Value, HaError> {
        let mut req = self
            .http
            .get(self.url("/api/states"))
            .bearer(&self.cfg.token);
        if let Some(d) = domain.filter(|d| !d.is_empty()) {
            req = req.query("domain", d);
        }
        let resp = req.send().await?;
        Ok(resp
            .json::<Value>()
            .unwrap_or_else(|_| Value::String(resp.text())))
    }

    /// Fetch the current state of one entity.
    pub async fn entity_state(&self, entity_id: &str) -> Result<Value, HaError> {
        if entity_id.is_empty() {
            return Err(HaError::MissingEntityId);
        }
        let path = format!("/api/states/{}", urlencoding::encode(entity_id));
        let resp = self
            .http
            .get(self.url(&path))
            .bearer(&self.cfg.token)
            .send()
            .await?;
        Ok(resp
            .json::<Value>()
            .unwrap_or_else(|_| Value::String(resp.text())))
    }

    /// List automations (entity_list filtered to domain=automation).
    pub async fn automation_list(&self) -> Result<Value, HaError> {
        self.entity_list(Some("automation")).await
    }

    /// Invoke a Home Assistant service (light.turn_on, switch.toggle, …).
    pub async fn service_call(&self, call: &ServiceCall) -> Result<Value, HaError> {
        if call.domain.is_empty() || call.service.is_empty() {
            return Err(HaError::MissingService);
        }
        let mut payload = call.data.clone();
        if let Some(eid) = call.entity_id.as_ref().filter(|s| !s.is_empty()) {
            payload.insert("entity_id".to_string(), Value::String(eid.clone()));
        }
        let path = format!(
            "/api/services/{}/{}",
            urlencoding::encode(&call.domain),
            urlencoding::encode(&call.service),
        );
        let resp = self
            .http
            .post(self.url(&path))
            .bearer(&self.cfg.token)
            .json(Value::Object(payload))
            .send()
            .await?;
        Ok(resp
            .json::<Value>()
            .unwrap_or_else(|_| Value::String(resp.text())))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.base_url.trim_end_matches('/'), path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg(uri: String) -> Config {
        Config::new(uri, "tok")
    }

    #[tokio::test]
    async fn entity_list_no_filter() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/states"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([{"entity_id":"light.lr"}])),
            )
            .mount(&server)
            .await;
        let v = Client::new(cfg(server.uri()))
            .entity_list(None)
            .await
            .unwrap();
        assert_eq!(v[0]["entity_id"], "light.lr");
    }

    #[tokio::test]
    async fn entity_list_with_domain_filter() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/states"))
            .and(query_param("domain", "light"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        Client::new(cfg(server.uri()))
            .entity_list(Some("light"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn entity_state_url_encodes_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/states/light.living_room"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state":"on"})))
            .mount(&server)
            .await;
        let v = Client::new(cfg(server.uri()))
            .entity_state("light.living_room")
            .await
            .unwrap();
        assert_eq!(v["state"], "on");
    }

    #[tokio::test]
    async fn entity_state_empty_rejected() {
        let c = Client::new(Config::new("http://nope", "t"));
        assert!(matches!(
            c.entity_state("").await.unwrap_err(),
            HaError::MissingEntityId
        ));
    }

    #[tokio::test]
    async fn service_call_merges_entity_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/services/light/turn_on"))
            .and(body_json(json!({"entity_id":"light.lr","brightness":128})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        let mut data = Map::new();
        data.insert("brightness".into(), json!(128));
        let call = ServiceCall {
            domain: "light".into(),
            service: "turn_on".into(),
            entity_id: Some("light.lr".into()),
            data,
        };
        Client::new(cfg(server.uri()))
            .service_call(&call)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn service_call_requires_domain_and_service() {
        let c = Client::new(Config::new("http://nope", "t"));
        let call = ServiceCall {
            domain: String::new(),
            service: "turn_on".into(),
            entity_id: None,
            data: Map::new(),
        };
        assert!(matches!(
            c.service_call(&call).await.unwrap_err(),
            HaError::MissingService
        ));
    }
}
