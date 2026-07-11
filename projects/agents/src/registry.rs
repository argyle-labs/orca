//! Agent / hook / prompt composition registry.
//!
//! The core composition seam for the capability-registry architecture
//! (see `docs/CAPABILITY-REGISTRIES.md`). Any plugin can contribute
//! **subagents**, **hooks**, and **CLAUDE.md prompt fragments** by registering
//! an [`AgentProvider`]. Core **composes** every registered provider's
//! contributions into the two sinks that consume them:
//!
//! 1. the materialized Claude Code config (`~/.claude/CLAUDE.md` +
//!    `~/.claude/agents/*` + hook entries in `settings.json`), written by
//!    `orca install`; and
//! 2. the internal chat's subagent roster (`conversation`).
//!
//! Mirrors the `projects/service` registry shape (trait + process-global
//! `LazyLock<RwLock<..>>`); all payloads are typed (no opaque JSON).
//! The base roster (wolf/otter/…) becomes an external `argyle-labs/agents`
//! plugin that registers as an `AgentProvider` — nothing here is
//! agent-specific by name.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, RwLock};

/// A subagent contributed by a provider. `body` is the full markdown file
/// (YAML frontmatter + prompt), written verbatim to `~/.claude/agents/<name>.md`
/// and parsed for the internal-chat roster.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentDef {
    /// kebab-case agent name — the file stem and picker id.
    pub name: String,
    /// Full markdown: frontmatter + prompt body, ready to write verbatim.
    pub body: String,
    /// Contributing provider name — for precedence reporting.
    pub origin: String,
}

/// The Claude Code lifecycle events a hook can bind to. Typed — never a raw
/// string — so composition can validate and group by event.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "PascalCase")]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PreCompact,
    PostCompact,
    Stop,
    Notification,
    SessionStart,
    UserPromptSubmit,
}

/// A hook contributed by a provider, composed into `settings.json`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HookDef {
    pub event: HookEvent,
    /// Tool matcher (e.g. `"Write|Edit"`); empty string means "all".
    #[serde(default)]
    pub matcher: String,
    /// Shell command to run.
    pub command: String,
    pub origin: String,
}

/// A CLAUDE.md fragment contributed by a provider, composed under its own
/// heading into the global directive file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PromptFragment {
    pub heading: String,
    pub body: String,
    pub origin: String,
}

/// One file inside a skill bundle. `path` is relative to the skill's own
/// directory (e.g. `SKILL.md`, `scripts/run.sh`, `reference/api.md`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillFile {
    pub path: String,
    pub contents: String,
}

/// A Claude Code skill contributed by a provider, materialized to
/// `~/.claude/skills/<name>/` (a directory with `SKILL.md` + supporting files).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillDef {
    /// kebab-case skill name — the directory name and invocation id.
    pub name: String,
    /// All files in the bundle; must include a `SKILL.md`.
    pub files: Vec<SkillFile>,
    pub origin: String,
}

/// A Claude Code slash command contributed by a provider, materialized to
/// `~/.claude/commands/<name>.md`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CommandDef {
    /// Command name (invoked as `/<name>`).
    pub name: String,
    /// Full markdown body (frontmatter + prompt), written verbatim.
    pub body: String,
    pub origin: String,
}

/// A plugin's contribution to Claude/chat composition. One plugin registers a
/// single provider exposing any subset of the artifact kinds Claude Code
/// accepts — agents, hooks, skills, slash commands, and CLAUDE.md fragments —
/// the "one plugin, many contributions" property of the capability model.
/// Each accessor defaults to empty, so a provider implements only what it has;
/// new Claude-acceptable artifact kinds are added as further defaulted methods.
pub trait AgentProvider: Send + Sync {
    /// Unique provider name (used for precedence + reporting).
    fn name(&self) -> &str;
    fn agents(&self) -> Vec<AgentDef> {
        Vec::new()
    }
    fn hooks(&self) -> Vec<HookDef> {
        Vec::new()
    }
    fn skills(&self) -> Vec<SkillDef> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandDef> {
        Vec::new()
    }
    fn prompt_fragments(&self) -> Vec<PromptFragment> {
        Vec::new()
    }
}

// ── Registry ─────────────────────────────────────────────────────────────────

