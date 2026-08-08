//! Cross-cutting operational-health primitive shared across every surface.
//!
//! [`Health`] is the single typed answer to "is this thing working?" that
//! runtime adapters, service providers, and pod/system projections all populate
//! and consumers all read — so no domain reinvents its own health enum.
//!
//! ## Layered precedence rule
//!
//! A consumer that populates [`Health`] must follow this precedence, most
//! authoritative first:
//!
//! 1. **Native runtime HEALTHCHECK** — the container runtime's own health
//!    status when the image defines a HEALTHCHECK (docker `.State.Health`).
//! 2. **orca plugin-declared probe** — else an orca-declared probe
//!    (HTTP / TCP / exec) the owning plugin runs against the thing.
//! 3. **[`Health::NotApplicable`]** — else, when neither a native check nor a
//!    declared probe exists, the thing has no health signal to report.
//!
//! [`Health::Unknown`] is distinct from `NotApplicable`: it means the signal
//! *applies* but has **not yet been determined** (no report has landed).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Operational health of a thing (container, service, host, …). Populated per
/// the layered precedence rule documented on this module.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// Working as intended — the native check or declared probe passes.
    Healthy,
    /// Coming up — inside its start-period / warm-up window, not yet Healthy.
    Starting,
    /// Working but impaired — some checks fail while it still serves.
    Degraded,
    /// Not working — the native check or declared probe fails.
    Unhealthy,
    /// The signal applies but has not yet been determined (no report yet).
    #[default]
    Unknown,
    /// No health signal applies — neither a native check nor a declared probe.
    NotApplicable,
}

impl Health {
    /// Stable short string used in tool output, log lines, and route matchers.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Starting => "starting",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
            Self::Unknown => "unknown",
            Self::NotApplicable => "not_applicable",
        }
    }
}
