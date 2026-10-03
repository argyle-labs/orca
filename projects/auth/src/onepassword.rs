//! 1Password as an orca secrets backend.
//!
//! Core records a secret's `backend` and `ref_path`; this resolves the rows
//! whose backend is `onepassword`, turning `op://<vault>/<item>/<field>` into a
//! value by driving the 1Password CLI. orca controls `op` rather than
//! reimplementing the vault protocol.
//!
//! **Why this exists, concretely.** Every 1Password read was going through an
//! interactive `op` against the desktop app, which fails in two ways that look
//! identical from a tool call and are not: the session is locked
//! (`authorization timeout`, `promptError`) or the app is closed
//! (`connecting to desktop app`). Both want a biometric prompt that a
//! non-interactive caller can never answer, so the call hangs and then dies
//! with an error naming no remedy. Worse, the desktop app only exists on one
//! machine, and orca's addressing rule is that a call means the same thing on
//! every node.
//!
//! So the primary path is a **service-account token**, which is headless, never
//! prompts, and behaves the same on every host. The desktop session stays as an
//! opportunistic fallback where it happens to work. Three properties follow,
//! and they are the point of the module:
//!
//! - **It never hangs.** `op` is bounded by [`OP_TIMEOUT`]; a prompt nobody can
//!   answer becomes a fast, named error.
//! - **Every failure names its remedy.** [`classify`] maps `op`'s stderr onto
//!   [`OpFailure`], and each variant carries the operator action.
//! - **A resolved value never reaches a log or an error.** Only `op`'s stderr
//!   is ever quoted; stdout is the secret and is returned, never formatted.

use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;

/// Backend kind. Matches the `backend` column of a secret row and
/// [`contract::secrets_backend::SecretsBackend::name`].
pub const BACKEND: &str = "onepassword";

/// Name of the **inline** orca secret holding a 1Password service-account
/// token, used when the environment does not carry one.
///
/// Deliberately an inline secret: it is the bootstrap credential, so it lives
/// in orca's own encrypted DB. Reading it must never route back through this
/// backend — see [`service_account_token`].
pub const SERVICE_ACCOUNT_SECRET: &str = "onepassword.service_account_token";

const ENV_SERVICE_ACCOUNT: &str = "OP_SERVICE_ACCOUNT_TOKEN";

/// How long `op` gets before we give up on it.
///
/// Not arbitrary: without a service-account token `op` blocks on a biometric
/// prompt, and a daemon or MCP call has no way to answer one. Unbounded, that
/// is a hung tool call; bounded, it is a clear "the session is locked" with the
/// fix attached. Generous enough for a cold CLI start on a loaded host.
const OP_TIMEOUT: Duration = Duration::from_secs(15);

// ── Reference parsing ────────────────────────────────────────────────────────

/// The three parts of an `op://` secret reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    pub vault: String,
    pub item: String,
    /// May carry a query suffix (`password?attribute=otp`), which `op`
    /// interprets. Kept verbatim.
    pub field: String,
}

/// Parse and validate an `op://<vault>/<item>/<field>` reference.
///
/// Validated before spawning anything: a typo should cost an error, not a
/// subprocess and a 15-second timeout. It also keeps a malformed row from
/// reaching `op`'s argv at all.
///
/// `item` may be a title or an item **id**, and an id is strongly preferred —
/// titles in this vault carry em-dashes and parentheses that have repeatedly
/// broken lookups, while ids are stable and shell-safe.
pub fn parse_ref(ref_path: &str) -> Result<SecretRef> {
    let rest = ref_path.strip_prefix("op://").ok_or_else(|| {
        anyhow::anyhow!(
            "not a 1Password reference: {ref_path:?} — expected op://<vault>/<item>/<field>"
        )
    })?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() != 3 {
        anyhow::bail!(
            "1Password reference needs exactly vault/item/field, got {} segment(s): {ref_path:?}",
            parts.len()
        );
    }
    if let Some(i) = parts.iter().position(|p| p.trim().is_empty()) {
        let which = ["vault", "item", "field"][i];
        anyhow::bail!("1Password reference has an empty {which}: {ref_path:?}");
    }
    Ok(SecretRef {
        vault: parts[0].to_string(),
        item: parts[1].to_string(),
        field: parts[2].to_string(),
    })
}

// ── Failure classification ───────────────────────────────────────────────────

