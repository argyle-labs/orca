/// Whether a verb only reads. Drives `REQUIRED_ROLE`, `DATA_MUTATION`, and the
/// dry-run gate together, so a mutating verb cannot go ungated by omission.
///
/// The list is explicit and conservative: a read misclassified as a write only
/// becomes admin-only with a `dryRun`, while a write misclassified as a read
/// mutates unaudited, so anything unlisted gates. A dotted verb is classified by
/// its last segment. `profile` is excluded because `system.profile` sets a
/// plugin's heap-instrumentation flag, which is a write.
pub fn is_read_shaped(verb: &str) -> bool {
    let verb = verb.rsplit('.').next().unwrap_or(verb);
    matches!(
        verb,
        "list"
            | "detail"
            | "search"
            | "info"
            | "health"
            | "status"
            | "tree"
            | "topology"
            | "stat"
            | "read"
            | "logs"
            | "history"
            | "audit"
            | "backends_check"
            | "exports"
            | "entities"
            | "server_info"
            | "diff"
            | "serve_asset"
            | "serve_release"
            | "render"
            | "get"
    )
}
