//! `InfraService` impl — talks to the local orca HTTP API for docker compose
//! state + service logs, and delegates `run_tests` to the in-process
//! test-suite runner.

use anyhow::Result;
use async_trait::async_trait;
use orca_tools_def::services::infra::{
    InfraProject, InfraService, InfraServiceState, TestRunResult,
};

pub struct ServerInfra;

#[async_trait]
impl InfraService for ServerInfra {
    async fn list_services(&self) -> Result<Vec<InfraProject>> {
        let resp = reqwest::get("http://127.0.0.1:12000/api/logs/services")
            .await?
            .json::<serde_json::Value>()
            .await?;
        let projects = resp["projects"].as_array().cloned().unwrap_or_default();
        let out = projects
            .into_iter()
            .map(|p| InfraProject {
                project: p["project"].as_str().unwrap_or("?").to_string(),
                path: p["path"].as_str().unwrap_or("?").to_string(),
                services: p["services"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|s| InfraServiceState {
                        name: s["name"].as_str().unwrap_or("?").to_string(),
                        state: s["state"].as_str().unwrap_or("unknown").to_string(),
                        health: s["health"]
                            .as_str()
                            .filter(|v| !v.is_empty())
                            .map(str::to_string),
                        ports: s["ports"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str())
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                    .collect(),
            })
            .collect();
        Ok(out)
    }

    async fn service_logs(&self, project: &str, service: &str, tail: u64) -> Result<String> {
        let tail_str = tail.to_string();
        let resp = reqwest::Client::new()
            .get("http://127.0.0.1:12000/api/logs")
            .query(&[
                ("project", project),
                ("service", service),
                ("tail", tail_str.as_str()),
            ])
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;
        Ok(resp["output"].as_str().unwrap_or("(no output)").to_string())
    }

    async fn run_tests(&self, suite: &str) -> Result<TestRunResult> {
        let r = crate::serve::api::run_test_suite(suite).await?;
        Ok(TestRunResult {
            suite: r.suite,
            output: r.output,
            exit_code: r.exit_code,
            passed: r.passed,
            failed: r.failed,
            duration_ms: r.duration_ms,
        })
    }
}
