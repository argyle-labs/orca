//! Server-side glue for the `llm` crate. The runtime/backends/tools have
//! moved to `projects/llm/`; this module only holds the `agent_backend`
//! service impl that bridges the `agents` crate's trait to `llm::resolve`.
pub mod agent_backend_service;
