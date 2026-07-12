//! Unified notification dispatcher. One generic [`Event`] shape, many backends.
//!
//! This is the initial slice:
//! `Event` types, the [`Backend`] trait, a [`Dispatcher`] that fans events
//! out to a static set of registered backends, and the ntfy backend ported
//! behind the trait. Routing engine, escalation, Slack/Discord, email and SMS
//! backends are explicit follow-ups.
//!
//! Callers never branch on backend; they emit one [`Event`] and the dispatcher
//! decides who receives it.
//!
//! Crate name is `notifications` (not `notify`) — the latter collides with
//! the popular fs-watcher crate on crates.io.

use derive::orca_async;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod notify_send;

// ── Event shape ─────────────────────────────────────────────────────────────

/// Coarse event taxonomy. Routing rules (§9.3) match on this plus
/// [`Severity`] plus optional host scope.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EventClass {
    /// Low-priority "still alive" ping. Default routing keeps these on ntfy.
    Heartbeat,
    /// Reconciler-detected divergence between declared and observed state.
    Drift,
    /// Credential/cert rotation needing operator awareness.
    Rotation,
    /// Host or guest lifecycle transition (install, update, decommission).
    Lifecycle,
    /// Generic operator alert.
    Alert,
    /// Pending change awaiting approval (correlated to a `ChangeId`).
    Approval,
}

/// Severity ladder. Backends map this to their native primitive (color,
/// priority header, subject prefix) per the rendering table in the planned doc.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    Error,
    Critical,
}

/// A `key: value` data row. `inline` is a hint for grid-capable backends
/// (Slack, Discord) — text backends ignore it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    pub key: String,
    pub value: String,
    pub inline: bool,
}

/// Interactive button. Backends that can't render buttons (ntfy, email, SMS)
/// degrade gracefully — they render the action as a link or short reply code.
/// `correlation` is the `ChangeId` that the action drives via `orca apply`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    pub label: String,
    pub style: ActionStyle,
    pub correlation: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ActionStyle {
    Primary,
    Danger,
    Secondary,
}

/// The single shape every backend renders. Callers in reconcilers / detectors /
/// the scheduler construct this and call [`Dispatcher::emit`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub class: EventClass,
    pub severity: Severity,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub fields: Vec<Field>,
    #[serde(default)]
    pub actions: Vec<Action>,
    /// Ties this event back to a pending change (for ack/approve flows).
    #[serde(default)]
    pub correlation: Option<String>,
    /// Host this event is about (not necessarily the host that emitted it).
    #[serde(default)]
    pub host: Option<String>,
    /// Emitter identification, e.g. `"reconciler:lxc"`, `"scheduler"`, `"drift"`.
    pub source: String,
    /// Optional URL for backends that support a tap/click target (ntfy
    /// `X-Click`, Slack/Discord embed title link). Email/SMS render as a
    /// trailing link line.
    #[serde(default)]
    pub click: Option<String>,
}

impl Event {
    /// Minimum-friction constructor: class + severity + title + source.
    /// Use the with_* setters to add body/host/etc.
    pub fn new(
        class: EventClass,
        severity: Severity,
        title: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            class,
            severity,
            title: title.into(),
            body: String::new(),
            fields: Vec::new(),
            actions: Vec::new(),
            correlation: None,
            host: None,
            source: source.into(),
            click: None,
        }
    }

    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    pub fn with_click(mut self, click: impl Into<String>) -> Self {
        self.click = Some(click.into());
        self
    }
}

// ── Severity / Class → presentation hints ──────────────────────────────────

impl Severity {
    /// Single-emoji shortcode used as a title prefix and as the first ntfy
    /// tag. Shortcodes (not unicode) so ntfy / Slack / Discord all expand
    /// them natively.
    pub fn emoji_tag(self) -> &'static str {
        match self {
            Severity::Info => "white_check_mark",
            Severity::Warn => "warning",
            Severity::Error => "rotating_light",
            Severity::Critical => "fire",
        }
    }
}

