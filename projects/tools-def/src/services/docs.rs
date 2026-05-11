//! Service trait for the `docs` domain (root registry + file tree, read,
//! search across all roots + embedded vault, command listing).

use anyhow::Result;
use async_trait::async_trait;

#[derive(Clone)]
pub struct DocRootSummary {
    pub name: String,
    pub path: String,
    pub exists: bool,
    pub doc_count: usize,
}

#[derive(Clone)]
pub enum DocNodeKind {
    File,
    Dir,
}

#[derive(Clone)]
pub struct DocTreeNodeData {
    pub name: String,
    pub path: String,
    pub kind: DocNodeKind,
    pub order: Option<u32>,
    pub children: Option<Vec<DocTreeNodeData>>,
}

#[derive(Clone)]
pub struct SearchDocMatch {
    pub line: u32,
    pub text: String,
}

#[derive(Clone)]
pub struct SearchDocHit {
    pub root: String,
    pub path: String,
    pub matches: Vec<SearchDocMatch>,
}

#[derive(Clone)]
pub struct SearchDocsData {
    pub hits: Vec<SearchDocHit>,
    pub enhanced_summary: Option<String>,
}

#[async_trait]
pub trait DocsService: Send + Sync {
    /// List doc roots from the config + the embedded vault, with file counts.
    async fn list_roots(&self) -> Result<Vec<DocRootSummary>>;

    /// Compacted tree under `root[/path]`. Errors when the root or path is
    /// unknown.
    async fn get_tree(&self, root: &str, path: Option<&str>) -> Result<Vec<DocTreeNodeData>>;

    /// Read a doc file. When `llm_format` is true, decorative markdown is
    /// stripped to reduce token usage.
    async fn read_doc(&self, root: &str, path: &str, llm_format: bool) -> Result<String>;

    /// Search docs across one or all roots (filter == "all" matches every
    /// configured root + the embedded vault).
    async fn search_docs(
        &self,
        query: &str,
        filter: &str,
        llm_format: bool,
    ) -> Result<SearchDocsData>;

    /// All embedded slash-command / skill basenames in the orca vault.
    async fn list_commands(&self) -> Result<Vec<String>>;
}
