// Embedded repo documentation — compiled into the binary from docs/ at build time.
// Separate from ~/orca (personal vault/memory); these are project-level WHY docs.
// Accessible via root="docs" in all tree/read/search endpoints and MCP tools.

use serde_json::{Value, json};

#[derive(rust_embed::RustEmbed)]
#[folder = "src"]
struct OrcaDocs;

pub fn list() -> Vec<String> {
    let mut files: Vec<String> = OrcaDocs::iter()
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
    OrcaDocs::get(&with_ext).map(|f| String::from_utf8_lossy(&f.data).into_owned())
}

pub fn search(query: &str) -> Vec<(String, Vec<String>)> {
    let q = query.to_lowercase();
    let mut results = Vec::new();
    for name in OrcaDocs::iter() {
        if !name.ends_with(".md") {
            continue;
        }
        if let Some(file) = OrcaDocs::get(&name) {
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

fn doc_title(path: &str) -> String {
    OrcaDocs::get(path)
        .and_then(|f| {
            let content = String::from_utf8_lossy(&f.data);
            content
                .lines()
                .find(|l| l.starts_with("# "))
                .map(|l| l[2..].trim().to_string())
        })
        .unwrap_or_else(|| {
            let stem = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".md");
            stem.replace('-', " ")
        })
}

pub fn tree() -> Value {
    use std::collections::BTreeMap;

    let mut top_files: Vec<Value> = Vec::new();
    let mut dirs: BTreeMap<String, Vec<Value>> = BTreeMap::new();

    for path in list() {
        match path.splitn(2, '/').collect::<Vec<_>>().as_slice() {
            [dir, _] if path.contains('/') => {
                let dir = dir.to_string();
                dirs.entry(dir).or_default().push(json!({
                    "name": doc_title(&path),
                    "path": path,
                    "type": "file"
                }));
            }
            _ => {
                top_files.push(json!({
                    "name": doc_title(&path),
                    "path": path,
                    "type": "file"
                }));
            }
        }
    }

    let mut nodes = top_files;
    for (dir_name, children) in dirs {
        nodes.push(json!({
            "name": dir_name,
            "path": dir_name,
            "type": "dir",
            "children": children
        }));
    }
    json!(nodes)
}

pub fn file_count() -> usize {
    list().len()
}
