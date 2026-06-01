# Local-First Model

brain routes all AI work to a local model by default. Claude is escalation-only.

## Why local first

- **Privacy** — session logs, project memory, and code context never leave the machine during normal use
- **Speed** — local models respond without network latency
- **Cost** — no API cost for exploratory or repetitive work
- **Availability** — works without internet

## LM Studio backend

The default backend (`src/backend/lmstudio.rs`) speaks the OpenAI-compatible API that LM Studio exposes on `http://localhost:1234`. Any model loaded in LM Studio works — brain sends the same message format regardless of the underlying model.

Override the URL with `LMSTUDIO_URL` if LM Studio is running on a different port or host.

## Claude escalation

Claude (`src/backend/claude.rs`) is invoked in two ways:

1. **`/escalate <question>`** — user explicitly sends one question to Claude and injects the answer back into the local session context
2. **`brain escalate "<question>"`** — same as above, non-interactive

The API key is stored in macOS Keychain (`brain login`), not in env files or config — it would be too easy to accidentally commit or expose. The env var `ANTHROPIC_API_KEY` overrides the keychain if set (CI/CD use case).

## Osprey — the escalation gate

The `osprey` agent is the escalation judge. When Wolf is unsure whether to escalate, it delegates to Osprey to decide. Osprey evaluates whether the task has exceeded what the local model can handle reliably. This prevents unnecessary API calls while still allowing escalation when it genuinely helps.

## Backend trait

Both backends implement `ModelBackend`:

```rust
trait ModelBackend {
    async fn stream_response(&self, messages: &[Message], system: &str, tools: &[ToolDef])
        -> Result<BackendResponse>;
}
```

Switching backends mid-session via `/model` just swaps which implementation `session.rs` holds. No session state is lost.
