//! HTTP-level backend tests against a wiremock server — verifies model
//! discovery endpoints without any real LLM runner installed.

use model::{LMStudioBackend, OllamaBackend};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn lmstudio_lists_models_from_v1_models() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [ { "id": "qwen/qwen3-8b" }, { "id": "nomic-embed-text" } ]
        })))
        .mount(&server)
        .await;

    let backend = LMStudioBackend::new(server.uri(), "qwen/qwen3-8b");
    let models = backend.list_models().await.unwrap();
    assert_eq!(models, vec!["qwen/qwen3-8b", "nomic-embed-text"]);
}

#[tokio::test]
async fn ollama_prefers_api_tags() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [ { "name": "llama3.2:3b" }, { "name": "qwen3:8b" } ]
        })))
        .mount(&server)
        .await;

    let backend = OllamaBackend::new(server.uri(), "llama3.2:3b");
    let models = backend.list_models().await.unwrap();
    assert_eq!(models, vec!["llama3.2:3b", "qwen3:8b"]);
}

#[tokio::test]
async fn ollama_falls_back_to_openai_compat() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [ { "id": "llama3.2:3b" } ]
        })))
        .mount(&server)
        .await;

    let backend = OllamaBackend::new(server.uri(), "llama3.2:3b");
    let models = backend.list_models().await.unwrap();
    assert_eq!(models, vec!["llama3.2:3b"]);
}

#[tokio::test]
async fn ollama_errors_when_both_endpoints_fail() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let backend = OllamaBackend::new(server.uri(), "x");
    assert!(backend.list_models().await.is_err());
}