impl EventClass {
    /// Class glyph shown after the severity emoji. Picked for visual
    /// distinctness in a notification list — heartbeat is a heartbeat,
    /// drift is a compass, rotation is a recycle arrow, etc.
    pub fn emoji_tag(self) -> &'static str {
        match self {
            EventClass::Heartbeat => "heartbeat",
            EventClass::Drift => "compass",
            EventClass::Rotation => "arrows_counterclockwise",
            EventClass::Lifecycle => "package",
            EventClass::Alert => "bell",
            EventClass::Approval => "raised_hand",
        }
    }
}

// ── Backend trait ──────────────────────────────────────────────────────────

/// Opaque per-backend reference to a previously-emitted message (Slack `ts`,
/// Discord `message_id`, ntfy `id`). Used by the future escalation logic to
/// edit existing messages with re-trigger counters / "Acked" status updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRef {
    pub backend: String,
    pub id: String,
}

impl MessageRef {
    pub fn new(backend: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            id: id.into(),
        }
    }
}

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("transport error: {0}")]
    Transport(String),
    /// Backend cannot perform the requested operation (e.g. ntfy can't
    /// edit an existing message). Caller may fall back to posting fresh.
    #[error("operation not supported by backend: {0}")]
    Unsupported(String),
}

#[orca_async]
pub trait Backend: Send + Sync {
    fn name(&self) -> &str;

    /// Render and send an event. Backends MAY drop actions if they don't
    /// support interaction; they must NOT silently drop body/title content.
    async fn emit(&self, event: &Event) -> Result<MessageRef, BackendError>;
}

// ── Routing ────────────────────────────────────────────────────────────────

/// Severity matcher. Parsed from strings like `"Warn"`, `"==Critical"`, or
/// `">=Warn"` for use in TOML route definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeverityMatch {
    pub op: SeverityOp,
    pub level: Severity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeverityOp {
    Eq,
    Gte,
}

impl SeverityMatch {
    pub fn matches(&self, sev: Severity) -> bool {
        match self.op {
            SeverityOp::Eq => sev == self.level,
            SeverityOp::Gte => sev >= self.level,
        }
    }
}

impl std::str::FromStr for SeverityMatch {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (op, rest) = if let Some(r) = s.strip_prefix(">=") {
            (SeverityOp::Gte, r.trim())
        } else if let Some(r) = s.strip_prefix("==") {
            (SeverityOp::Eq, r.trim())
        } else {
            (SeverityOp::Eq, s)
        };
        let level = match rest.to_ascii_lowercase().as_str() {
            "info" => Severity::Info,
            "warn" => Severity::Warn,
            "error" => Severity::Error,
            "critical" => Severity::Critical,
            other => return Err(format!("unknown severity `{other}`")),
        };
        Ok(SeverityMatch { op, level })
    }
}

impl<'de> Deserialize<'de> for SeverityMatch {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Route matcher. All present fields must hold (logical AND).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Match {
    #[serde(default)]
    pub class: Option<EventClass>,
    #[serde(default)]
    pub severity: Option<SeverityMatch>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

impl Match {
    pub fn matches(&self, event: &Event) -> bool {
        if let Some(c) = self.class
            && c != event.class
        {
            return false;
        }
        if let Some(s) = &self.severity
            && !s.matches(event.severity)
        {
            return false;
        }
        if let Some(h) = &self.host
            && event.host.as_deref() != Some(h.as_str())
        {
            return false;
        }
        if let Some(src) = &self.source
            && event.source != *src
        {
            return false;
        }
        true
    }
}

/// One row in the routing table. `send` is the list of backend names
/// (matching [`Backend::name`]) the event is dispatched to when `match` holds.
#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    #[serde(rename = "match", default)]
    pub matcher: Match,
    pub send: Vec<String>,
}

/// Routing config. Loadable from TOML under a `[notify]` table:
///
/// ```toml
/// [[notify.route]]
/// match = { class = "drift", severity = ">=Warn" }
/// send  = ["ntfy-alerts"]
///
/// [notify]
/// default = ["ntfy-default"]
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RoutingConfig {
    #[serde(default)]
    pub route: Vec<Route>,
    #[serde(default)]
    pub default: Vec<String>,
}

impl RoutingConfig {
    /// Parse from a TOML string with a top-level `[notify]` table (per §5).
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        #[derive(Deserialize)]
        struct Outer {
            #[serde(default)]
            notify: RoutingConfig,
        }
        let outer: Outer = toml::from_str(s)?;
        Ok(outer.notify)
    }

