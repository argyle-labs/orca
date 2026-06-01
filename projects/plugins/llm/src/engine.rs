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

/// Resolve the backend kind. Empty `supplied` infers from URL — port 11434
/// implies Ollama, otherwise LM Studio. Non-empty must be one of the two.
pub(crate) fn infer_kind(url: &str, supplied: &str) -> anyhow::Result<String> {
    let kind = if supplied.is_empty() {
        if url.contains(":11434") {
            "ollama"
        } else {
            "lmstudio"
        }
        .to_string()
    } else {
        supplied.to_string()
    };
    match kind.as_str() {
        "ollama" | "lmstudio" => Ok(kind),
        other => anyhow::bail!("unknown backend kind '{other}' (want: ollama|lmstudio)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_kind_uses_supplied_value() {
        assert_eq!(infer_kind("http://x", "ollama").unwrap(), "ollama");
        assert_eq!(infer_kind("http://x", "lmstudio").unwrap(), "lmstudio");
    }

    #[test]
    fn infer_kind_defaults_to_lmstudio() {
        assert_eq!(infer_kind("http://localhost:1234", "").unwrap(), "lmstudio");
    }

    #[test]
    fn infer_kind_defaults_to_ollama_on_11434() {
        assert_eq!(infer_kind("http://localhost:11434", "").unwrap(), "ollama");
    }

    #[test]
    fn infer_kind_rejects_unknown() {
        let e = infer_kind("http://x", "bogus").unwrap_err();
        assert!(e.to_string().contains("unknown backend kind"));
    }

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
