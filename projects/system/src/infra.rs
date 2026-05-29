//! Infra domain tools — docker compose service listing, log fetch, test
//! runner. Previously lived in the now-dissolved `infra` crate; folded into
//! fleet to drop the 3-tool single-purpose bucket.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;

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
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ListServicesOutput> {
    use std::path::PathBuf;

    let home = std::env::var("HOME").unwrap_or_default();
    let rebuy_root = std::env::var("REBUY_ROOT").unwrap_or_else(|_| format!("{home}/code/rebuy"));

    let entries = match std::fs::read_dir(&rebuy_root) {
        Ok(e) => e,
        Err(_) => {
            return Ok(ListServicesOutput {
                projects: Vec::new(),
            });
        }
    };

    let project_dirs: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.is_dir() && ::docker::Compose::find(&p).is_some() {
                Some(p)
            } else {
                None
            }
        })
        .collect();

    let mut out = Vec::with_capacity(project_dirs.len());
    for project_path in project_dirs {
        let path_str = project_path.to_string_lossy().into_owned();
        let name = project_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path_str.clone());

        let services = match ::docker::Compose::find(&project_path) {
            None => Vec::new(),
            Some(c) => match c.services().await {
                Ok(s) => s
                    .into_iter()
                    .map(|s| ServiceState {
                        name: s.name,
                        state: s.state,
                        health: if s.health.is_empty() {
                            None
                        } else {
                            Some(s.health)
                        },
                        ports: s.ports,
                    })
                    .collect(),
                Err(_) => Vec::new(),
            },
        };

        out.push(ProjectServices {
            project: name,
            path: path_str,
            services,
        });
    }

    Ok(ListServicesOutput { projects: out })
}

/// Fetch docker compose logs for a running rebuy service. Specify the project
/// path and service name.
#[orca_tool(domain = "system.infra.service", verb = "detail")]
async fn infra_service_detail(
    args: GetServiceLogsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<GetServiceLogsOutput> {
    use std::path::Path;

    let tail = args.tail.unwrap_or(200);
    let compose = ::docker::Compose::find(Path::new(&args.project))
        .ok_or_else(|| anyhow::anyhow!("no compose file found in {}", args.project))?;
    let output = compose.logs(&[args.service.as_str()], tail as u32).await?;
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
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<RunTestsOutput> {
    let suite = args.suite.as_deref().unwrap_or("rust").to_string();
    let r = run_test_suite(&suite).await?;
    Ok(RunTestsOutput {
        suite: r.suite,
        output: r.output,
        exit_code: r.exit_code,
        passed: r.passed,
        failed: r.failed,
        duration_ms: r.duration_ms,
    })
}

// ─── Test runner — moved from server/serve/api/tests_handler.rs ────────────

struct TestRunResult {
    suite: String,
    output: String,
    exit_code: i32,
    passed: u32,
    failed: u32,
    duration_ms: u64,
}

async fn run_test_suite(suite: &str) -> anyhow::Result<TestRunResult> {
    let source_root = std::env::var("ORCA_SOURCE_ROOT")
        .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string());
    let site_root = format!("{source_root}/site");

    let start = std::time::Instant::now();

    let (output, exit_code) = match suite {
        "rust" => run_command("cargo", &["test", "--color=never"], &source_root).await?,
        "frontend" => {
            run_command("npx", &["vitest", "run", "--reporter=verbose"], &site_root).await?
        }
        "e2e" => {
            run_command(
                "npx",
                &["playwright", "test", "--reporter=list"],
                &site_root,
            )
            .await?
        }
        "all" => {
            let mut combined = String::new();
            let mut total_exit = 0i32;
            for s in &["rust", "frontend", "e2e"] {
                combined.push_str(&format!("\n=== {} ===\n", s.to_uppercase()));
                let (out, code) = run_command(
                    if *s == "rust" { "cargo" } else { "npx" },
                    &match *s {
                        "rust" => vec!["test", "--color=never"],
                        "frontend" => vec!["vitest", "run", "--reporter=verbose"],
                        _ => vec!["playwright", "test", "--reporter=list"],
                    },
                    if *s == "rust" {
                        &source_root
                    } else {
                        &site_root
                    },
                )
                .await?;
                combined.push_str(&out);
                if code != 0 {
                    total_exit = code;
                }
            }
            (combined, total_exit)
        }
        _ => anyhow::bail!("unknown suite: {suite}. Valid: rust | frontend | e2e | all"),
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    let (passed, failed) = parse_test_counts(&output, suite);

    Ok(TestRunResult {
        suite: suite.to_string(),
        output,
        exit_code,
        passed,
        failed,
        duration_ms,
    })
}

async fn run_command(cmd: &str, args: &[&str], cwd: &str) -> anyhow::Result<(String, i32)> {
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .await?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let code = out.status.code().unwrap_or(-1);
    Ok((combined, code))
}

fn parse_test_counts(output: &str, suite: &str) -> (u32, u32) {
    match suite {
        "rust" => {
            for line in output.lines() {
                if line.contains("test result:") {
                    let passed = extract_count(line, "passed");
                    let failed = extract_count(line, "failed");
                    return (passed, failed);
                }
            }
            (0, 0)
        }
        "frontend" => {
            let passed = output
                .lines()
                .filter(|l| l.contains("passed"))
                .filter_map(extract_first_number)
                .next()
                .unwrap_or(0);
            let failed = output
                .lines()
                .filter(|l| l.contains("failed"))
                .filter_map(extract_first_number)
                .next()
                .unwrap_or(0);
            (passed, failed)
        }
        _ => (0, 0),
    }
}

fn extract_count(line: &str, keyword: &str) -> u32 {
    line.split_whitespace()
        .zip(line.split_whitespace().skip(1))
        .find(|(_, b)| b.starts_with(keyword))
        .and_then(|(a, _)| a.trim_end_matches(';').parse().ok())
        .unwrap_or(0)
}

fn extract_first_number(s: &str) -> Option<u32> {
    s.split_whitespace().find_map(|w| w.parse::<u32>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_test_counts_rust_extracts_pass_fail() {
        let out = "running 5 tests\n\ntest result: ok. 5 passed; 0 failed; 0 ignored; 0 measured\n";
        let (p, f) = parse_test_counts(out, "rust");
        assert_eq!(p, 5);
        assert_eq!(f, 0);
    }

    #[test]
    fn parse_test_counts_rust_returns_zero_when_no_result_line() {
        let (p, f) = parse_test_counts("nothing here", "rust");
        assert_eq!(p, 0);
        assert_eq!(f, 0);
    }

    #[test]
    fn parse_test_counts_unknown_suite_returns_zero() {
        let (p, f) = parse_test_counts("anything", "weird");
        assert_eq!(p, 0);
        assert_eq!(f, 0);
    }

    #[test]
    fn extract_count_handles_trailing_semicolon() {
        let line = "test result: ok. 42 passed; 1 failed; 0 ignored";
        assert_eq!(extract_count(line, "passed"), 42);
        assert_eq!(extract_count(line, "failed"), 1);
    }

    #[test]
    fn extract_first_number_returns_first_int() {
        assert_eq!(extract_first_number(" 7 tests passed"), Some(7));
        assert_eq!(extract_first_number("no digits"), None);
    }
}
