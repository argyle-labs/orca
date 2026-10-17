pub fn is_read_shaped(verb: &str) -> bool {
    // A dotted verb is classified by its LAST segment: `remediation.get` is a
    // `get`, `update.status` is a `status`. Matching the whole string made both
    // read as write-shaped, so #658 turned two pure reads into admin-only,
    // execute-gated data mutations — reading the remediation policy started
    // demanding `--execute`.
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
