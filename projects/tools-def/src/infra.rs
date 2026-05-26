//! Infra domain tools — docker compose service listing, log fetch, test runner.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;
#[cfg(feature = "native")]
use crate::services::infra::InfraService;
#[cfg(feature = "native")]
use std::sync::Arc;

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ServiceState {
    pub name: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    pub ports: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProjectServices {
    pub project: String,
    pub path: String,
    pub services: Vec<ServiceState>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListServicesArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListServicesOutput {
    pub projects: Vec<ProjectServices>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetServiceLogsArgs {
    /// Absolute path to the project directory.
    pub project: String,
    /// Service name as defined in docker-compose.
    pub service: String,
    /// Number of log lines to return (default: 200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetServiceLogsOutput {
    pub project: String,
    pub service: String,
    pub output: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RunTestsArgs {
    /// Which suite to run: rust | frontend | e2e | all (default: rust).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RunTestsOutput {
    pub suite: String,
    pub output: String,
    pub exit_code: i32,
    pub passed: u32,
    pub failed: u32,
    pub duration_ms: u64,
}

/// List all running docker compose services across all rebuy projects. Returns
/// project name, path, and per-service state/health/ports.
#[orca_tool(domain = "system.infra.service", verb = "list")]
async fn infra_service_list(
    _args: ListServicesArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<ListServicesOutput> {
    let projects = ctx
        .service::<Arc<dyn InfraService>>()?
        .list_services()
        .await?
        .into_iter()
        .map(|p| ProjectServices {
            project: p.project,
            path: p.path,
            services: p
                .services
                .into_iter()
                .map(|s| ServiceState {
                    name: s.name,
                    state: s.state,
                    health: s.health,
                    ports: s.ports,
                })
                .collect(),
        })
        .collect();
    Ok(ListServicesOutput { projects })
}

/// Fetch docker compose logs for a running rebuy service. Specify the project
/// path and service name.
#[orca_tool(domain = "system.infra.service", verb = "detail")]
async fn infra_service_detail(
    args: GetServiceLogsArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<GetServiceLogsOutput> {
    let tail = args.tail.unwrap_or(200);
    let output = ctx
        .service::<Arc<dyn InfraService>>()?
        .service_logs(&args.project, &args.service, tail)
        .await?;
    Ok(GetServiceLogsOutput {
        project: args.project,
        service: args.service,
        output,
    })
}

/// Run the orca project test suite. Returns test output with pass/fail counts.
/// Suites: rust (cargo test), frontend (vitest), e2e (playwright), all.
#[orca_tool(domain = "system.infra.test", verb = "create")]
async fn infra_test_create(
    args: RunTestsArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<RunTestsOutput> {
    let suite = args.suite.as_deref().unwrap_or("rust");
    let r = ctx
        .service::<Arc<dyn InfraService>>()?
        .run_tests(suite)
        .await?;
    Ok(RunTestsOutput {
        suite: r.suite,
        output: r.output,
        exit_code: r.exit_code,
        passed: r.passed,
        failed: r.failed,
        duration_ms: r.duration_ms,
    })
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use crate::services::infra::{InfraProject, InfraService, InfraServiceState, TestRunResult};
    use crate::test_support::empty_ctx;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Stub {
        last_logs_req: Mutex<Option<(String, String, u64)>>,
        last_test_suite: Mutex<Option<String>>,
    }
    #[async_trait]
    impl InfraService for Stub {
        async fn list_services(&self) -> Result<Vec<InfraProject>> {
            Ok(vec![InfraProject {
                project: "rebuy".into(),
                path: "/x".into(),
                services: vec![InfraServiceState {
                    name: "api".into(),
                    state: "running".into(),
                    health: Some("healthy".into()),
                    ports: vec!["8080:8080".into()],
                }],
            }])
        }
        async fn service_logs(&self, project: &str, service: &str, tail: u64) -> Result<String> {
            *self.last_logs_req.lock().unwrap() = Some((project.into(), service.into(), tail));
            Ok(format!("{project}/{service}#{tail}"))
        }
        async fn run_tests(&self, suite: &str) -> Result<TestRunResult> {
            *self.last_test_suite.lock().unwrap() = Some(suite.into());
            Ok(TestRunResult {
                suite: suite.into(),
                output: "ok".into(),
                exit_code: 0,
                passed: 3,
                failed: 0,
                duration_ms: 12,
            })
        }
    }

    fn ctx_with_stub() -> (orca_tool::ToolCtx, Arc<Stub>) {
        let stub = Arc::new(Stub::default());
        let svc: Arc<dyn InfraService> = stub.clone();
        let mut ctx = empty_ctx();
        ctx.register_service(svc);
        (ctx, stub)
    }

    #[tokio::test]
    async fn list_services_maps_each_field() {
        let (ctx, _) = ctx_with_stub();
        let out = infra_service_list(ListServicesArgs {}, &ctx).await.unwrap();
        assert_eq!(out.projects.len(), 1);
        let p = &out.projects[0];
        assert_eq!(p.project, "rebuy");
        assert_eq!(p.path, "/x");
        let s = &p.services[0];
        assert_eq!(s.name, "api");
        assert_eq!(s.state, "running");
        assert_eq!(s.health.as_deref(), Some("healthy"));
        assert_eq!(s.ports, vec!["8080:8080".to_string()]);
    }

    #[tokio::test]
    async fn get_service_logs_default_tail_is_200() {
        let (ctx, stub) = ctx_with_stub();
        let out = infra_service_detail(
            GetServiceLogsArgs {
                project: "/p".into(),
                service: "api".into(),
                tail: None,
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(out.project, "/p");
        assert_eq!(out.service, "api");
        assert_eq!(out.output, "/p/api#200");
        let req = stub.last_logs_req.lock().unwrap().clone().unwrap();
        assert_eq!(req.2, 200);
    }

    #[tokio::test]
    async fn get_service_logs_honors_explicit_tail() {
        let (ctx, stub) = ctx_with_stub();
        infra_service_detail(
            GetServiceLogsArgs {
                project: "/p".into(),
                service: "web".into(),
                tail: Some(42),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(stub.last_logs_req.lock().unwrap().as_ref().unwrap().2, 42);
    }

    #[tokio::test]
    async fn run_tests_defaults_to_rust_suite() {
        let (ctx, stub) = ctx_with_stub();
        let out = infra_test_create(RunTestsArgs { suite: None }, &ctx)
            .await
            .unwrap();
        assert_eq!(out.suite, "rust");
        assert_eq!(out.passed, 3);
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.duration_ms, 12);
        assert_eq!(
            stub.last_test_suite.lock().unwrap().as_deref(),
            Some("rust")
        );
    }

    #[tokio::test]
    async fn run_tests_honors_explicit_suite() {
        let (ctx, stub) = ctx_with_stub();
        let out = infra_test_create(
            RunTestsArgs {
                suite: Some("frontend".into()),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(out.suite, "frontend");
        assert_eq!(
            stub.last_test_suite.lock().unwrap().as_deref(),
            Some("frontend")
        );
    }
}