    /// Decide which backend names should receive `event`. Returns
    /// `default` if no routes match, or an empty vec if there are no
    /// routes AND no default (caller's responsibility to fall back).
    pub fn targets(&self, event: &Event) -> Vec<String> {
        let mut hit = false;
        let mut out: Vec<String> = Vec::new();
        for r in &self.route {
            if r.matcher.matches(event) {
                hit = true;
                for name in &r.send {
                    if !out.contains(name) {
                        out.push(name.clone());
                    }
                }
            }
        }
        if hit { out } else { self.default.clone() }
    }
}

// ── Dispatcher ─────────────────────────────────────────────────────────────

/// Per-backend outcome of [`Dispatcher::emit`]. Errors from one backend
/// never fail the dispatch as a whole — peer backends still receive the
/// event. Callers inspect this to log/escalate failures.
#[derive(Debug)]
pub struct EmitOutcome {
    pub backend: String,
    pub result: Result<MessageRef, BackendError>,
}

/// Dispatcher. Holds the registered backends and an optional [`RoutingConfig`].
///
/// Without routing configured, the dispatcher fans every event out to every
/// registered backend in registration order (the §9.2 behavior).
///
/// With routing configured, only backends named by a matching route receive
/// the event. If no route matches, [`RoutingConfig::default`] is used; if
/// that is empty, the event is dropped and the returned outcome list is empty.
pub struct Dispatcher {
    backends: Vec<Box<dyn Backend>>,
    routing: Option<RoutingConfig>,
}

impl Dispatcher {
    pub fn new() -> Self {
        Self {
            backends: Vec::new(),
            routing: None,
        }
    }

    pub fn with_backend(mut self, backend: Box<dyn Backend>) -> Self {
        self.backends.push(backend);
        self
    }

    pub fn with_routing(mut self, routing: RoutingConfig) -> Self {
        self.routing = Some(routing);
        self
    }

    pub fn register(&mut self, backend: Box<dyn Backend>) {
        self.backends.push(backend);
    }

    pub fn set_routing(&mut self, routing: RoutingConfig) {
        self.routing = Some(routing);
    }

