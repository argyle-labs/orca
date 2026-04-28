// Embedded repo documentation — compiled into the binary from docs/ at build time.
// Separate from ~/brain (personal vault/memory); these are project-level WHY docs.
// Accessible via root="docs" in all tree/read/search endpoints and MCP tools.

use serde_json::{Value, json};

#[derive(rust_embed::RustEmbed)]
#[folder = "."]
struct BrainDocs;

pub fn list() -> Vec<String> {
    let mut files: Vec<String> = BrainDocs::iter()
        .filter(|f| f.ends_with(".md"))
        .map(|f| f.into_owned())
        .collect();
    files.sort();
    files
}

pub fn read(path: &str) -> Option<String> {
    let with_ext = if path.ends_with(".md") {
        path.to_string()
    } else {
        format!("{path}.md")
    };
    BrainDocs::get(&with_ext).map(|f| String::from_utf8_lossy(&f.data).into_owned())
}

pub fn search(query: &str) -> Vec<(String, Vec<String>)> {
    let q = query.to_lowercase();
    let mut results = Vec::new();
    for name in BrainDocs::iter() {
        if !name.ends_with(".md") {
            continue;
        }
        if let Some(file) = BrainDocs::get(&name) {
            let content = String::from_utf8_lossy(&file.data);
            let matches: Vec<String> = content
                .lines()
                .enumerate()
                .filter(|(_, l)| l.to_lowercase().contains(&q))
                .take(5)
                .map(|(i, l)| format!("L{}: {}", i + 1, l.trim()))
                .collect();
            if !matches.is_empty() {
                results.push((name.into_owned(), matches));
            }
        }
    }
    results
}

pub fn tree() -> Value {
    let nodes: Vec<Value> = list()
        .into_iter()
        .map(|f| {
            let stem = f.trim_end_matches(".md");
            // Use the H1 heading as the display name if present
            let title = BrainDocs::get(&f)
                .and_then(|file| {
                    let content = String::from_utf8_lossy(&file.data);
                    content
                        .lines()
                        .find(|l| l.starts_with("# "))
                        .map(|l| l[2..].trim().to_string())
                })
                .unwrap_or_else(|| stem.to_string());
            json!({ "name": title, "path": f, "type": "file" })
        })
        .collect();
    json!(nodes)
}

pub fn file_count() -> usize {
    list().len()
}
