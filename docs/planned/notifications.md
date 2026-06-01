# Notifications — scope

`projects/notify/` — generic notification dispatcher with pluggable
backends. Many backends active at once, routed by event class +
severity + per-host policy.

> **HARD RULE — user-triggered changes only.** Interactive
> backends (Discord, Slack) may surface action buttons (approve /
> decline / ack) that drive `orca apply <change_id>`. The user
> still makes every decision; the chat surface is just an
> additional input channel to the same operator-gated apply path.
> No backend ever auto-approves.

---

## 1. Why

Notifications need to be **wide-ranging** — Slack, Discord
(interactive), email, ntfy, and "others" (Pushover, Matrix,
Telegram, generic webhook). Today only ntfy exists, as a thin
library demoted from a plugin, with no abstraction.

The notification *content* is generic — every modern chat
platform expresses the same primitives:

| Slack Block Kit | Discord Embed | ntfy header | Email |
|---|---|---|---|
| `header` block | `title` | `X-Title` | Subject |
| `section` blocks | `description` + `fields[]` | body + `X-Tags` | body |
| color via `attachments` | `color` | `X-Priority` | (none — convention) |
| `actions` block w/ buttons | components: buttons | `X-Actions` | (none) |

Implementations are agnostic. A Discord notification block and a
Slack notification block aren't that different. Orca emits one
generic `Event` and the active backends render it.

---

## 2. Crate shape

```
projects/notify/
  src/
    lib.rs              ← Event, Severity, Action, dispatcher
    backend.rs          ← Backend trait
    routing.rs          ← class/severity/host → backend selection
    backends/
      ntfy.rs           ← ports the existing projects/plugins/ntfy/ code
      email.rs          ← SMTP
      slack.rs          ← Block Kit webhook + Events API
      discord.rs        ← Embed webhook + Interactions endpoint
      webhook.rs        ← generic JSON POST (escape hatch)
```

Each backend is a sub-module (or sub-crate behind a feature flag
if dependency weight matters). Active backends are listed in
config; events fan out per routing rules.

---

## 3. The generic Event

```rust
pub struct Event {
    pub class: EventClass,        // "drift", "rotation", "lifecycle", "alert", "approval"
    pub severity: Severity,       // Info | Warn | Error | Critical
    pub title: String,            // one line
    pub body: String,             // markdown subset
    pub fields: Vec<Field>,       // (key, value, inline) — render as Slack/Discord field grid
    pub actions: Vec<Action>,     // interactive — see §4
    pub correlation: Option<ChangeId>,  // ties back to a pending change
    pub host: Option<HostId>,
    pub source: String,           // "reconciler:lxc", "scheduler", "drift", "rotation"
}

pub enum Severity { Info, Warn, Error, Critical }

pub struct Field { pub key: String, pub value: String, pub inline: bool }

pub struct Action {
    pub id: String,               // "approve", "decline", "ack"
    pub label: String,            // "Apply change"
    pub style: ActionStyle,       // Primary | Danger | Secondary
    pub correlation: ChangeId,    // what apply does this drive
}
```

**Severity → color mapping** (consistent across backends):

| Severity | Slack/Discord color | ntfy priority |
|---|---|---|
| Info | `#3498db` (blue) | 3 (default) |
| Warn | `#f39c12` (amber) | 4 |
| Error | `#e74c3c` (red) | 5 |
| Critical | `#9b1c1c` (deep red) | 5 + tags `urgent` |

---

## 4. Interactive backends

Slack and Discord support **action buttons** in messages. Orca
uses them as an additional input surface for `orca apply`:

1. Drift detector / rotation detection / reconciler creates a
   `ChangeId` and emits an `Event` with `actions = [Approve,
   Decline]`. Active backends render the buttons.
2. Operator clicks **Approve** in Discord.
3. Discord posts to orca's interaction endpoint (per-deployment
   URL, signed payload validation).
4. Orca verifies the signature, looks up `ChangeId`, runs the
   same code path as `orca apply <change-id>` from CLI.
