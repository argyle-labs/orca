//! Filesystem change notifications. Wraps the cross-platform `notify` crate
//! and exposes a tokio mpsc receiver of [`WatchEvent`] so consumers can
//! `.recv().await` instead of dealing with notify's blocking sync channel.

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Error)]
pub enum WatchError {
    #[error("notify: {0}")]
    Notify(#[from] notify::Error),
}

/// One change event reported by [`watch`]. Coalesces notify's many event
/// kinds into a small enum so callers don't have to match on platform-
/// specific subtleties.
#[derive(Debug, Clone)]
pub struct WatchEvent {
    pub kind: WatchEventKind,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEventKind {
    Created,
    Modified,
    Removed,
    Renamed,
    Other,
}

/// Watch `path` (recursively if a directory) and return a receiver of
/// [`WatchEvent`]s plus the watcher itself. The watcher must be kept alive
/// for the duration of the subscription — drop it to stop.
///
/// `buffer` is the channel capacity (events past it are dropped, oldest
/// first, by tokio mpsc semantics).
pub fn watch(
    path: &Path,
    recursive: bool,
    buffer: usize,
) -> Result<(RecommendedWatcher, mpsc::Receiver<WatchEvent>), WatchError> {
    let (tx, rx) = mpsc::channel::<WatchEvent>(buffer.max(1));
    let mut w = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(ev) = res {
            let kind = match ev.kind {
                EventKind::Create(_) => WatchEventKind::Created,
                EventKind::Modify(notify::event::ModifyKind::Name(_)) => WatchEventKind::Renamed,
                EventKind::Modify(_) => WatchEventKind::Modified,
                EventKind::Remove(_) => WatchEventKind::Removed,
                _ => WatchEventKind::Other,
            };
            // try_send drops on full buffer; that matches the documented
            // "oldest first" coalescing for fast-changing trees.
            tx.try_send(WatchEvent {
                kind,
                paths: ev.paths,
            })
            .ok();
        }
    })?;
    let mode = if recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    w.watch(path, mode)?;
    Ok((w, rx))
}

/// Convenience: same as [`watch`] but returns an `anyhow::Result` and
/// re-uses anyhow context for the path string.
pub fn watch_anyhow(
    path: &Path,
    recursive: bool,
    buffer: usize,
) -> Result<(RecommendedWatcher, mpsc::Receiver<WatchEvent>)> {
    watch(path, recursive, buffer).with_context(|| format!("watch {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    #[tokio::test]
    async fn watch_emits_create_event() {
        let dir = tempdir().unwrap();
        let (_w, mut rx) = watch_anyhow(dir.path(), true, 16).unwrap();
        // Give notify's backend a moment to subscribe before we generate
        // events. Without this the create can race ahead of the watcher.
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::write(dir.path().join("a.txt"), "hi").unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("no event in 2s")
            .expect("channel closed");
        assert!(
            matches!(ev.kind, WatchEventKind::Created | WatchEventKind::Modified),
            "got {:?}",
            ev.kind
        );
        assert!(!ev.paths.is_empty());
    }
}
