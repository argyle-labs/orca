//! `DocsService` impl — wraps `crate::mcp::docs` helpers and the embedded
//! vault accessor `crate::docs`.
#![allow(clippy::disallowed_types)] // forced by docs::tree() / build_doc_tree() / collect_all_doc_files() returning Value

use anyhow::Result;
use async_trait::async_trait;
use orca_tools_def::services::docs::{
    DocNodeKind, DocRootSummary, DocRootTree, DocTreeNodeData, DocsService, SearchDocHit,
    SearchDocMatch, SearchDocsData,
};
use orca_utils::config::Config;
use std::sync::Arc;

use crate::markdown::to_llm_text;
use crate::mcp::docs as docs_mod;
use crate::serve::api::llm as local_llm;
use crate::serve::tree::{NodeType, TreeNode};

pub struct ServerDocs {
    pub config: Arc<Config>,
}

fn node_to_data(node: &TreeNode) -> DocTreeNodeData {
    DocTreeNodeData {
        name: node.name.clone(),
        path: node.path.clone(),
        kind: match node.node_type {
            NodeType::File => DocNodeKind::File,
            NodeType::Dir => DocNodeKind::Dir,
        },
        order: node.order,
        children: node
            .children
            .as_ref()
            .map(|cs| cs.iter().map(node_to_data).collect()),
    }
}

fn value_to_tree_node(v: &serde_json::Value) -> Option<TreeNode> {
    serde_json::from_value(v.clone()).ok()
}

#[async_trait]
impl DocsService for ServerDocs {
    async fn list_roots(&self) -> Result<Vec<DocRootSummary>> {
        let roots = docs_mod::doc_roots(&self.config);
        let mut out: Vec<DocRootSummary> = roots
            .iter()
            .map(|r| {
                let exists = r.path.exists();
                let doc_count = if exists {
                    docs_mod::count_doc_files(&docs_mod::build_doc_tree(
                        &r.path, &r.path, &r.ignored,
                    ))
                } else {
                    0
                };
                DocRootSummary {
                    name: r.name.clone(),
                    path: r.path.to_string_lossy().into_owned(),
                    exists,
                    doc_count,
                }
            })
            .collect();
        out.push(DocRootSummary {
            name: "docs".to_string(),
            path: "(embedded in binary)".to_string(),
            exists: true,
            doc_count: crate::docs::file_count(),
        });
        Ok(out)
    }

    async fn get_tree(&self, root: &str, path: Option<&str>) -> Result<Vec<DocTreeNodeData>> {
        if root == "docs" {
            let tree_value = crate::docs::tree();
            // crate::docs::tree() returns serde_json::Value array of nodes.
            let arr = tree_value.as_array().cloned().unwrap_or_default();
            return Ok(arr
                .iter()
                .filter_map(value_to_tree_node)
                .map(|n| node_to_data(&n))
                .collect());
        }

        let roots = docs_mod::doc_roots(&self.config);
        let r = roots
            .iter()
            .find(|r| r.name == root)
            .ok_or_else(|| anyhow::anyhow!("unknown root: {root}"))?;

        let dir = match path {
            Some(p) => docs_mod::resolve_within_root(&r.path, p)?,
            None => r.path.clone(),
        };
        let compact =
            docs_mod::compact_doc_tree(docs_mod::build_doc_tree(&dir, &r.path, &r.ignored));
        Ok(compact
            .iter()
            .filter_map(value_to_tree_node)
            .map(|n| node_to_data(&n))
            .collect())
    }

    async fn get_full_tree(&self, raw: bool) -> Result<Vec<DocRootTree>> {
        let roots = crate::serve::tree::get_roots();
        let mut out: Vec<DocRootTree> = Vec::new();
        for name in roots.keys() {
            let nodes = if raw {
                crate::serve::tree::get_root_tree_raw(name)
            } else {
                crate::serve::tree::get_root_tree(name)
            };
            out.push(DocRootTree {
                root: name.clone(),
                nodes: nodes.iter().map(node_to_data).collect(),
            });
        }
        Ok(out)
    }

