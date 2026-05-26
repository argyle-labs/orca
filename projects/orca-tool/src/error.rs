//! Unified error type for every OrcaTool. The `kind` taxonomy classifies
//! failures so REST/MCP/CLI surfaces can render an appropriate status code
//! and body without re-parsing free-form strings. Construction routes
//! through `tracing::error!` once so all error paths are captured in logs.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Stable classification of a failure mode. Drives HTTP status / CLI exit
/// code / MCP error envelope. ADD new variants here — do NOT introduce
/// parallel taxonomies elsewhere.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    NotFound,
    Invalid,
    Unauthorized,
    Forbidden,
    Conflict,
    Timeout,
    Unavailable,
    Internal,
}

impl ErrorKind {
    pub fn http_status(self) -> u16 {
        match self {
            Self::NotFound => 404,
            Self::Invalid => 400,
            Self::Unauthorized => 401,
            Self::Forbidden => 403,
            Self::Conflict => 409,
            Self::Timeout => 504,
            Self::Unavailable => 503,
            Self::Internal => 500,
        }
    }

    /// CLI exit code (1..125 reserved for shell semantics; 64..78 sysexits).
    pub fn cli_exit_code(self) -> i32 {
        match self {
            Self::NotFound => 66,
            Self::Invalid => 64,
            Self::Unauthorized | Self::Forbidden => 77,
            Self::Conflict => 73,
            Self::Timeout => 75,
            Self::Unavailable => 69,
            Self::Internal => 70,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Invalid => "invalid",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::Conflict => "conflict",
            Self::Timeout => "timeout",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
        }
    }
}

/// Universal orca error. Constructors auto-emit `tracing::error!` so every
/// failure is captured at first construction. Re-throwing via `?` does NOT
/// re-log — From<anyhow::Error> logs once on conversion, then subsequent
/// propagation is silent.
#[derive(Debug, Serialize)]
pub struct OrcaError {
    pub kind: ErrorKind,
    pub message: String,
    /// Stable error code for client matching (e.g. `"secrets.backend_missing"`).
    /// `kind` is the broad bucket; `code` is the specific reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Caller-correlatable context (request id, tool name) — populated by
    /// the surface layer (axum middleware / cli wrapper / mcp handler).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

impl OrcaError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        let message = message.into();
        tracing::error!(
            error.kind = kind.as_str(),
            error.message = %message,
            "OrcaError",
        );
        Self {
            kind,
            message,
            code: None,
            context: None,
        }
    }

    /// Attach a stable machine-readable code. Use for cross-language
    /// clients that need to branch on specific failures (e.g. show a
    /// "set up 1Password" button when `secrets.backend_missing` returns).
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, msg)
    }
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, msg)
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unauthorized, msg)
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Forbidden, msg)
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, msg)
    }
    pub fn timeout(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Timeout, msg)
    }
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unavailable, msg)
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, msg)
    }
}

impl fmt::Display for OrcaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

impl std::error::Error for OrcaError {}

#[cfg(feature = "native")]
impl From<anyhow::Error> for OrcaError {
    fn from(e: anyhow::Error) -> Self {
        // anyhow::Error has no kind info — default to Internal. Tools that
        // want classification should return OrcaError directly with a
        // specific kind instead of bailing through anyhow.
        Self::new(ErrorKind::Internal, e.to_string())
    }
}

impl From<std::io::Error> for OrcaError {
    fn from(e: std::io::Error) -> Self {
        let kind = match e.kind() {
            std::io::ErrorKind::NotFound => ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied => ErrorKind::Forbidden,
            std::io::ErrorKind::TimedOut => ErrorKind::Timeout,
            std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::HostUnreachable
            | std::io::ErrorKind::NetworkUnreachable => ErrorKind::Unavailable,
            std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
                ErrorKind::Invalid
            }
            std::io::ErrorKind::AlreadyExists => ErrorKind::Conflict,
            _ => ErrorKind::Internal,
        };
        Self::new(kind, e.to_string())
    }
}

impl From<serde_json::Error> for OrcaError {
    fn from(e: serde_json::Error) -> Self {
        Self::new(ErrorKind::Invalid, format!("json: {e}"))
    }
}

impl From<&str> for OrcaError {
    fn from(s: &str) -> Self {
        Self::new(ErrorKind::Internal, s)
    }
}

impl From<String> for OrcaError {
    fn from(s: String) -> Self {
        Self::new(ErrorKind::Internal, s)
    }
}

/// Workspace result alias — every OrcaTool fn body returns this.
pub type OrcaResult<T> = std::result::Result<T, OrcaError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_status_codes_are_distinct_enough() {
        assert_eq!(ErrorKind::NotFound.http_status(), 404);
        assert_eq!(ErrorKind::Invalid.http_status(), 400);
        assert_eq!(ErrorKind::Internal.http_status(), 500);
    }

    #[cfg(feature = "native")]
    #[test]
    fn anyhow_conversion_defaults_to_internal() {
        let e: OrcaError = anyhow::anyhow!("boom").into();
        assert_eq!(e.kind, ErrorKind::Internal);
        assert_eq!(e.message, "boom");
    }

    #[test]
    fn io_not_found_maps_to_kind_not_found() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let e: OrcaError = io.into();
        assert_eq!(e.kind, ErrorKind::NotFound);
    }

    #[test]
    fn display_includes_kind_and_message() {
        let e = OrcaError::invalid("bad input");
        assert_eq!(format!("{e}"), "invalid: bad input");
    }

    #[test]
    fn with_code_and_context_attach() {
        let e = OrcaError::not_found("missing")
            .with_code("secrets.not_found")
            .with_context("req-abc");
        assert_eq!(e.code.as_deref(), Some("secrets.not_found"));
        assert_eq!(e.context.as_deref(), Some("req-abc"));
    }
}
