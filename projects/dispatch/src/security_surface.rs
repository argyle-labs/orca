#![allow(clippy::disallowed_types)]

use anyhow::Result;
use serde_json::{Value, json};

use contract::security::{self, FindingsDetailArgs, FindingsListArgs, ScanArgs};

pub const SCAN_TOOL: &str = "security.scan";
pub const FINDINGS_LIST_TOOL: &str = "security.findings.list";
pub const FINDINGS_DETAIL_TOOL: &str = "security.findings.detail";

fn schema_value<T: schemars::JsonSchema>() -> Value {
    let mut v: Value = serde_json::to_value(schemars::schema_for!(T))
        .unwrap_or_else(|_| json!({ "type": "object" }));
    if let Some(m) = v.as_object_mut() {
        m.remove("$schema");
        m.remove("title");
    }
    v
}

pub fn security_owns(name: &str) -> bool {
    name == SCAN_TOOL || name == FINDINGS_LIST_TOOL || name == FINDINGS_DETAIL_TOOL
}

pub fn security_role_pairs() -> Vec<(&'static str, &'static str)> {
    vec![
        (SCAN_TOOL, "admin"),
        (FINDINGS_LIST_TOOL, "user"),
        (FINDINGS_DETAIL_TOOL, "user"),
    ]
}

pub fn security_mutation_names() -> Vec<&'static str> {
    vec![SCAN_TOOL]
}

pub fn security_mcp_defs() -> Vec<Value> {
    vec![
        json!({
            "name": SCAN_TOOL,
            "description": "Run registered security scanners against one unit and record their findings.",
            "inputSchema": schema_value::<ScanArgs>(),
        }),
        json!({
            "name": FINDINGS_LIST_TOOL,
            "description": "List security findings (open by default), filterable by unit, scanner and minimum severity.",
            "inputSchema": schema_value::<FindingsListArgs>(),
        }),
        json!({
            "name": FINDINGS_DETAIL_TOOL,
            "description": "Show one security finding by unit, scanner, id and package.",
            "inputSchema": schema_value::<FindingsDetailArgs>(),
        }),
    ]
}

pub async fn security_dispatch(name: &str, args: &Value) -> Option<Result<Value>> {
    match name {
        SCAN_TOOL => Some(run_scan(args).await),
        FINDINGS_LIST_TOOL => Some(run_list(args)),
        FINDINGS_DETAIL_TOOL => Some(run_detail(args)),
        _ => None,
    }
}

async fn run_scan(args: &Value) -> Result<Value> {
    let a: ScanArgs = crate::erased::parse_named_args(SCAN_TOOL, args.clone())?;
    let summary = security::scan(a).await?;
    serde_json::to_value(&summary).map_err(|e| anyhow::anyhow!("encode scan summary: {e}"))
}

fn run_list(args: &Value) -> Result<Value> {
    let a: FindingsListArgs = if args.is_null() {
        FindingsListArgs::default()
    } else {
        crate::erased::parse_named_args(FINDINGS_LIST_TOOL, args.clone())?
    };
    let page = security::findings_list(&a)?;
    serde_json::to_value(&page).map_err(|e| anyhow::anyhow!("encode findings: {e}"))
}

fn run_detail(args: &Value) -> Result<Value> {
    let a: FindingsDetailArgs =
        crate::erased::parse_named_args(FINDINGS_DETAIL_TOOL, args.clone())?;
    let finding = security::findings_detail(&a)?;
    serde_json::to_value(&finding).map_err(|e| anyhow::anyhow!("encode finding: {e}"))
}

pub fn security_cli_command() -> clap::Command {
    let unit_arg = clap::Arg::new("unit_id").long("unit-id").value_name("UUID");
    let scanner_arg = clap::Arg::new("scanner").long("scanner").value_name("NAME");
    clap::Command::new("security")
        .about("Scan units for vulnerabilities and exposure (plugin-driven scanners)")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            clap::Command::new("scan")
                .about("Run security scanners against one unit")
                .arg(unit_arg.clone().required(true))
                .arg(scanner_arg.clone()),
        )
        .subcommand(
            clap::Command::new("findings")
                .about("Inspect recorded security findings")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(
                    clap::Command::new("list")
                        .about("List security findings")
                        .arg(unit_arg.clone())
                        .arg(scanner_arg.clone())
                        .arg(
                            clap::Arg::new("min_severity")
                                .long("min-severity")
                                .value_name("SEVERITY")
                                .value_parser(["unknown", "low", "medium", "high", "critical"]),
                        )
                        .arg(
                            clap::Arg::new("include_resolved")
                                .long("include-resolved")
                                .action(clap::ArgAction::SetTrue),
                        )
                        .arg(clap::Arg::new("limit").long("limit").value_name("N"))
                        .arg(clap::Arg::new("cursor").long("cursor").value_name("CURSOR")),
                )
                .subcommand(
                    clap::Command::new("detail")
                        .about("Show one security finding")
                        .arg(unit_arg.required(true))
                        .arg(scanner_arg.required(true))
                        .arg(
                            clap::Arg::new("id")
                                .long("id")
                                .required(true)
                                .value_name("ID"),
                        )
                        .arg(
                            clap::Arg::new("package")
                                .long("package")
                                .value_name("PACKAGE"),
                        ),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_only_the_three_ops() {
        assert!(security_owns(SCAN_TOOL));
        assert!(security_owns(FINDINGS_LIST_TOOL));
        assert!(security_owns(FINDINGS_DETAIL_TOOL));
        assert!(!security_owns("security.nope"));
    }

    #[test]
    fn scan_is_admin_reads_are_user() {
        let pairs = security_role_pairs();
        assert!(pairs.contains(&(SCAN_TOOL, "admin")));
        assert!(pairs.contains(&(FINDINGS_LIST_TOOL, "user")));
        assert!(pairs.contains(&(FINDINGS_DETAIL_TOOL, "user")));
        assert_eq!(security_mutation_names(), vec![SCAN_TOOL]);
    }

    #[tokio::test]
    async fn bad_args_are_invalid_naming_the_field() {
        let err = security_dispatch(SCAN_TOOL, &json!({ "unit_id": 7 }))
            .await
            .unwrap()
            .unwrap_err();
        crate::erased::assert_invalid(&err, "unit_id");
    }

    #[tokio::test]
    async fn list_on_empty_store_is_empty_page() {
        let out = security_dispatch(FINDINGS_LIST_TOOL, &Value::Null)
            .await
            .expect("is a security op")
            .expect("ok");
        assert!(out["items"].as_array().is_some());
    }

    #[tokio::test]
    async fn unknown_name_returns_none() {
        assert!(
            security_dispatch("security.nope", &json!({}))
                .await
                .is_none()
        );
    }

    #[test]
    fn cli_parses_nested_findings_verbs() {
        let m = security_cli_command()
            .try_get_matches_from(["security", "findings", "list", "--min-severity", "high"])
            .unwrap();
        let (_, f) = m.subcommand().unwrap();
        let (op, l) = f.subcommand().unwrap();
        assert_eq!(op, "list");
        assert_eq!(l.get_one::<String>("min_severity").unwrap(), "high");
    }
}
