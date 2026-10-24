pub mod store;

use contract::paging::{Page, PageParams};
use contract::security::Severity;
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utils::id::Id;

use store::{DAY_MS, DetailQuery, Finding, ListQuery, Retention, ScanSummary};

pub const RESOLVED_RETENTION_DAYS_SETTING: &str = "security.findings.resolved_retention_days";
pub const MAX_RESOLVED_PER_UNIT_SETTING: &str = "security.findings.max_resolved_per_unit";

fn setting<T: std::str::FromStr>(key: &str, valid: impl Fn(&T) -> bool) -> Option<T> {
    let raw = match db::pool::with_pooled_or_open(|conn| db::settings::get(conn, key)) {
        Ok(raw) => raw?,
        Err(e) => {
            tracing::warn!(setting = key, error = %e, "read security retention setting; using default");
            return None;
        }
    };
    match raw.trim().parse::<T>() {
        Ok(v) if valid(&v) => Some(v),
        _ => {
            tracing::warn!(setting = key, value = %raw, "invalid security retention setting; using default");
            None
        }
    }
}

fn retention() -> Retention {
    let mut r = Retention::default();
    if let Some(days) = setting::<i64>(RESOLVED_RETENTION_DAYS_SETTING, |d| *d >= 0) {
        r.resolved_max_age_ms = days * DAY_MS;
    }
    if let Some(cap) = setting::<usize>(MAX_RESOLVED_PER_UNIT_SETTING, |c| *c > 0) {
        r.max_resolved_per_unit = cap;
    }
    r
}

fn unit_known(id: &str) -> anyhow::Result<bool> {
    if crate::unit_identity::exists(id)? || crate::host_identity::machine_id() == id {
        return Ok(true);
    }
    db::pool::with_read_pooled_or_open(|conn| {
        Ok(db::mesh::peerdb::list_peers(conn)?
            .iter()
            .any(|p| p.peer_id == id))
    })
}

fn severity_parser() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(Severity::ALL.map(Severity::as_str))
}

fn parse_severity(raw: Option<String>) -> anyhow::Result<Option<Severity>> {
    raw.map(|s| s.parse::<Severity>().map_err(|e| anyhow::anyhow!(e)))
        .transpose()
}

