//! Plugin-facing agents facade — the push-direction registration seam.
//!
//! A subprocess plugin contributes agents/hooks/skills/commands/prompt-fragments
//! into core's `agents` domain by building an [`AgentRegistration`] (each vec
//! pre-serialized to a JSON array string) and pushing it over the
//! `agents.register` capability. Core's loader parses the arrays back into the
//! `agents` crate's registry defs and registers a provider under the given name.
//!
//! This mirrors how [`crate::secrets`] delegates `secret.op` to the host: the
//! plugin links no agents machinery, only the wire struct.

use anyhow::{Result, bail};

use crate::abi::AgentRegistration;
use crate::capsink::cap_route;
use crate::serde_json;

/// Push `reg` into core's `agents` domain over the `agents.register` capability.
/// Errors if no capability sink is installed (i.e. this isn't running as a
/// subprocess plugin) or if core rejects the registration.
pub fn register(reg: AgentRegistration) -> Result<()> {
    let op_json = serde_json::to_string(&reg)?;
    match cap_route("agents.register", &op_json) {
        Some(reply) => {
            reply?;
            Ok(())
        }
        None => bail!("agents.register: no capability sink installed"),
    }
}
