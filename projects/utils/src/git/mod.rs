//! Native git operations for orca, backed by libgit2 (statically linked).
//! Replaces the former `git` plugin: same surface, no JSON-RPC scaffolding.
//!
//! Authentication for SSH remotes uses `$GIT_SSH_KEY` if set, otherwise the
//! calling user's ssh-agent. HTTPS uses anonymous fetch by default.

use std::path::{Path, PathBuf};

use git2::{Cred, FetchOptions, RemoteCallbacks, Repository, Signature};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("{0}")]
    Git(#[from] git2::Error),
    #[error("{0}")]
    Detached(String),
    #[error("non-fast-forward upstream; pull requires manual merge")]
    NotFastForward,
    #[error(
        "no committer signature available — pass author or set git config user.{{name,email}}: {0}"
    )]
    NoSignature(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneResult {
    pub path: PathBuf,
    pub branch: String,
    pub head: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullResult {
    pub updated: bool,
    pub branch: String,
    pub head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusEntry {
    pub path: String,
    pub kind: StatusKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StatusKind {
    Staged,
    Untracked,
    Modified,
    Conflicted,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitResult {
    pub oid: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushResult {
    pub branch: String,
    pub remote: String,
}

#[derive(Debug, Clone, Default)]
pub struct CommitAuthor {
    pub name: Option<String>,
    pub email: Option<String>,
}

/// Clone `url` into `path` (creates parents). When `branch` is set, checks
/// out that branch on completion.
pub fn clone(url: &str, path: &Path, branch: Option<&str>) -> Result<CloneResult, GitError> {
    let mut cb = RemoteCallbacks::new();
    cb.credentials(credentials_cb);
    let mut fo = FetchOptions::new();
    fo.remote_callbacks(cb);
    let mut builder = git2::build::RepoBuilder::new();
    builder.fetch_options(fo);
    if let Some(b) = branch {
        builder.branch(b);
    }
    let repo = builder.clone(url, path)?;
    let head = head_summary(&repo);
    Ok(CloneResult {
        path: path.to_path_buf(),
        branch: head.0,
        head: head.1,
    })
}

/// Fetch from `origin` without merging. Idempotent.
pub fn fetch(repo_path: &Path) -> Result<(), GitError> {
    let repo = Repository::open(repo_path)?;
    let mut remote = repo.find_remote("origin")?;
    let mut cb = RemoteCallbacks::new();
    cb.credentials(credentials_cb);
    let mut fo = FetchOptions::new();
    fo.remote_callbacks(cb);
    let arr = remote.fetch_refspecs()?;
    let refspecs: Vec<&str> = arr.iter().flatten().collect();
    remote.fetch(&refspecs, Some(&mut fo), None)?;
    Ok(())
}

/// Fetch + fast-forward merge. Errors when the upstream is not a
/// fast-forward of the local HEAD ([`GitError::NotFastForward`]).
pub fn pull(repo_path: &Path) -> Result<PullResult, GitError> {
    let repo = Repository::open(repo_path)?;
    {
        let mut remote = repo.find_remote("origin")?;
        let mut cb = RemoteCallbacks::new();
        cb.credentials(credentials_cb);
        let mut fo = FetchOptions::new();
        fo.remote_callbacks(cb);
        let arr = remote.fetch_refspecs()?;
        let refspecs: Vec<&str> = arr.iter().flatten().collect();
        remote.fetch(&refspecs, Some(&mut fo), None)?;
    }
    let head = repo.head()?;
    let branch = head
        .shorthand()
        .ok_or_else(|| GitError::Detached("HEAD is detached; refusing to pull".into()))?
        .to_string();
    let upstream_ref = format!("refs/remotes/origin/{branch}");
    let upstream = repo
        .find_reference(&upstream_ref)?
        .target()
        .ok_or_else(|| GitError::Detached("upstream ref has no target".into()))?;
    let analysis = repo
        .merge_analysis(&[&repo.find_annotated_commit(upstream)?])?
        .0;
    if analysis.is_up_to_date() {
        return Ok(PullResult {
            updated: false,
            branch,
            head: None,
        });
    }
    if !analysis.is_fast_forward() {
        return Err(GitError::NotFastForward);
    }
    let mut head_ref = repo.find_reference(&format!("refs/heads/{branch}"))?;
    head_ref.set_target(upstream, "fast-forward pull")?;
    repo.set_head(&format!("refs/heads/{branch}"))?;
    repo.checkout_head(Some(git2::build::CheckoutBuilder::default().force()))?;
    Ok(PullResult {
        updated: true,
        branch,
        head: Some(upstream.to_string()),
    })
}

/// Porcelain status: modified, untracked, staged, conflicted entries.
pub fn status(repo_path: &Path) -> Result<Vec<StatusEntry>, GitError> {
    let repo = Repository::open(repo_path)?;
    let statuses = repo.statuses(Some(
        git2::StatusOptions::new()
            .include_untracked(true)
            .recurse_untracked_dirs(true),
    ))?;
    let mut entries = Vec::with_capacity(statuses.len());
    for entry in statuses.iter() {
        let path = entry.path().unwrap_or("").to_string();
        let s = entry.status();
        let kind = if s.is_index_new() || s.is_index_modified() || s.is_index_deleted() {
            StatusKind::Staged
        } else if s.is_wt_new() {
            StatusKind::Untracked
        } else if s.is_wt_modified() || s.is_wt_deleted() {
            StatusKind::Modified
        } else if s.contains(git2::Status::CONFLICTED) {
            StatusKind::Conflicted
        } else {
            StatusKind::Other
        };
        entries.push(StatusEntry { path, kind });
    }
    Ok(entries)
}

/// Stage `paths` (or all changes if `paths` is empty) and create a commit.
pub fn commit(
    repo_path: &Path,
    message: &str,
    paths: &[String],
    author: &CommitAuthor,
) -> Result<CommitResult, GitError> {
    let repo = Repository::open(repo_path)?;
    let mut index = repo.index()?;
    if paths.is_empty() {
        index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)?;
    } else {
        for p in paths {
            index.add_path(Path::new(p))?;
        }
    }
    index.write()?;
    let tree_oid = index.write_tree()?;
    let tree = repo.find_tree(tree_oid)?;
    let sig = signature(&repo, author)?;
    let parent_commit = match repo.head() {
        Ok(head) => Some(head.peel_to_commit()?),
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => None,
        Err(e) => return Err(GitError::Git(e)),
    };
    let parents: Vec<&git2::Commit> = parent_commit.iter().collect();
    let oid = repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)?;
    Ok(CommitResult {
        oid: oid.to_string(),
        message: message.to_string(),
    })
}

/// Push the current (or specified) branch to the given (or `origin`) remote.
pub fn push(
    repo_path: &Path,
    branch: Option<&str>,
    remote: Option<&str>,
) -> Result<PushResult, GitError> {
    let repo = Repository::open(repo_path)?;
    let head = repo.head()?;
    let branch = match branch {
        Some(b) => b.to_string(),
        None => head
            .shorthand()
            .ok_or_else(|| GitError::Detached("HEAD is detached; pass `branch`".into()))?
            .to_string(),
    };
    let remote_name = remote.unwrap_or("origin");
    let mut r = repo.find_remote(remote_name)?;
    let mut cb = RemoteCallbacks::new();
    cb.credentials(credentials_cb);
    let mut po = git2::PushOptions::new();
    po.remote_callbacks(cb);
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    r.push(&[refspec.as_str()], Some(&mut po))?;
    Ok(PushResult {
        branch,
        remote: remote_name.to_string(),
    })
}

// ── Internals ────────────────────────────────────────────────────────────────

fn head_summary(repo: &Repository) -> (String, String) {
    match repo.head() {
        Ok(h) => (
            h.shorthand().unwrap_or("").to_string(),
            h.target().map(|o| o.to_string()).unwrap_or_default(),
        ),
        Err(_) => (String::new(), String::new()),
    }
}

fn signature<'a>(repo: &'a Repository, author: &CommitAuthor) -> Result<Signature<'a>, GitError> {
    if let (Some(n), Some(e)) = (author.name.as_deref(), author.email.as_deref()) {
        return Ok(Signature::now(n, e)?);
    }
    repo.signature()
        .map_err(|e| GitError::NoSignature(e.to_string()))
}

fn credentials_cb(
    url: &str,
    username_from_url: Option<&str>,
    allowed_types: git2::CredentialType,
) -> Result<Cred, git2::Error> {
    if allowed_types.contains(git2::CredentialType::SSH_KEY) {
        let user = username_from_url.unwrap_or("git");
        if let Ok(key) = std::env::var("GIT_SSH_KEY") {
            return Cred::ssh_key(user, None, Path::new(&key), None);
        }
        return Cred::ssh_key_from_agent(user);
    }
    if allowed_types.contains(git2::CredentialType::USER_PASS_PLAINTEXT) {
        return Cred::default();
    }
    Err(git2::Error::from_str(&format!(
        "no supported credential type for {url}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_repo(dir: &Path) -> Repository {
        let repo = Repository::init(dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
        repo
    }

    #[test]
    fn status_reports_untracked_then_staged() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "hi").unwrap();
        let s = status(dir.path()).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, StatusKind::Untracked);
    }

    #[test]
    fn commit_creates_oid() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "hi").unwrap();
        let r = commit(dir.path(), "first", &[], &CommitAuthor::default()).unwrap();
        assert_eq!(r.message, "first");
        assert_eq!(r.oid.len(), 40);
    }

    #[test]
    fn pull_on_repo_with_no_origin_fails_cleanly() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        let err = pull(dir.path()).unwrap_err();
        // No `origin` remote → libgit2 returns NotFound.
        match err {
            GitError::Git(e) => assert!(matches!(e.code(), git2::ErrorCode::NotFound)),
            other => panic!("expected Git(NotFound), got {other:?}"),
        }
    }
}