5. Result is posted back to the same thread / channel as an
   updated message (button replaced with "Applied at HH:MM by
   @user").

**Authentication** — the chat user's identity must map to an orca
operator with `apply` permission. Default mapping: chat user ID →
orca user via a one-time pairing (`orca user link discord <id>`).
Unmapped users see disabled buttons.

**Single-operator default** — for the homelab, one mapped user
suffices. Multi-operator approval (k-of-n) is Phase 3.

---

## 5. Routing

Per-event routing decides which backends receive which events.
Declarative in repo config:

```toml
[[notify.route]]
match  = { class = "drift", severity = ">=Warn" }
send   = ["slack:#orca-ops", "ntfy:orca-alerts"]

[[notify.route]]
match  = { class = "approval" }
send   = ["discord:#orca-approvals"]   # interactive — show buttons here
                                       # not on ntfy where buttons can't render

[[notify.route]]
match  = { class = "lifecycle", host = "freyr" }
send   = ["email:scott@…", "ntfy:orca-freyr"]
```

Defaults if no route matches: `["ntfy:orca-default"]`.

---

## 6. Backend matrix

| Backend | Direction | Interactive | Native primitives |
|---|---|---|---|
| ntfy | out | no | title, body, priority, tags, click URL |
| email (SMTP) | out | no | subject, html+text body |
| Slack | out + in (Events API) | yes | Block Kit blocks, attachments, actions |
| Discord | out + in (Interactions) | yes | Embeds, components (buttons / selects) |
| Generic webhook | out | no | configurable JSON template |

Future: Matrix, Telegram, Pushover, Pagerduty (for on-call
escalation). Each is one file under `backends/` implementing the
`Backend` trait.

---

## 7. Backend trait

```rust
#[async_trait]
pub trait Backend: Send + Sync {
    fn name(&self) -> &str;

    /// Render and send an event. Backends MAY drop actions if
    /// they don't support interaction.
    async fn emit(&self, event: &Event) -> Result<MessageRef, Error>;

    /// Update a previously-emitted message (e.g. "button replaced
    /// with 'Applied'"). Backends that can't edit (ntfy, email)
    /// return Err(Unsupported) and the dispatcher posts a new
    /// message instead.
    async fn update(&self, ref_: &MessageRef, event: &Event) -> Result<(), Error>;

    /// Optional: register an incoming-event handler for
    /// interactive backends.
    fn subscribe(&self, _sink: ApprovalSink) -> Option<IncomingTask> { None }
}
```

`MessageRef` is opaque per-backend (Slack `ts`, Discord
`message_id`, ntfy `id`).

---

## 8. Migration from current ntfy code

`projects/plugins/ntfy/` becomes `projects/notify/backends/ntfy.rs`
verbatim — same `send` + `heartbeat` surface, now adapted to the
`Backend` trait. Callers stop importing `ntfy` directly; they
call `notify::emit(Event { ... })`.

Heartbeat becomes a special event class:

```rust
notify::emit(Event {
    class: EventClass::Heartbeat,
    severity: Severity::Info,
    title: format!("{} alive", host_id),
    ...
});
```

Routing keeps heartbeats on ntfy by default; nobody wants them in
Slack.

---

## 9. Work breakdown

| # | Item | Size | Notes |
|---|---|---|---|
| 9.1 | `projects/notify/` crate scaffold + `Event` + `Backend` trait + dispatcher | M | |
| 9.2 | Port ntfy backend | S | Lift existing code into `backends/ntfy.rs` |
| 9.3 | Routing engine + TOML config | M | Declarative routes per §5 |
| 9.4 | Email (SMTP) backend | M | |
| 9.5 | Slack backend (webhook out + Events API in) | L | Block Kit renderer + signed-request validation |
| 9.6 | Discord backend (webhook out + Interactions in) | L | Embed renderer + signed-request validation + button → apply path |
| 9.7 | `orca user link <backend> <id>` for interactive backends | S | Operator-to-chat-user mapping |
| 9.8 | Generic webhook backend | S | Configurable JSON template |
| 9.9 | Retire `projects/plugins/ntfy/` directory | S | After 9.2 lands |
| 9.10 | Wire reconcilers + drift + rotation detectors to emit events | M | Touches several call sites |

---

## 10. Cross-refs

- `docs/planned/secrets-identity.md` §2.2.4 (rotation → notification)
- `docs/planned/lxc-vm-reconciler.md` §drift → notification
- `docs/planned/host-lifecycle.md` (update events)
- ROADMAP §1.11 (rotation detection → pending-change notification)
- ROADMAP §1.4 (drift detection)
- ROADMAP §1.9 (observability — notifications are the user-facing edge of the lifecycle timeline)