    async fn read_doc(&self, root: &str, path: &str, llm_format: bool) -> Result<String> {
        let apply = |s: String| if llm_format { to_llm_text(&s) } else { s };

        if root == "docs" {
            return crate::docs::read(path)
                .map(apply)
                .ok_or_else(|| anyhow::anyhow!("not found: docs/{path}"));
        }

        let roots = docs_mod::doc_roots(&self.config);
        let r = roots
            .iter()
            .find(|r| r.name == root)
            .ok_or_else(|| anyhow::anyhow!("unknown root: {root}"))?;

        let full = docs_mod::resolve_doc_file(&r.path, path)
            .ok_or_else(|| anyhow::anyhow!("not found: {root}/{path}"))?;
        Ok(apply(std::fs::read_to_string(full)?))
    }

    async fn search_docs(
        &self,
        query: &str,
        filter: &str,
        llm_format: bool,
    ) -> Result<SearchDocsData> {
        let all_roots = docs_mod::doc_roots(&self.config);
        let roots: Vec<&docs_mod::DocRoot> = all_roots
            .iter()
            .filter(|r| filter == "all" || r.name == filter)
            .collect();
        let query_lower = query.to_lowercase();
        let mut hits: Vec<SearchDocHit> = Vec::new();

        for root in roots {
            if !root.path.exists() {
                continue;
            }
            let files = docs_mod::collect_all_doc_files(&docs_mod::build_doc_tree(
                &root.path,
                &root.path,
                &root.ignored,
            ));
            for file in files {
                let rel = file["path"].as_str().unwrap_or("").to_string();
                let full = root.path.join(&rel);
                let Ok(content) = std::fs::read_to_string(&full) else {
                    continue;
                };
                let matches: Vec<SearchDocMatch> = content
                    .lines()
                    .enumerate()
                    .filter(|(_, l)| l.to_lowercase().contains(&query_lower))
                    .take(5)
                    .map(|(i, l)| SearchDocMatch {
                        line: (i + 1) as u32,
                        text: if llm_format {
                            to_llm_text(l.trim()).trim_end_matches('\n').to_string()
                        } else {
                            l.trim().to_string()
                        },
                    })
                    .collect();
                if !matches.is_empty() {
                    hits.push(SearchDocHit {
                        root: root.name.clone(),
                        path: rel,
                        matches,
                    });
                }
            }
        }

        if filter == "all" || filter == "docs" {
            for (path, line_matches) in crate::docs::search(query) {
                let matches: Vec<SearchDocMatch> = line_matches
                    .into_iter()
                    .enumerate()
                    .map(|(i, l)| SearchDocMatch {
                        line: (i + 1) as u32,
                        text: if llm_format {
                            to_llm_text(l.trim()).trim_end_matches('\n').to_string()
                        } else {
                            l
                        },
                    })
                    .collect();
                hits.push(SearchDocHit {
                    root: "docs".to_string(),
                    path,
                    matches,
                });
            }
        }

        let enhanced_summary = if !hits.is_empty()
            && let Some(llm) = local_llm::discover_local_llm().await
        {
            let raw = hits
                .iter()
                .map(|h| {
                    let lines: Vec<String> = h
                        .matches
                        .iter()
                        .map(|m| format!("L{}: {}", m.line, m.text))
                        .collect();
                    format!("{}/{}\n{}", h.root, h.path, lines.join("\n"))
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            local_llm::present_text_results(&llm, query, &raw, 8000).await
        } else {
            None
        };

        Ok(SearchDocsData {
            hits,
            enhanced_summary,
        })
    }

    async fn list_commands(&self) -> Result<Vec<String>> {
        Ok(crate::commands::list_embedded_commands())
    }
}
