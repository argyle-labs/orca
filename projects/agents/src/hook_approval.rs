//! Operator approval gate for plugin-contributed hooks.
//!
//! A hook is a shell command Claude Code runs in the operator's session, so a
//! plugin's hook is withheld from [`crate::compose_hooks`] until the operator
//! approves its exact `(origin, hash)` pair. The hash covers event, matcher and
//! command, so any change to a hook re-requires approval. Approvals persist in
//! `<orca_home>/agents/hook-approvals.json`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, anyhow};
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::registry::HookDef;

pub type Approvals = BTreeSet<(String, String)>;

// Serializes read-modify-write of the approvals file within this process.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Content hash of a hook: sha256 over event, matcher and command, each
/// length-prefixed (u64 LE) so no field boundary can be shifted into another.
pub fn hook_hash(hook: &HookDef) -> String {
    let event = serde_json::to_string(&hook.event).unwrap_or_default();
    let mut h = Sha256::new();
    for part in [event.as_str(), &hook.matcher, &hook.command] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    hex::encode(h.finalize())
}

fn store_path() -> Option<PathBuf> {
    contract::config::orca_home().map(|h| h.join("agents").join("hook-approvals.json"))
}

/// Load persisted approvals. A missing or unreadable file means nothing is
/// approved — the gate fails closed.
pub fn load() -> Approvals {
    store_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn is_approved(approvals: &Approvals, hook: &HookDef) -> bool {
    approvals.contains(&(hook.origin.clone(), hook_hash(hook)))
}

/// Persist approval of `(origin, hash)`.
pub fn approve(origin: &str, hash: &str) -> anyhow::Result<()> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = store_path().ok_or_else(|| anyhow!("cannot resolve orca home"))?;
    let mut approvals = load();
    approvals.insert((origin.to_string(), hash.to_string()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    write_private_synced(&path, &serde_json::to_vec_pretty(&approvals)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Atomically replace `path`: write a uniquely named 0600 temp file in the same
/// directory (`tempfile` creates it with `O_EXCL`), fsync it, rename it over
/// `path`, then fsync the directory so the rename itself is durable.
fn write_private_synced(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    #[cfg(unix)]
    tmp.as_file()
        .set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    std::fs::File::open(dir)?.sync_all()
}

// ── Tools ───────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HookEntry {
    pub origin: String,
    pub hash: String,
    pub event: crate::registry::HookEvent,
    pub matcher: String,
    pub command: String,
    pub approved: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct ListHooksArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListHooksOutput {
    pub hooks: Vec<HookEntry>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ApproveHookArgs {
    /// Registering plugin (the hook's `origin`).
    #[arg(long)]
    pub origin: String,
    /// Hook hash as shown by `agent_hook list`.
    #[arg(long)]
    pub hash: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ApproveHookOutput {
    pub origin: String,
    pub hash: String,
    pub command: String,
}

/// List every plugin-contributed hook with its hash and approval state.
/// Unapproved hooks are never materialized into Claude settings.
#[orca_tool(domain = "agent_hook", verb = "list")]
async fn list_hooks(
    _args: ListHooksArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ListHooksOutput> {
    let approvals = load();
    let hooks = crate::registry::all_hooks()
        .into_iter()
        .map(|h| HookEntry {
            approved: is_approved(&approvals, &h),
            hash: hook_hash(&h),
            origin: h.origin,
            event: h.event,
            matcher: h.matcher,
            command: h.command,
        })
        .collect();
    Ok(ListHooksOutput { hooks })
}

/// [MUTATES STATE] Approve a plugin hook so it is materialized into Claude
/// settings. Must name a currently registered `(origin, hash)` pair.
#[orca_tool(
    domain = "agent_hook",
    verb = "approve",
    role = "admin",
    execute_gated = true
)]
async fn approve_hook(
    args: ApproveHookArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ApproveHookOutput> {
    let hook = crate::registry::all_hooks()
        .into_iter()
        .find(|h| h.origin == args.origin && hook_hash(h) == args.hash)
        .ok_or_else(|| {
            anyhow!(
                "no registered hook from '{}' with hash '{}'",
                args.origin,
                args.hash
            )
        })?;
    approve(&args.origin, &args.hash)?;
    Ok(ApproveHookOutput {
        origin: args.origin,
        hash: args.hash,
        command: hook.command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{compose_hooks, compose_unapproved_hooks, deregister_provider};

    fn register(name: &str, command: &str) {
        let hooks = format!(
            r#"[{{"event":"PreToolUse","matcher":"","command":"{command}","origin":"x"}}]"#
        );
        crate::register_from_json(name.to_string(), "[]", &hooks, "[]", "[]", "[]");
    }

    fn hook(matcher: &str, command: &str) -> HookDef {
        HookDef {
            event: crate::registry::HookEvent::PreToolUse,
            matcher: matcher.to_string(),
            command: command.to_string(),
            origin: "o".to_string(),
        }
    }

    #[test]
    fn hash_does_not_collide_across_field_boundary() {
        assert_ne!(hook_hash(&hook("a\0b", "c")), hook_hash(&hook("a", "b\0c")));
    }

    #[test]
    fn control_characters_are_rejected_at_registration() {
        let hooks = r#"[{"event":"Stop","matcher":"a\u0000b","command":"c","origin":"x"},
            {"event":"Stop","matcher":"","command":"echo\u001b[2Jx","origin":"x"},
            {"event":"Stop","matcher":"","command":"echo\tok","origin":"x"}]"#;
        crate::register_from_json("hook-ctrl-xyz".to_string(), "[]", hooks, "[]", "[]", "[]");
        let got: Vec<_> = compose_unapproved_hooks()
            .into_iter()
            .filter(|h| h.origin == "hook-ctrl-xyz")
            .collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].command, "echo\tok");
        deregister_provider("hook-ctrl-xyz");
    }

    #[test]
    fn format_characters_are_rejected_at_registration() {
        let hooks = r#"[{"event":"Stop","matcher":"","command":"echo \u202Egnp.exe","origin":"x"},
            {"event":"Stop","matcher":"Wr\u200Bite","command":"c","origin":"x"},
            {"event":"Stop","matcher":"","command":"echo plain","origin":"x"}]"#;
        crate::register_from_json("hook-cf-xyz".to_string(), "[]", hooks, "[]", "[]", "[]");
        let got: Vec<_> = crate::registry::all_hooks()
            .into_iter()
            .filter(|h| h.origin == "hook-cf-xyz")
            .collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].command, "echo plain");
        deregister_provider("hook-cf-xyz");
    }

    #[cfg(unix)]
    #[test]
    fn approvals_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        approve("perm-xyz", "h").unwrap();
        let mode = std::fs::metadata(store_path().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn unapproved_hook_is_not_composed() {
        register("hook-unapproved-xyz", "echo unapproved");
        assert!(
            compose_hooks()
                .iter()
                .all(|h| h.origin != "hook-unapproved-xyz")
        );
        assert!(
            compose_unapproved_hooks()
                .iter()
                .any(|h| h.origin == "hook-unapproved-xyz")
        );
        deregister_provider("hook-unapproved-xyz");
    }

    #[test]
    fn approved_hook_is_composed_and_change_reverts_approval() {
        register("hook-approved-xyz", "echo approved");
        let hook = compose_unapproved_hooks()
            .into_iter()
            .find(|h| h.origin == "hook-approved-xyz")
            .unwrap();
        approve(&hook.origin, &hook_hash(&hook)).unwrap();
        assert!(
            compose_hooks()
                .iter()
                .any(|h| h.origin == "hook-approved-xyz" && h.command == "echo approved")
        );

        register("hook-approved-xyz", "echo changed");
        assert!(
            compose_hooks()
                .iter()
                .all(|h| h.origin != "hook-approved-xyz")
        );
        deregister_provider("hook-approved-xyz");
    }
}