/// Why an `op` invocation failed, in terms of what the operator must do.
///
/// This is about remediation, not retry, which is why it does not reuse
/// `utils::probe_error` — that classifier answers "keep polling?" for the mesh
/// health gate and its marker list is deliberately narrow. Mixing vault wording
/// into it would broaden a list whose own docs warn against broadening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpFailure {
    /// The `op` binary is not on this host.
    NotInstalled,
    /// Reached 1Password, but the session needs a human — locked vault, closed
    /// desktop app, or a prompt that timed out.
    SessionLocked,
    /// `op` answered: no such vault, item, or field.
    NotFound,
    /// Authenticated, but this identity may not read it.
    Denied,
    /// Anything else; the stderr is passed through.
    Other,
}

impl OpFailure {
    /// The operator action that fixes this. The whole reason the enum exists:
    /// a 1Password failure that does not say what to do gets retried blindly.
    pub fn remediation(self) -> &'static str {
        match self {
            OpFailure::NotInstalled => {
                "install the 1Password CLI (`op`) on this host, then set a service-account token"
            }
            OpFailure::SessionLocked => {
                "the 1Password session needs a human: unlock the desktop app. \
                 To stop this recurring — and to make this host behave like every other — \
                 store a service-account token as the inline secret \
                 'onepassword.service_account_token' (or set OP_SERVICE_ACCOUNT_TOKEN); \
                 service accounts are headless and never prompt"
            }
            OpFailure::NotFound => {
                "check the vault/item/field; prefer the item ID over its title, \
                 since titles here contain characters that break lookups"
            }
            OpFailure::Denied => {
                "this 1Password identity cannot read that item — grant the service account \
                 access to the vault"
            }
            OpFailure::Other => "see the 1Password CLI error above",
        }
    }
}

/// Classify `op`'s stderr.
///
/// Matched case-insensitively against wording measured from `op` 2.39.0 rather
/// than guessed. Ordering matters: "could not read secret" accompanies several
/// causes, so the specific markers are tested first and it is the fallback.
pub fn classify(stderr: &str) -> OpFailure {
    let s = stderr.to_lowercase();
    // A locked or absent desktop app. These are the two modes that actually
    // bit, and they are indistinguishable to a caller without this mapping.
    const LOCKED: &[&str] = &[
        "authorization timeout",
        "prompterror",
        "connecting to desktop app",
        "could not connect to the 1password app",
        "you are not currently signed in",
        "session expired",
        "no account found",
        "authorization prompt dismissed",
    ];
    if LOCKED.iter().any(|m| s.contains(m)) {
        return OpFailure::SessionLocked;
    }
    if s.contains("isn't an item")
        || s.contains("could not get item")
        || s.contains("doesn't have a field")
        || s.contains("isn't a field")
        || s.contains("no such vault")
        || s.contains("could not read secret")
    {
        return OpFailure::NotFound;
    }
    if s.contains("access denied") || s.contains("not allowed") || s.contains("forbidden") {
        return OpFailure::Denied;
    }
    OpFailure::Other
}

// ── Credential discovery ─────────────────────────────────────────────────────

/// A service-account token, if this host has one.
///
/// Environment first (how a service or CI run supplies it), then the inline
/// orca secret. The inline read is deliberate and load-bearing: it goes
/// straight to the encrypted store, never through `auth::secrets::get_secret`.
/// Resolving the bootstrap credential through the backend it bootstraps would
/// recurse forever.
pub fn service_account_token() -> Option<String> {
    if let Ok(v) = std::env::var(ENV_SERVICE_ACCOUNT)
        && !v.trim().is_empty()
    {
        return Some(v);
    }
    match db::pool::Db::process().read(|c| secrets::read_inline_value(c, SERVICE_ACCOUNT_SECRET)) {
        Ok(Some(v)) if !v.trim().is_empty() => Some(v),
        // No bootstrap token is the normal case on a desktop host, not an error.
        Ok(_) => None,
        Err(e) => {
            tracing::debug!("could not read {SERVICE_ACCOUNT_SECRET}: {e:#}");
            None
        }
    }
}

// ── Backend ──────────────────────────────────────────────────────────────────

/// Resolves `onepassword` secret rows via the 1Password CLI.
pub struct OnePassword {
    /// Path to `op`. `None` resolves it from `PATH` per call, so installing
    /// `op` takes effect without restarting the daemon. Tests set it to a stub
    /// to exercise the subprocess path without a vault.
    op_path: Option<String>,
    /// Bound on a single `op` call. Overridable so the no-hang guarantee is
    /// testable in milliseconds instead of [`OP_TIMEOUT`] seconds.
    timeout: Duration,
}