static PROVIDERS: LazyLock<RwLock<Vec<Arc<dyn AgentProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register (or replace, by provider name) an agent/hook/prompt provider.
pub fn register_provider(provider: Arc<dyn AgentProvider>) {
    let mut g = PROVIDERS.write().expect("agent registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

pub fn providers() -> Vec<Arc<dyn AgentProvider>> {
    PROVIDERS.read().expect("agent registry poisoned").clone()
}

pub fn deregister_provider(name: &str) -> bool {
    let mut g = PROVIDERS.write().expect("agent registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

// ── FFI bridge ────────────────────────────────────────────────────────────────
//
// The same JSON-proxy boundary every capability domain uses (storage, service,
// cluster_roster, topology, …): a plugin cdylib advertises `domain = "agents"`
// and the loader hands us an [`InvokeThunk`] that maps an op to a
// `"{prefix}.{op}"` call across FFI, returning result/error JSON.
// [`register_from_def`] wraps that thunk in an [`FfiAgentProvider`] so an
// external plugin contributes agents/hooks/skills/commands/fragments exactly
// like the in-process [`BaseRosterProvider`] — the loader's `domain_register`
// table just adds an `"agents"` arm. No new mechanism: this is the identical
// register-from-def pattern used by every other core capability.

/// The op→JSON thunk a domain proxy drives to reach the plugin. Identical shape
/// to the loader's `BackendInvoke` and the `cluster_roster`/`topology` thunks,
/// so it passes through unwrapped.
pub type InvokeThunk = Arc<dyn Fn(&str, String) -> Result<String, String> + Send + Sync>;

/// An [`AgentProvider`] backed by a plugin across the FFI boundary. Each
/// accessor calls its op (`agents`/`hooks`/`skills`/`commands`/`prompt_fragments`)
/// with empty args and parses the returned JSON array. A transport or decode
/// failure yields an empty contribution rather than panicking — a broken plugin
/// composes to nothing instead of taking down `orca install`.
struct FfiAgentProvider {
    name: String,
    invoke: InvokeThunk,
}

impl FfiAgentProvider {
    fn fetch<T: for<'de> Deserialize<'de>>(&self, op: &str) -> Vec<T> {
        match (self.invoke)(op, "{}".to_string()) {
            Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }
}

impl AgentProvider for FfiAgentProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn agents(&self) -> Vec<AgentDef> {
        self.fetch("agents")
    }
    fn hooks(&self) -> Vec<HookDef> {
        self.fetch("hooks")
    }
    fn skills(&self) -> Vec<SkillDef> {
        self.fetch("skills")
    }
    fn commands(&self) -> Vec<CommandDef> {
        self.fetch("commands")
    }
    fn prompt_fragments(&self) -> Vec<PromptFragment> {
        self.fetch("prompt_fragments")
    }
}

/// Register a plugin-backed agent provider from a loaded `BackendDef`. Mirrors
/// `contract::cluster_roster::register_from_def` — the loader calls this for a
/// `domain = "agents"` descriptor. Idempotent by name via [`register_provider`].
pub fn register_from_def(name: String, invoke: InvokeThunk) {
    register_provider(Arc::new(FfiAgentProvider { name, invoke }));
}

/// An [`AgentProvider`] holding a plugin's composition pushed once over the
/// `agents.register` capability. Unlike [`FfiAgentProvider`] (which pulls each
/// op across FFI on demand), a subprocess plugin serializes its whole
/// contribution up front, so this just owns the decoded vecs.
struct StaticProvider {
    name: String,
    agents: Vec<AgentDef>,
    hooks: Vec<HookDef>,
    skills: Vec<SkillDef>,
    commands: Vec<CommandDef>,
    prompt_fragments: Vec<PromptFragment>,
}

impl AgentProvider for StaticProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn agents(&self) -> Vec<AgentDef> {
        self.agents.clone()
    }
    fn hooks(&self) -> Vec<HookDef> {
        self.hooks.clone()
    }
    fn skills(&self) -> Vec<SkillDef> {
        self.skills.clone()
    }
    fn commands(&self) -> Vec<CommandDef> {
        self.commands.clone()
    }
    fn prompt_fragments(&self) -> Vec<PromptFragment> {
        self.prompt_fragments.clone()
    }
}

/// Register a provider from a plugin's pushed `AgentRegistration`: each
/// vec arrives as a JSON array string. A decode failure on any field degrades to
/// an empty contribution (like [`FfiAgentProvider::fetch`]) rather than failing
/// the whole registration. Idempotent by name via [`register_provider`].
pub fn register_from_json(
    name: String,
    agents_json: &str,
    hooks_json: &str,
    skills_json: &str,
    commands_json: &str,
    prompt_fragments_json: &str,
) {
    register_provider(Arc::new(StaticProvider {
        name,
        agents: serde_json::from_str(agents_json).unwrap_or_default(),
        hooks: serde_json::from_str(hooks_json).unwrap_or_default(),
        skills: serde_json::from_str(skills_json).unwrap_or_default(),
        commands: serde_json::from_str(commands_json).unwrap_or_default(),
        prompt_fragments: serde_json::from_str(prompt_fragments_json).unwrap_or_default(),
    }));
}

/// Compose the full agent roster across all registered providers. Registration
/// order is precedence: a later provider overrides an earlier one on name
/// collision (that's how an external plugin overrides a base-roster default).
pub fn compose_agents() -> Vec<AgentDef> {
    let mut by_name: BTreeMap<String, AgentDef> = BTreeMap::new();
    for provider in providers() {
        for agent in provider.agents() {
            by_name.insert(agent.name.clone(), agent);
        }
    }
    by_name.into_values().collect()
}

/// Compose all hooks across registered providers (no dedup — every contribution
/// is a distinct binding; core groups them by `event` when writing settings).
pub fn compose_hooks() -> Vec<HookDef> {
    providers().iter().flat_map(|p| p.hooks()).collect()
}

/// Compose all skills across registered providers. Registration order is
/// precedence: a later provider overrides an earlier one on skill-name collision.
pub fn compose_skills() -> Vec<SkillDef> {
    let mut by_name: BTreeMap<String, SkillDef> = BTreeMap::new();
    for provider in providers() {
        for skill in provider.skills() {
            by_name.insert(skill.name.clone(), skill);
        }
    }
    by_name.into_values().collect()
}

/// Compose all slash commands across registered providers (later provider wins
/// on name collision).
pub fn compose_commands() -> Vec<CommandDef> {
    let mut by_name: BTreeMap<String, CommandDef> = BTreeMap::new();
    for provider in providers() {
        for command in provider.commands() {
            by_name.insert(command.name.clone(), command);
        }
    }
    by_name.into_values().collect()
}

/// Compose all CLAUDE.md fragments across registered providers.
pub fn compose_prompt_fragments() -> Vec<PromptFragment> {
    providers()
        .iter()
        .flat_map(|p| p.prompt_fragments())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProvider {
        name: &'static str,
        agents: Vec<AgentDef>,
    }
    impl AgentProvider for FakeProvider {
        fn name(&self) -> &str {
            self.name
        }
        fn agents(&self) -> Vec<AgentDef> {
            self.agents.clone()
        }
    }

    fn agent(name: &str, origin: &str) -> AgentDef {
        AgentDef {
            name: name.to_string(),
            body: format!("---\nname: {name}\n---\nbody"),
            origin: origin.to_string(),
        }
    }

    #[test]
    fn ffi_provider_parses_invoke_json_and_composes() {
        let invoke: InvokeThunk = Arc::new(|op: &str, _args: String| {
            match op {
            "agents" => Ok(
                r#"[{"name":"ffi-owl-xyz","body":"---\nname: ffi-owl-xyz\n---\nb","origin":"ext"}]"#
                    .to_string(),
            ),
            _ => Ok("[]".to_string()),
        }
        });
        register_from_def("ext-plugin-xyz".to_string(), invoke);

        let roster = compose_agents();
        let owl = roster.iter().find(|a| a.name == "ffi-owl-xyz").unwrap();
        assert_eq!(owl.origin, "ext");

        deregister_provider("ext-plugin-xyz");
    }

    #[test]
    fn ffi_provider_transport_error_composes_to_nothing() {
        let invoke: InvokeThunk = Arc::new(|_op: &str, _args: String| Err("boom".to_string()));
        register_from_def("broken-xyz".to_string(), invoke);
        assert!(compose_agents().iter().all(|a| a.origin != "broken-xyz"));
        deregister_provider("broken-xyz");
    }

    #[test]
    fn later_provider_wins_on_name_collision() {
        // Isolated by unique provider names so the global registry stays clean.
        register_provider(Arc::new(FakeProvider {
            name: "base-xyz",
            agents: vec![agent("wolf-xyz", "base-xyz")],
        }));
        register_provider(Arc::new(FakeProvider {
            name: "override-xyz",
            agents: vec![agent("wolf-xyz", "override-xyz")],
        }));
        let roster = compose_agents();
        let wolf = roster.iter().find(|a| a.name == "wolf-xyz").unwrap();
        assert_eq!(wolf.origin, "override-xyz");
        deregister_provider("base-xyz");
        deregister_provider("override-xyz");
    }
}