    /// Dispatch `event` to the backends selected by the routing config (or
    /// to all backends when no routing is configured). Always returns an
    /// outcome row per dispatched backend — never short-circuits on the
    /// first failure. Backend names in the routing config that don't
    /// resolve to a registered backend are silently skipped (logging is
    /// the caller's job).
    pub async fn emit(&self, event: &Event) -> Vec<EmitOutcome> {
        let selected: Vec<&Box<dyn Backend>> = match &self.routing {
            None => self.backends.iter().collect(),
            Some(cfg) => {
                let targets = cfg.targets(event);
                targets
                    .iter()
                    .filter_map(|name| self.backends.iter().find(|b| b.name() == name))
                    .collect()
            }
        };
        let mut out = Vec::with_capacity(selected.len());
        for b in selected {
            out.push(EmitOutcome {
                backend: b.name().to_string(),
                result: b.emit(event).await,
            });
        }
        out
    }
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

// ── Process-global dispatcher ──────────────────────────────────────────────

use std::sync::{Arc, LazyLock, RwLock};

static GLOBAL: LazyLock<RwLock<GlobalState>> =
    LazyLock::new(|| RwLock::new(GlobalState::default()));

#[derive(Default)]
struct GlobalState {
    backends: Vec<Arc<dyn Backend>>,
    routing: Option<RoutingConfig>,
}

/// Register a backend with the process-global dispatcher. Each backend plugin
/// (ntfy, smtp, slack, …) calls this from its own bootstrap once per enabled
/// endpoint row. Backend names are the [`Backend::name`] string and are what
/// routing rules' `send = [...]` entries match against.
pub fn register_backend(backend: Arc<dyn Backend>) {
    let mut g = GLOBAL.write().expect("notifications global poisoned");
    let name = backend.name().to_string();
    // Replace any existing entry with the same name. A backend plugin
    // reconnecting (or a dev rebuild that re-bootstraps) would otherwise
    // duplicate the entry and fan the same event out N times.
    if let Some(slot) = g.backends.iter_mut().find(|b| b.name() == name) {
        *slot = backend;
    } else {
        g.backends.push(backend);
    }
}

/// Replace the routing config on the global dispatcher. `None` means
/// fan-out-to-all (§9.2 behavior).
pub fn set_routing(routing: Option<RoutingConfig>) {
    let mut g = GLOBAL.write().expect("notifications global poisoned");
    g.routing = routing;
}

/// Snapshot of currently-registered backend names. For observability /
/// `notify.status` tools.
pub fn registered_backend_names() -> Vec<String> {
    let g = GLOBAL.read().expect("notifications global poisoned");
    g.backends.iter().map(|b| b.name().to_string()).collect()
}

/// Emit `event` through every backend selected by the routing config (or all
/// registered backends when routing is unset). Returns one outcome per
/// dispatched backend — never short-circuits on a single backend failure.
/// When no backends are registered, returns an empty vec.
pub async fn emit(event: &Event) -> Vec<EmitOutcome> {
    let (selected, _) = {
        let g = GLOBAL.read().expect("notifications global poisoned");
        let chosen: Vec<Arc<dyn Backend>> = match &g.routing {
            None => g.backends.clone(),
            Some(cfg) => {
                let targets = cfg.targets(event);
                targets
                    .iter()
                    .filter_map(|name| g.backends.iter().find(|b| b.name() == name).cloned())
                    .collect()
            }
        };
        (chosen, ())
    };
    let mut out = Vec::with_capacity(selected.len());
    for b in selected {
        out.push(EmitOutcome {
            backend: b.name().to_string(),
            result: b.emit(event).await,
        });
    }
    out
}

/// Deregister the backend named `name`, if present. The removal path the
/// plugin reload/unload flow needs: a cdylib backend plugin's registration
/// must be reversible so unloading the library drops its backends rather than
/// leaving stale entries pointing at a dead invoke thunk. Returns `true` if a
/// backend was removed. Mirrors `storage::deregister_backend`.
pub fn deregister_backend(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("notifications global poisoned");
    let before = g.backends.len();
    g.backends.retain(|b| b.name() != name);
    before != g.backends.len()
}

// ── cdylib JSON-proxy backend ───────────────────────────────────────────────

/// The synchronous invoke thunk a cdylib plugin's notification backend is
/// driven through: `(op, args_json) -> Result<result_json, error_string>`. The
/// loader supplies a closure that marshals `op` into a `"{invoke_prefix}.{op}"`
/// tool call across the FFI `invoke` boundary. Kept as a plain `Fn` of strings
/// so this crate stays free of any dependency on the ABI/loader crates (no
/// cycle): the loader owns the FFI types, this crate owns the domain shape.
/// Mirrors `storage::InvokeThunk`.
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> Result<String, BackendError> + Send + Sync + 'static>;

/// Build and register a [`Backend`] from a plugin's backend descriptor plus an
/// [`InvokeThunk`]. The loader calls this from its domain dispatch table for
/// every `BackendDef` whose `domain == "notifications"`; each enabled ntfy /
/// slack / smtp endpoint a plugin advertises becomes one named proxy backend.
/// Registration replaces any existing backend of the same name (idempotent
/// reload), matching [`register_backend`]. Mirrors `storage::register_from_def`.
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<(), BackendError> {
    register_backend(Arc::new(NotifyProxy { name, invoke }));
    Ok(())
}

/// A [`Backend`] backed by a cdylib plugin reached over the JSON-proxy FFI
/// boundary. `emit` serializes the [`Event`] to JSON, offloads the synchronous
/// [`InvokeThunk`] onto `spawn_blocking` (so a slow/wedged plugin never blocks
/// the async runtime), and deserializes the returned [`MessageRef`].
struct NotifyProxy {
    name: String,
    invoke: InvokeThunk,
}

#[orca_async]
impl Backend for NotifyProxy {
    fn name(&self) -> &str {
        &self.name
    }

