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

fn retention() -> Retention {
    let read = |key: &str| {
        db::pool::with_pooled_or_open(|conn| db::settings::get(conn, key))
            .ok()
            .flatten()
    };
    let mut r = Retention::default();
    if let Some(days) = read(RESOLVED_RETENTION_DAYS_SETTING).and_then(|v| v.parse::<i64>().ok())
        && days >= 0
    {
        r.resolved_max_age_ms = days * DAY_MS;
    }
    if let Some(cap) = read(MAX_RESOLVED_PER_UNIT_SETTING).and_then(|v| v.parse::<usize>().ok())
        && cap > 0
    {
        r.max_resolved_per_unit = cap;
    }
    r
}

fn severity_parser() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(Severity::ALL.map(Severity::as_str))
}

fn parse_severity(raw: Option<String>) -> anyhow::Result<Option<Severity>> {
    raw.map(|s| s.parse::<Severity>().map_err(|e| anyhow::anyhow!(e)))
        .transpose()
}

fn require_daemon() -> anyhow::Result<()> {
    if utils::invocation_scope::current_core_verb().is_none() {
        return Err(contract::OrcaError::invalid(
            "security.scan: daemon required; run it through the orca daemon",
        )
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
#[orca_tool(domain = "security", verb = "scan", role = "admin")]
async fn security_scan(
    args: SecurityScanArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ScanSummary> {
    require_daemon()?;
    if !crate::unit_identity::exists(args.unit_id.as_str())? {
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

    #[test]
    fn scan_refuses_without_the_daemon() {
        let err = require_daemon().unwrap_err();
        assert!(err.to_string().contains("daemon required"), "{err}");
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
