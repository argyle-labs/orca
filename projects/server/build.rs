use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");

    let version = resolve_version();
    println!("cargo:rustc-env=ORCA_VERSION={version}");
    watch_git_head(&Path::new(&manifest).join("../.."));

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ORCA_RELEASE_VERSION");
}

fn resolve_git_dirs(repo: &Path) -> Option<(PathBuf, PathBuf)> {
    let dot_git = repo.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let contents = fs::read_to_string(&dot_git).ok()?;
        let rel = contents.trim().strip_prefix("gitdir:")?.trim();
        repo.join(rel)
    };
    let common_dir = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(c) => git_dir.join(c.trim()),
        Err(_) => git_dir.clone(),
    };
    Some((git_dir, common_dir))
}

fn watch_git_head(repo: &Path) {
    let Some((git_dir, common_dir)) = resolve_git_dirs(repo) else {
        return;
    };
    let head = git_dir.join("HEAD");
    let mut watched = vec![head.clone()];
    if let Ok(contents) = fs::read_to_string(&head)
        && let Some(reference) = contents.trim().strip_prefix("ref:")
    {
        let reference = reference.trim();
        watched.push(git_dir.join(reference));
        watched.push(common_dir.join(reference));
    }
    watched.push(git_dir.join("logs").join("HEAD"));
    watched.push(common_dir.join("packed-refs"));
    for path in watched.iter().filter(|p| p.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn resolve_version() -> String {
    if let Ok(v) = env::var("ORCA_RELEASE_VERSION")
        && !v.trim().is_empty()
    {
        return v.trim().trim_start_matches('v').to_string();
    }
    let cargo_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());

    let sha = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    format!("{cargo_version}-dev+g{sha}")
}
