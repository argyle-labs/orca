//! Async byte-sink helpers — an orca-owned surface over the executor's async IO
//! traits.
//!
//! A plugin that owns a domain client (e.g. docker → bollard) sometimes gets
//! handed an async writer by that client — an exec stdin stream, an upload
//! body. Driving it directly would make the plugin name the executor's
//! `AsyncWriteExt`; these helpers keep that trait inside the toolkit so the
//! plugin writes bytes through orca instead. tokio is an internal detail here,
//! exactly as in [`crate::process`]. See [[plugins-stay-thin]].

/// Write all `bytes` to `w`, then shut the writer down (flush + close). This is
/// the "send this payload to the child's stdin and signal EOF" sequence a
/// plugin needs after a domain client hands it a writable stream.
///
/// Generic over any async writer the executor produces (e.g. bollard's
/// `Pin<Box<dyn AsyncWrite + Send>>` exec input), so the plugin never names the
/// executor's write extension trait.
pub async fn write_all_and_shutdown<W>(w: &mut W, bytes: &[u8]) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    use tokio::io::AsyncWriteExt;
    w.write_all(bytes).await?;
    w.shutdown().await?;
    Ok(())
}
