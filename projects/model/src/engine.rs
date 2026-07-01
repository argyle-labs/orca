//! LLM backend registry (LM Studio, Ollama).
//!
//! Reads (the engine list) surface as `system.detail.engines`. Writes
//! (add/remove/enable/disable) are flags on `system.update`. There is no
//! `system.engine.*` orca_tool — engines are configuration of the system,
//! not a separate resource.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct ProviderDto {
    pub name: String,
    pub url: String,
    pub kind: String,
    pub enabled: bool,
    pub created_at: String,
}

impl From<db::llm::Provider> for ProviderDto {
    fn from(p: db::llm::Provider) -> Self {
        Self {
            name: p.name,
            url: p.url,
            kind: p.kind,
            enabled: p.enabled,
            created_at: p.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_dto_from_db_row_copies_fields() {
        let dto: ProviderDto = db::llm::Provider {
            name: "n".into(),
            url: "u".into(),
            kind: "ollama".into(),
            enabled: true,
            created_at: "ts".into(),
        }
        .into();
        assert_eq!(dto.name, "n");
        assert_eq!(dto.url, "u");
        assert_eq!(dto.kind, "ollama");
        assert!(dto.enabled);
        assert_eq!(dto.created_at, "ts");
    }
}