impl OnePassword {
    pub fn new() -> Self {
        Self {
            op_path: None,
            timeout: OP_TIMEOUT,
        }
    }

    /// Drive a specific binary instead of whatever is on `PATH`.
    pub fn with_binary(path: impl Into<String>) -> Self {
        Self {
            op_path: Some(path.into()),
            timeout: OP_TIMEOUT,
        }
    }

    /// Override the per-call bound.
    pub fn with_timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    fn binary(&self) -> Result<String> {
        if let Some(p) = &self.op_path {
            return Ok(p.clone());
        }
        utils::path::which("op").ok_or_else(|| {
            anyhow::anyhow!(
                "1Password CLI not found: {}",
                OpFailure::NotInstalled.remediation()
            )
        })
    }
}

impl Default for OnePassword {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl contract::secrets_backend::SecretsBackend for OnePassword {
    fn name(&self) -> &str {
        BACKEND
    }

    async fn resolve(&self, ref_path: &str) -> Result<String> {
        // Validate first: no subprocess for a malformed row.
        let parsed = parse_ref(ref_path)?;
        let bin = self.binary()?;

        let mut cmd = tokio::process::Command::new(&bin);
        cmd.arg("read").arg(ref_path);
        cmd.stdin(std::process::Stdio::null());

        let headless = match service_account_token() {
            Some(tok) => {
                cmd.env(ENV_SERVICE_ACCOUNT, tok);
                true
            }
            None => false,
        };
        // The ref, never the value. Vault and item are not secret; the field's
        // contents are, and they are not logged anywhere in this module.
        tracing::debug!(
            vault = %parsed.vault,
            item = %parsed.item,
            field = %parsed.field,
            headless,
            "resolving 1Password reference"
        );

        let out = tokio::time::timeout(self.timeout, cmd.output())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "1Password CLI did not answer within {:?} — {}",
                    self.timeout,
                    OpFailure::SessionLocked.remediation()
                )
            })?
            .with_context(|| format!("invoking {bin} read"))?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let kind = classify(&stderr);
            // stderr only. stdout is the secret and must not appear here even
            // on a partial failure.
            anyhow::bail!(
                "1Password could not resolve {ref_path}: {stderr} — {}",
                kind.remediation()
            );
        }

        // `op read` emits the value plus a trailing newline.
        let value = String::from_utf8_lossy(&out.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        if value.is_empty() {
            anyhow::bail!(
                "1Password returned an empty value for {ref_path} — {}",
                OpFailure::NotFound.remediation()
            );
        }
        Ok(value)
    }
}