    async fn emit(&self, event: &Event) -> Result<MessageRef, BackendError> {
        let args_json = serde_json::to_string(event)
            .map_err(|e| BackendError::Transport(format!("encode `emit` args: {e}")))?;
        let invoke = self.invoke.clone();
        let out = tokio::task::spawn_blocking(move || invoke("emit", args_json))
            .await
            .map_err(|e| BackendError::Transport(format!("`emit` proxy task failed: {e}")))??;
        serde_json::from_str(&out)
            .map_err(|e| BackendError::Transport(format!("decode `emit` result: {e}")))
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct RecordingBackend {
        name: String,
        captured: Mutex<Vec<Event>>,
    }

    #[orca_async]
    impl Backend for RecordingBackend {
        fn name(&self) -> &str {
            &self.name
        }
        async fn emit(&self, event: &Event) -> Result<MessageRef, BackendError> {
            self.captured
                .lock()
                .expect("mutex poisoned")
                .push(event.clone());
            Ok(MessageRef::new(&self.name, "msg-1"))
        }
    }

    struct FailBackend;
    #[orca_async]
    impl Backend for FailBackend {
        fn name(&self) -> &str {
            "fail"
        }
        async fn emit(&self, _: &Event) -> Result<MessageRef, BackendError> {
            Err(BackendError::Transport("simulated".into()))
        }
    }

    #[tokio::test]
    async fn dispatcher_fans_out_to_all_backends_in_order() {
        let a = std::sync::Arc::new(RecordingBackend {
            name: "a".into(),
            captured: Mutex::new(Vec::new()),
        });
        let b = std::sync::Arc::new(RecordingBackend {
            name: "b".into(),
            captured: Mutex::new(Vec::new()),
        });
        struct Forward(std::sync::Arc<RecordingBackend>);
        #[orca_async]
        impl Backend for Forward {
            fn name(&self) -> &str {
                self.0.name()
            }
            async fn emit(&self, e: &Event) -> Result<MessageRef, BackendError> {
                self.0.emit(e).await
            }
        }
        let d = Dispatcher::new()
            .with_backend(Box::new(Forward(a.clone())))
            .with_backend(Box::new(Forward(b.clone())));

        let evt = Event::new(EventClass::Alert, Severity::Warn, "t", "test");
        let outcomes = d.emit(&evt).await;

        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].backend, "a");
        assert_eq!(outcomes[1].backend, "b");
        assert_eq!(a.captured.lock().expect("mutex poisoned").len(), 1);
        assert_eq!(b.captured.lock().expect("mutex poisoned").len(), 1);
    }

