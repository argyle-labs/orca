//! Extra context for a refused tool argument.

/// Extends an `Invalid` args refusal with what the rejected text names, e.g.
/// the id of the system a hostname belongs to. Argument parsing stays pure; a
/// host that can look the text up registers one on its `ToolCtx`, and dispatch
/// consults it only when a parse fails.
pub trait ArgRefusalHint: Send + Sync {
    /// Text to append to the refusal `message`, or `None` when there is none.
    fn hint(&self, ctx: &crate::ToolCtx, message: &str) -> Option<String>;
}