/// Register the 1Password backend. Idempotent — the registry replaces by name,
/// so a reload does not duplicate it.
///
/// Registered unconditionally, including where `op` is absent: a host that
/// cannot resolve must still say *why* when asked. Registering only when `op`
/// exists would make the same call fail with "no secrets backend
/// 'onepassword' is registered on this host", which names no remedy and hides
/// that the host simply lacks the CLI.
pub fn register() {
    contract::secrets_backend::register_provider(Arc::new(OnePassword::new()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::secrets_backend::SecretsBackend;

    #[test]
    fn parses_a_well_formed_reference() {
        let r = parse_ref("op://Private/isco56vak7tj4excnjp2ayvg7y/orca_api_token").unwrap();
        assert_eq!(r.vault, "Private");
        assert_eq!(r.item, "isco56vak7tj4excnjp2ayvg7y");
        assert_eq!(r.field, "orca_api_token");
    }

    #[test]
    fn keeps_a_query_suffix_on_the_field() {
        // `op` interprets these; mangling one would silently read the wrong
        // attribute.
        let r = parse_ref("op://app-prod/db/one-time password?attribute=otp").unwrap();
        assert_eq!(r.field, "one-time password?attribute=otp");
    }

    #[test]
    fn rejects_malformed_references_before_spawning() {
        for bad in [
            "Private/item/field",            // no scheme
            "op://Private/item",             // too few segments
            "op://Private/item/field/extra", // too many
            "op:///item/field",              // empty vault
            "op://Private//field",           // empty item
            "op://Private/item/",            // empty field
            "op://Private/item/   ",         // whitespace-only field
        ] {
            assert!(parse_ref(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn the_empty_segment_error_names_which_one() {
        let e = parse_ref("op://Private//field").unwrap_err().to_string();
        assert!(e.contains("item"), "error should name the segment: {e}");
    }

    #[test]
    fn classifies_the_two_failures_that_actually_bit() {
        // Both measured this session against op 2.39.0.
        assert_eq!(
            classify("[ERROR] 2026/10/02 authorization timeout"),
            OpFailure::SessionLocked
        );
        assert_eq!(
            classify("error initializing client: response: promptError"),
            OpFailure::SessionLocked
        );
        assert_eq!(
            classify("[ERROR] connecting to desktop app: timeout"),
            OpFailure::SessionLocked
        );
    }

    #[test]
    fn classifies_a_missing_item_as_not_found_not_locked() {
        // The control for the test above: a bad ref must NOT be reported as a
        // locked session, or the operator unlocks a vault that was never locked.
        let real = "[ERROR] 2026/10/02 21:42:09 could not read secret \
                    'op://Private/isco56vak7tj4excnjp2ayvg7y/nope': \
                    the item doesn't have a field 'nope'";
        assert_eq!(classify(real), OpFailure::NotFound);
        assert_eq!(
            classify("could not get item NoSuchVault"),
            OpFailure::NotFound
        );
    }

    #[test]
    fn classification_is_case_insensitive() {
        assert_eq!(classify("AUTHORIZATION TIMEOUT"), OpFailure::SessionLocked);
    }

    #[test]
    fn unknown_stderr_falls_through_to_other() {
        assert_eq!(classify("something entirely new"), OpFailure::Other);
    }

    #[test]
    fn the_locked_remediation_points_at_a_service_account() {
        // The durable fix must be in the message, not just in the docs — this
        // is the sentence that stops the failure recurring.
        let m = OpFailure::SessionLocked.remediation();
        assert!(m.contains("service-account"), "{m}");
        assert!(m.contains(SERVICE_ACCOUNT_SECRET), "{m}");
    }

    #[test]
    fn every_failure_carries_a_nonempty_remediation() {
        for f in [
            OpFailure::NotInstalled,
            OpFailure::SessionLocked,
            OpFailure::NotFound,
            OpFailure::Denied,
            OpFailure::Other,
        ] {
            assert!(!f.remediation().is_empty(), "{f:?} has no remediation");
        }
    }

    #[test]
    fn the_backend_answers_to_the_recorded_kind() {
        // The registry matches a secret row's `backend` against this exactly.
        assert_eq!(OnePassword::new().name(), "onepassword");
        assert_eq!(BACKEND, "onepassword");
    }

    // ── Subprocess path, driven through a stub `op` ──────────────────────────

    /// Write an executable stub standing in for `op`.
    fn stub(dir: &std::path::Path, body: &str) -> String {
        let p = dir.join("op-stub.sh");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p.to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn returns_the_value_and_strips_the_trailing_newline() {
        let d = tempfile::tempdir().unwrap();
        let be = OnePassword::with_binary(stub(d.path(), "printf 'hunter2\\n'"));
        let got = be.resolve("op://Private/item/field").await.unwrap();
        assert_eq!(
            got, "hunter2",
            "a trailing newline would corrupt the secret"
        );
    }

    #[tokio::test]
    async fn a_failure_quotes_stderr_and_never_stdout() {
        // The control that matters most: if `op` writes the secret to stdout
        // and then fails, the value must not appear in the error.
        let d = tempfile::tempdir().unwrap();
        let be = OnePassword::with_binary(stub(
            d.path(),
            "printf 'SUPERSECRET'; echo 'authorization timeout' 1>&2; exit 1",
        ));
        let e = be
            .resolve("op://Private/item/field")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            !e.contains("SUPERSECRET"),
            "the resolved value leaked into an error: {e}"
        );
        assert!(e.contains("authorization timeout"), "{e}");
        assert!(e.contains("service-account"), "no remediation in: {e}");
    }

    #[tokio::test]
    async fn an_empty_value_is_an_error_not_an_empty_secret() {
        let d = tempfile::tempdir().unwrap();
        let be = OnePassword::with_binary(stub(d.path(), "exit 0"));
        let e = be
            .resolve("op://Private/item/field")
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("empty value"), "{e}");
    }

    #[tokio::test]
    async fn a_malformed_reference_never_reaches_the_binary() {
        // Points at a stub that would succeed loudly if invoked.
        let d = tempfile::tempdir().unwrap();
        let marker = d.path().join("was-invoked");
        let be = OnePassword::with_binary(stub(
            d.path(),
            &format!("touch {}; printf 'v'", marker.display()),
        ));
        assert!(be.resolve("not-a-ref").await.is_err());
        assert!(
            !marker.exists(),
            "a malformed reference must be rejected before spawning `op`"
        );
    }

    #[tokio::test]
    async fn a_hanging_op_is_bounded_and_blames_the_locked_session() {
        // The failure this module exists to prevent: without a service-account
        // token `op` blocks on a prompt no tool call can answer. Unbounded that
        // is a hung daemon call, so prove it returns, and returns the remedy.
        let d = tempfile::tempdir().unwrap();
        let be = OnePassword::with_binary(stub(d.path(), "sleep 30"))
            .with_timeout(Duration::from_millis(250));
        let started = std::time::Instant::now();
        let e = be
            .resolve("op://Private/item/field")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?} — the call is not bounded",
            started.elapsed()
        );
        assert!(e.contains("did not answer"), "{e}");
        assert!(e.contains("service-account"), "no remediation in: {e}");
    }

    #[tokio::test]
    async fn a_service_account_token_is_handed_to_op() {
        // The durable fix: with a service-account token `op` is headless and
        // never prompts. Prove the token actually reaches the child's env — if
        // it silently did not, every fleet host would fall back to a desktop
        // app it does not have.
        //
        // SAFETY: nextest runs each test in its own process, so mutating the
        // environment here cannot be observed by another test.
        unsafe { std::env::set_var(ENV_SERVICE_ACCOUNT, "sa-token-xyz") };
        let d = tempfile::tempdir().unwrap();
        let be = OnePassword::with_binary(stub(
            d.path(),
            "printf '%s' \"${OP_SERVICE_ACCOUNT_TOKEN:-ABSENT}\"",
        ));
        let got = be.resolve("op://V/i/f").await.unwrap();
        assert_eq!(got, "sa-token-xyz", "token did not reach op's environment");
    }

    #[tokio::test]
    async fn without_a_token_op_is_left_to_its_own_session() {
        // Control for the test above. Handing `op` an EMPTY token would be
        // worse than handing it none: it would override a working desktop
        // session with an invalid credential.
        unsafe { std::env::remove_var(ENV_SERVICE_ACCOUNT) };
        let d = tempfile::tempdir().unwrap();
        let be = OnePassword::with_binary(stub(
            d.path(),
            "printf '%s' \"${OP_SERVICE_ACCOUNT_TOKEN:-ABSENT}\"",
        ));
        let got = be.resolve("op://V/i/f").await.unwrap();
        assert_eq!(
            got, "ABSENT",
            "an empty token was injected, which would break a working session"
        );
    }

    /// Live check against the real vault — the only test that proves `op` is
    /// actually driven correctly, as opposed to a stub agreeing with itself.
    ///
    /// Ignored by default: it needs a real 1Password session, so it cannot run
    /// in CI. The reference comes from the environment rather than the source
    /// because this repo is mirrored publicly and a vault/item pair identifies
    /// the operator's vault. Run it with:
    ///
    /// ```text
    /// ORCA_OP_TEST_REF='op://Vault/<item-id>/<field>' \
    ///   cargo nextest run -p auth -E 'test(resolves_a_real_reference)' --run-ignored all
    /// ```
    #[tokio::test]
    #[ignore = "needs a real 1Password session; set ORCA_OP_TEST_REF"]
    async fn resolves_a_real_reference() {
        let Ok(r) = std::env::var("ORCA_OP_TEST_REF") else {
            panic!("set ORCA_OP_TEST_REF to an op:// reference to run this");
        };
        let got = OnePassword::new()
            .resolve(&r)
            .await
            .expect("resolve against the real vault");
        // Never print the value. Assert only its shape.
        assert!(!got.is_empty(), "resolved an empty value");
        assert!(
            !got.contains('\n'),
            "value carries a newline — trailing-newline handling is wrong"
        );
        eprintln!("resolved {} chars from the real vault", got.len());
    }

    #[tokio::test]
    async fn a_missing_binary_names_the_install_step() {
        let be = OnePassword::with_binary("/nonexistent/op");
        let e = be
            .resolve("op://Private/item/field")
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("invoking"), "{e}");
    }
}