    #[tokio::test]
    async fn dispatcher_continues_past_failing_backend() {
        let good = std::sync::Arc::new(RecordingBackend {
            name: "good".into(),
            captured: Mutex::new(Vec::new()),
        });
        struct Forward(std::sync::Arc<RecordingBackend>);
        #[orca_async]
        impl Backend for Forward {
            fn name(&self) -> &str {
                self.0.name()
            }
            async fn emit(&self, e: &Event) -> Result<MessageRef, BackendError> {
                self.0.emit(e).await
            }
        }
        let d = Dispatcher::new()
            .with_backend(Box::new(FailBackend))
            .with_backend(Box::new(Forward(good.clone())));

        let evt = Event::new(EventClass::Alert, Severity::Info, "t", "test");
        let outcomes = d.emit(&evt).await;
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes[0].result.is_err());
        assert!(outcomes[1].result.is_ok());
        assert_eq!(good.captured.lock().expect("mutex poisoned").len(), 1);
    }

    fn make_disp_with_recorders(
        names: &[&str],
    ) -> (Dispatcher, Vec<std::sync::Arc<RecordingBackend>>) {
        struct Forward(std::sync::Arc<RecordingBackend>);
        #[orca_async]
        impl Backend for Forward {
            fn name(&self) -> &str {
                self.0.name()
            }
            async fn emit(&self, e: &Event) -> Result<MessageRef, BackendError> {
                self.0.emit(e).await
            }
        }
        let mut d = Dispatcher::new();
        let mut arcs = Vec::new();
        for n in names {
            let r = std::sync::Arc::new(RecordingBackend {
                name: (*n).to_string(),
                captured: Mutex::new(Vec::new()),
            });
            d.register(Box::new(Forward(r.clone())));
            arcs.push(r);
        }
        (d, arcs)
    }

    #[test]
    fn severity_match_parses_operators() {
        let m: SeverityMatch = ">=Warn".parse().expect("parses");
        assert_eq!(m.op, SeverityOp::Gte);
        assert_eq!(m.level, Severity::Warn);
        assert!(m.matches(Severity::Error));
        assert!(!m.matches(Severity::Info));

        let m: SeverityMatch = "==Critical".parse().expect("parses");
        assert_eq!(m.op, SeverityOp::Eq);
        assert!(m.matches(Severity::Critical));
        assert!(!m.matches(Severity::Error));

        let m: SeverityMatch = "Info".parse().expect("bare = Eq");
        assert_eq!(m.op, SeverityOp::Eq);
        assert!(m.matches(Severity::Info));

        assert!("nope".parse::<SeverityMatch>().is_err());
    }

    #[test]
    fn routing_targets_selects_matching_routes_and_dedupes() {
        let cfg = RoutingConfig::from_toml(
            r#"
[[notify.route]]
match = { class = "drift", severity = ">=Warn" }
send  = ["ntfy-alerts", "slack-ops"]

[[notify.route]]
match = { host = "charlie" }
send  = ["ntfy-alerts", "email"]

[notify]
default = ["ntfy-default"]
"#,
        )
        .expect("parses");

        let drift_warn =
            Event::new(EventClass::Drift, Severity::Warn, "t", "src").with_host("charlie");
        // both routes match → dedup ntfy-alerts
        let t = cfg.targets(&drift_warn);
        assert_eq!(t, vec!["ntfy-alerts", "slack-ops", "email"]);

        // no match → default
        let info = Event::new(EventClass::Heartbeat, Severity::Info, "t", "src");
        assert_eq!(cfg.targets(&info), vec!["ntfy-default"]);
    }

    #[tokio::test]
    async fn dispatcher_with_routing_only_hits_matched_backends() {
        let (mut d, arcs) = make_disp_with_recorders(&["ntfy-alerts", "slack-ops", "ntfy-default"]);
        d.set_routing(
            RoutingConfig::from_toml(
                r#"
[[notify.route]]
match = { class = "alert" }
send  = ["slack-ops"]

[notify]
default = ["ntfy-default"]
"#,
            )
            .expect("parses"),
        );

        let alert = Event::new(EventClass::Alert, Severity::Warn, "t", "src");
        let outcomes = d.emit(&alert).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].backend, "slack-ops");
        assert_eq!(arcs[0].captured.lock().expect("mutex").len(), 0);
        assert_eq!(arcs[1].captured.lock().expect("mutex").len(), 1);
        assert_eq!(arcs[2].captured.lock().expect("mutex").len(), 0);

        // unmatched → falls to default
        let hb = Event::new(EventClass::Heartbeat, Severity::Info, "t", "src");
        let outcomes = d.emit(&hb).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].backend, "ntfy-default");
    }

    #[tokio::test]
    async fn dispatcher_skips_unknown_backend_names() {
        let (mut d, arcs) = make_disp_with_recorders(&["a"]);
        d.set_routing(RoutingConfig {
            route: vec![Route {
                matcher: Match::default(),
                send: vec!["a".into(), "ghost".into()],
            }],
            default: vec![],
        });
        let evt = Event::new(EventClass::Alert, Severity::Info, "t", "src");
        let outcomes = d.emit(&evt).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].backend, "a");
        assert_eq!(arcs[0].captured.lock().expect("mutex").len(), 1);
    }

    #[test]
    fn severity_orders_lowest_to_highest() {
        assert!(Severity::Info < Severity::Warn);
        assert!(Severity::Warn < Severity::Error);
        assert!(Severity::Error < Severity::Critical);
    }
}
