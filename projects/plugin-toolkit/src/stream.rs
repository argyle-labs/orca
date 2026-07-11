//! Async stream consumption — an orca-owned surface over the executor's async
//! `Stream`.
//!
//! A plugin that owns a domain client (docker → bollard) is often handed an
//! async stream by that client — container logs, an exec's interleaved
//! stdout/stderr, an event feed. Draining it directly would make the plugin name
//! `futures`'s `StreamExt`. [`next`] keeps that dependency inside the toolkit, so
//! the plugin pulls items through orca and owns only its domain client. tokio /
//! futures are internal details here, exactly as in [`crate::process`] /
//! [`crate::io`]. See [[plugins-stay-thin]].

use std::future::poll_fn;
use std::pin::Pin;

use futures_core::Stream;

/// Pull the next item from `stream`, or `None` once it ends — the orca-owned
/// equivalent of `StreamExt::next`, so a plugin can `while let Some(item) =
/// stream::next(&mut s).await { … }` over a domain client's stream without
/// naming the executor's stream extension trait.
///
/// Generic over any async stream the executor/domain client produces (e.g.
/// bollard's boxed `Pin<Box<dyn Stream + Send>>` log/exec streams).
pub async fn next<S>(stream: &mut S) -> Option<S::Item>
where
    S: Stream + Unpin + ?Sized,
{
    poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}
