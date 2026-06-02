# Notifications — scope

`projects/notify/` — generic notification dispatcher with pluggable
backends. Many backends active at once, routed by event class +
severity + per-host policy.

> **HARD RULE — one unified surface.** Orca emits **one** generic
> `Event` shape. Every backend — ntfy, Slack, Discord, email, SMS,
> webhook, future Matrix/Telegram/Pagerduty — receives the same
> `Event` and renders it into its native primitives. **Callers in
> the daemon never know which backend will receive the event**;
> they just emit. Email is held to the same shape as Slack and
> Discord (it just renders actions as links instead of buttons).
> No backend gets a special-case API. No caller calls into a
> specific backend.

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

## 3a. Rendering convention per backend

Every backend takes the **same** `Event` and renders it. The
unification rule is: callers never branch on backend; backends
never get bespoke fields. If a backend can't represent something
natively (e.g. email can't show a click button), it **degrades
gracefully**, never silently drops content.

| Field | Slack/Discord | ntfy | Email | SMS |
|---|---|---|---|---|
| `title` | header block / embed title | `X-Title` header | Subject | first line |
| `body` (markdown) | section blocks (mrkdwn) | message body | html body | plain text, truncated to 160 |
| `severity` | color accent | `X-Priority` | colored banner | prefix `[CRIT]` etc. |
| `fields[]` | field grid | rendered as `key: value` lines | html `<dl>` | comma-joined `key=value` |
| `actions[]` | buttons | listed as `X-Actions` (ntfy supports HTTP-action headers) | rendered as `<a href="https://orca.../apply/<change_id>?action=approve">Approve</a>` links | replies: text `apply <change_id>` |
| `correlation` | thread / message edit | message id | reply-to threading | inline `[id:<change_id>]` |

The action-link target for email/SMS is an orca HTTPS endpoint
that **requires authentication** (per-user signed link with
short TTL) — clicking the link does not bypass auth, it
authenticates the operator into the same `orca apply` path.

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
| email (SMTP) | out + in (mailto-link callbacks) | actions-as-links | subject = title, html body renders fields + buttons-as-links |
| Slack | out + in (Events API) | yes | Block Kit blocks, attachments, action buttons |
| Discord | out + in (Interactions) | yes | Embeds, components (buttons / selects) |
| SMS (Twilio etc.) | out | actions-as-shortcodes | 160-char title; body truncated; reply with `apply <id>` to ack |
| Generic webhook | out | no | configurable JSON template — receiver maps to whatever |

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

## 10. Escalation

Some events need eyes on them. If you don't ack within a window,
escalate to the next surface in the chain. A single notification
moves through the path until someone responds — Discord first
(when you're at the computer), email next (when you've stepped
away), SMS / phone after that (when something is actually broken).

> **Ack ≠ approve.** Acking an event means "I see this, stop
> paging me." It does **not** trigger `orca apply` or any other
> state change. Approvals are a separate `actions[]` button.
> Both can live on the same event — you might ack a drift
> notification to silence escalation, then decide later whether
> to apply.

### 10.1 Event flags

Three new fields on `Event`:

```rust
pub struct Event {
    // ... existing fields ...
    pub requires_ack: bool,
    pub retrigger: Option<Duration>,        // cadence to re-fire until acked
    pub escalation: Option<EscalationPolicy>,
}
```

Behavior:

- `requires_ack = false` → fires once through normal routing,
  done. `retrigger` and `escalation` ignored.
- `requires_ack = true`, **no escalation** → fires through normal
  routing, then **re-fires** on the same routes every
  `retrigger` interval until acked. Bounded by a hard ceiling
  (default 24 h; override per route).
- `requires_ack = true`, **with escalation** → fires step 0,
  re-triggers step 0 at `retrigger` cadence until either
  `advance_after` elapses (move to step 1) or operator acks
  (stop). Once at the final step, re-trigger continues at the
  same cadence until `max_total` ceiling.

The simple "no escalation" case is conceptually a single-step
chain with infinite repeats; the implementation can model it
that way or as a separate code path — what matters is the
behavior: **keep paging until acked**.

### 10.2 Escalation policy

```rust
pub struct EscalationPolicy {
    pub name: String,                 // "default", "critical", "after-hours"
    pub steps: Vec<EscalationStep>,
    pub max_total: Duration,          // hard ceiling — give up after this
}

pub struct EscalationStep {
    pub backends: Vec<BackendRef>,    // fire all of these in parallel for this step
    pub retrigger: Duration,          // re-fire interval while waiting at this step
    pub advance_after: Duration,      // when to move to the next step (None = stay)
}
```

Per-step semantics:

- **`retrigger`** — how often the **same** backends are pinged
  again while waiting at this step. E.g. Discord step with
  `retrigger = "5m"` posts to Discord, waits 5 min, re-posts if
  no ack, waits 5 min, re-posts, …
- **`advance_after`** — when to move to the next step regardless
  of re-trigger cadence. Always ≥ `retrigger`. If the final step
  has no `advance_after`, re-trigger continues until `max_total`.
- **Ack at any time** stops everything — no further re-triggers,
  no advancement.

Policies are declared once in repo config and referenced by name
on events. Example:

```toml
[[notify.escalation]]
name = "critical"
max_total = "2h"

  # At your computer: Discord. Re-poke every 2 min for 5 min, then move on.
  [[notify.escalation.steps]]
  backends      = ["discord:#orca-ops"]
  retrigger     = "2m"
  advance_after = "5m"

  # Stepped away: email. Re-send every 5 min for 15 min.
  [[notify.escalation.steps]]
  backends      = ["email:scott@…"]
  retrigger     = "5m"
  advance_after = "15m"

  # Off the grid: SMS. Ring every 5 min for 20 min.
  [[notify.escalation.steps]]
  backends      = ["sms:+1…"]
  retrigger     = "5m"
  advance_after = "20m"

  # Last resort: pager. Re-page every 15 min until max_total.
  [[notify.escalation.steps]]
  backends      = ["pagerduty:on-call"]
  retrigger     = "15m"
  # no advance_after — stay here, keep paging until acked or max_total

[[notify.escalation]]
name = "default"
max_total = "4h"

  # Re-poke Discord+ntfy every 30 min for 2 h, then email every 30 min.
  [[notify.escalation.steps]]
  backends      = ["discord:#orca-ops", "ntfy:orca-alerts"]
  retrigger     = "30m"
  advance_after = "2h"

  [[notify.escalation.steps]]
  backends      = ["email:scott@…"]
  retrigger     = "30m"
```

For events that require ack **without** escalation, the event's
own `retrigger` field drives the re-fire cadence on the original
route:

```rust
Event {
    requires_ack: true,
    retrigger: Some(Duration::from_secs(15 * 60)),  // every 15 min
    escalation: None,                                // no chain
    // ...
}
```

Routes attach policies by event class / severity:

```toml
[[notify.route]]
match     = { class = "alert", severity = "Critical" }
escalate  = "critical"

[[notify.route]]
match     = { class = "drift" }
escalate  = "default"
```

### 10.3 Ack channels

Any of these stop the escalation chain for an event:

- **Action button** (`actions[]`) clicked in Slack/Discord — emits
  an interaction → orca records ack with `via = "discord-button"`.
- **Authenticated link** clicked in email — same as above, `via
  = "email-link"`.
- **SMS reply** with `ack <change_id>` — `via = "sms-reply"`.
- **CLI**: `orca ack <event_id>` — `via = "cli"`.
- **UI**: dismiss button in the lifecycle timeline — `via = "ui"`.
- **Approve / decline** of a correlated `ChangeId` — implicitly
  acks the event ("I made a decision; you can stop paging me").

Each ack records actor + via + timestamp into the lifecycle
timeline (ROADMAP §1.9). Value/secret content never logged.

### 10.4 What "no ack" actually does

- Dispatcher tracks `(event_id, step_index, last_fired_at,
  step_entered_at)` in a SQLite table — survives daemon restart.
- Scheduler tick checks every pending event:
  - If `now - last_fired_at >= retrigger` → re-fire current
    step's backends, update `last_fired_at`. Surfaces that
    support editing (Slack, Discord) update the existing
    message with an incrementing counter ("re-trigger 3 of n");
    surfaces that don't (ntfy, email, SMS) post a new message
    referencing the original.
  - If `now - step_entered_at >= advance_after` → move to next
    step, reset `step_entered_at`, fire new step.
  - Ack at any point clears the pending row and emits the
    "Acked via X" update through every backend that previously
    fired for this event.
- After `max_total`, the event terminates with state `expired`.
  A final "escalation expired without ack" notification is sent
  via the **first** backend in the chain (so the operator knows
  later that orca gave up).
- An expired event remains visible in the timeline; the operator
  can re-ack or re-escalate from there.

### 10.5 Parallel ack on multi-backend steps

When a step fires multiple backends at once (e.g. Discord + ntfy
in the `default` policy), **any** of them acking stops the chain
— operator only needs to respond once. The other backends get an
update message ("Acked at HH:MM via Discord by @user") so the
notification surface stays consistent.

### 10.6 Quiet hours / DnD

Deferred to a follow-up. v1 escalates 24/7. Workaround: use
different escalation policies per class (e.g. info-class events
use `default`, critical events use `critical-with-sms`); the
operator routes accordingly.

### 10.7 Suppression

To prevent runaway pages on stuck failures, the dispatcher
de-duplicates events with the same `(class, host, source,
correlation)` key within a 5-minute window — counts the repeat
on the existing event instead of starting a new escalation.

---

## 11. Cross-refs

- `docs/planned/secrets-identity.md` §2.2.4 (rotation → notification)
- `docs/planned/lxc-vm-reconciler.md` §drift → notification
- `docs/planned/host-lifecycle.md` (update events)
- ROADMAP §1.11 (rotation detection → pending-change notification)
- ROADMAP §1.4 (drift detection)
- ROADMAP §1.9 (observability — notifications are the user-facing edge of the lifecycle timeline)
