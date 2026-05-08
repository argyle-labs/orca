use axum::{
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};

use super::{TestRunQuery, TestRunResponse, prelude::*};

// ── GET /api/tests/run ────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/tests/run",
    operation_id = "runTests",
    params(
        ("suite" = String, Query, description = "Test suite to run: rust | frontend | e2e | all"),
    ),
    responses(
        (status = 200, description = "Test run result", body = TestRunResponse),
        (status = 500, description = "Runner error", body = ErrorResponse),
    ),
    tag = "tests"
)]
pub async fn tests_run_handler(Query(params): Query<TestRunQuery>) -> Response {
    let result = run_test_suite(&params.suite).await;
    match result {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn run_test_suite(suite: &str) -> anyhow::Result<TestRunResponse> {
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

    Ok(TestRunResponse {
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
    use super::super::ctx7::extract_library_id;
    use super::super::docker::parse_compose_ps;
    use super::super::schema::parse_mysql_tsv;

    // ── parse_mysql_tsv ────────────────────────────────────────────────────────

    #[test]
    fn parse_mysql_tsv_normal() {
        let raw = "foo\tbar\nbaz\tqux\n";
        let cols = &["col1", "col2"];
        let rows = parse_mysql_tsv(raw, cols);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["col1"], "foo");
        assert_eq!(rows[0]["col2"], "bar");
        assert_eq!(rows[1]["col1"], "baz");
    }

    #[test]
    fn parse_mysql_tsv_empty_input() {
        let rows = parse_mysql_tsv("", &["col1"]);
        assert!(rows.is_empty());
    }

    #[test]
    fn parse_mysql_tsv_short_row_fills_empty_strings() {
        let raw = "only_one_field\n";
        let cols = &["col1", "col2", "col3"];
        let rows = parse_mysql_tsv(raw, cols);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["col1"], "only_one_field");
        assert_eq!(rows[0]["col2"], "");
        assert_eq!(rows[0]["col3"], "");
    }

    // ── parse_compose_ps ──────────────────────────────────────────────────────

    #[test]
    fn parse_compose_ps_normal_service() {
        let line = r#"{"Service":"web","State":"running","Health":"healthy","Publishers":[{"PublishedPort":8080,"TargetPort":80}]}"#;
        let result = parse_compose_ps(line);
        let svc = result.get("web").expect("web service missing");
        assert_eq!(svc.state, "running");
        assert_eq!(svc.health, "healthy");
        assert_eq!(svc.ports, vec!["8080:80"]);
    }

    #[test]
    fn parse_compose_ps_filters_zero_published_port() {
        let line = r#"{"Service":"worker","State":"running","Health":"","Publishers":[{"PublishedPort":0,"TargetPort":9000}]}"#;
        let result = parse_compose_ps(line);
        let svc = result.get("worker").expect("worker service missing");
        assert!(
            svc.ports.is_empty(),
            "zero-port should be filtered: {:?}",
            svc.ports
        );
    }

    #[test]
    fn parse_compose_ps_skips_malformed_lines() {
        let raw = "not json\n{\"Service\":\"ok\",\"State\":\"running\",\"Health\":\"\",\"Publishers\":[]}";
        let result = parse_compose_ps(raw);
        assert_eq!(result.len(), 1);
        assert!(result.contains_key("ok"));
    }

    #[test]
    fn parse_compose_ps_empty_input() {
        let result = parse_compose_ps("");
        assert!(result.is_empty());
    }

    // ── extract_library_id ────────────────────────────────────────────────────

    #[test]
    fn extract_library_id_from_json() {
        let json = r#"{"libraries":[{"id":"/vercel/next.js","name":"Next.js"}]}"#;
        let result = extract_library_id(json);
        assert_eq!(
            result,
            Some(("/vercel/next.js".to_string(), "Next.js".to_string()))
        );
    }

    #[test]
    fn extract_library_id_regex_fallback() {
        let text = "See /tanstack/react-query for details";
        let result = extract_library_id(text);
        let (id, title) = result.expect("should match via regex");
        assert_eq!(id, "/tanstack/react-query");
        assert_eq!(title, "react-query");
    }

    #[test]
    fn extract_library_id_returns_none_for_no_match() {
        let result = extract_library_id("no library here at all");
        assert!(result.is_none());
    }
}