fn require_verified_dispatch() -> anyhow::Result<()> {
    let Some(verb) = utils::invocation_scope::current_core_verb() else {
        return Err(contract::OrcaError::invalid(
            "security.scan: daemon required; run it through the orca daemon",
        )
        .into());
    };
    if !dispatch::tool_roles::satisfies(&verb.caller().role, "admin") {
        return Err(contract::OrcaError::forbidden(format!(
            "security.scan requires role 'admin'; caller has '{}'",
            verb.caller().role
        ))
        .into());
    }
    Ok(())
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecurityScanArgs {
    #[arg(long)]
    pub unit_id: Id,
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanner: Option<String>,
}

/// [MUTATES STATE] Run registered security scanners against one unit and
/// record their findings on this system. Requires the daemon.
#[orca_tool(
    domain = "security",
    verb = "scan",
    role = "admin",
    execute_gated = true
)]
async fn security_scan(
    args: SecurityScanArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ScanSummary> {
    require_verified_dispatch()?;
    if !unit_known(args.unit_id.as_str())? {
        return Err(
            contract::OrcaError::not_found(format!("no unit with id {}", args.unit_id)).into(),
        );
    }
    store::scan_into(
        &store::store_path()?,
        crate::host_identity::machine_id(),
        &args.unit_id,
        args.scanner.as_deref(),
        retention(),
    )
    .await
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SecurityFindingsListArgs {
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit_id: Option<Id>,
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scanner: Option<String>,
    #[arg(long, value_parser = severity_parser())]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_severity: Option<String>,
    #[arg(long)]
    pub include_resolved: bool,
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Security findings recorded on this system, open by default, highest
/// severity first.
#[orca_tool(domain = "security.findings", verb = "list", role = "admin")]
async fn security_findings_list(
    args: SecurityFindingsListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<Page<Finding>> {
    let query = ListQuery {
        unit_id: args.unit_id,
        scanner: args.scanner,
        min_severity: parse_severity(args.min_severity)?,
        include_resolved: args.include_resolved,
        page: PageParams {
            limit: args.limit,
            cursor: args.cursor,
        },
    };
    Ok(store::FindingsStore::load(&store::store_path()?)?.list(&query))
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecurityFindingsDetailArgs {
    #[arg(long)]
    pub unit_id: Id,
    #[arg(long)]
    pub scanner: String,
    #[arg(long)]
    pub id: String,
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
}

/// One security finding by unit, scanner, CVE/check id and package.
#[orca_tool(domain = "security.findings", verb = "detail", role = "admin")]
async fn security_findings_detail(
    args: SecurityFindingsDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<Finding> {
    let query = DetailQuery {
        unit_id: args.unit_id,
        scanner: args.scanner,
        id: args.id,
        package: args.package,
    };
    store::FindingsStore::load(&store::store_path()?)?
        .detail(&query)
        .cloned()
        .ok_or_else(|| {
            contract::OrcaError::not_found(format!(
                "no finding {} from {} on unit {}",
                query.id, query.scanner, query.unit_id
            ))
            .into()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verb(role: &str) -> utils::invocation_scope::CoreVerb {
        utils::invocation_scope::testing::verb(
            utils::invocation_scope::VerbCaller {
                user_id: "u".into(),
                username: "u".into(),
                role: role.into(),
                can_mutate: false,
            },
            "admin",
            true,
            false,
            true,
            false,
        )
    }

    #[test]
    fn scan_refuses_without_verified_dispatch_or_below_admin() {
        let err = require_verified_dispatch().unwrap_err();
        assert!(err.to_string().contains("daemon required"), "{err}");
        let err = utils::invocation_scope::testing::sync_core_verb(
            verb("user"),
            require_verified_dispatch,
        )
        .unwrap_err();
        assert!(err.to_string().contains("requires role 'admin'"), "{err}");
        utils::invocation_scope::testing::sync_core_verb(verb("admin"), require_verified_dispatch)
            .unwrap();
    }

    #[tokio::test]
    async fn scan_dry_run_plans_without_running() {
        const _: () = assert!(<SecurityScan as contract::OrcaToolDef>::EXECUTE_GATED);
        let dir = tempfile::tempdir().unwrap();
        let ctx = contract::ToolCtx::new(std::sync::Arc::new(contract::config::Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: contract::config::Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: dir.path().to_path_buf(),
            memory_root: dir.path().to_path_buf(),
            db_path: dir.path().join("t.db"),
            ports: Default::default(),
        }));
        let plan = dispatch::dispatch(
            "security.scan",
            serde_json::json!({ "unitId": utils::id::new(), "dryRun": true }),
            &ctx,
        )
        .await
        .expect("a dry run plans instead of hitting the daemon gate");
        assert!(plan.to_string().contains("security.scan"), "{plan}");
    }

    #[test]
    fn severity_flag_accepts_only_severity_names() {
        use clap::Parser;
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: SecurityFindingsListArgs,
        }
        let cli = Cli::try_parse_from(["t", "--min-severity", "high"]).unwrap();
        assert_eq!(
            parse_severity(cli.args.min_severity).unwrap(),
            Some(Severity::High)
        );
        assert!(Cli::try_parse_from(["t", "--min-severity", "severe"]).is_err());
    }

    #[test]
    fn all_three_verbs_are_admin() {
        use contract::OrcaToolDef;
        assert_eq!(<SecurityScan as OrcaToolDef>::REQUIRED_ROLE, "admin");
        assert_eq!(
            <SecurityFindingsList as OrcaToolDef>::REQUIRED_ROLE,
            "admin"
        );
        assert_eq!(
            <SecurityFindingsDetail as OrcaToolDef>::REQUIRED_ROLE,
            "admin"
        );
    }
}
