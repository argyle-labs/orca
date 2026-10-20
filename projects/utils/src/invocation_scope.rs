#[cfg(feature = "reactor")]
use std::future::Future;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerbCaller {
    pub user_id: String,
    pub username: String,
    pub role: String,
    pub can_mutate: bool,
}

#[derive(Debug, Clone)]
pub struct CoreVerb {
    pub caller: VerbCaller,
    pub role: &'static str,
    pub execute_gated: bool,
    pub dry_run: bool,
    pub local: bool,
    pub second_factor: bool,
}

#[derive(Debug, Clone)]
pub enum BackendCall {
    Verb { verb: CoreVerb, targeted: bool },
    Daemon,
}

impl BackendCall {
    pub fn targeted(&self) -> bool {
        matches!(self, BackendCall::Verb { targeted: true, .. })
    }
}

pub fn backend_key(domain: &str, name: &str) -> String {
    format!("{domain}/{name}")
}

#[cfg(feature = "reactor")]
tokio::task_local! {
    static CORE_VERB: CoreVerb;
    static TARGET: String;
    static DAEMON_TASK: ();
}

#[cfg(feature = "reactor")]
pub async fn core_verb<F: Future>(verb: CoreVerb, f: F) -> F::Output {
    CORE_VERB.scope(verb, f).await
}

#[cfg(feature = "reactor")]
pub fn current_core_verb() -> Option<CoreVerb> {
    CORE_VERB.try_with(Clone::clone).ok()
}

#[cfg(feature = "reactor")]
pub async fn targeting<F: Future>(domain: &str, name: &str, f: F) -> F::Output {
    TARGET.scope(backend_key(domain, name), f).await
}

#[cfg(feature = "reactor")]
pub fn capture(domain: &str, name: &str) -> Option<BackendCall> {
    let Some(verb) = current_core_verb() else {
        return in_daemon_task().then_some(BackendCall::Daemon);
    };
    let key = backend_key(domain, name);
    let targeted = TARGET.try_with(|t| *t == key).unwrap_or(false);
    Some(BackendCall::Verb { verb, targeted })
}

#[cfg(feature = "reactor")]
pub async fn daemon_task<F: Future>(f: F) -> F::Output {
    DAEMON_TASK.scope((), f).await
}

#[cfg(feature = "reactor")]
pub fn in_daemon_task() -> bool {
    DAEMON_TASK.try_with(|_| ()).is_ok()
}

#[cfg(feature = "reactor")]
pub fn sync_daemon_task<R>(f: impl FnOnce() -> R) -> R {
    DAEMON_TASK.sync_scope((), f)
}

#[cfg(not(feature = "reactor"))]
pub fn capture(_domain: &str, _name: &str) -> Option<BackendCall> {
    None
}

#[cfg(feature = "reactor")]
pub fn sync_core_verb<R>(verb: CoreVerb, f: impl FnOnce() -> R) -> R {
    CORE_VERB.sync_scope(verb, f)
}

#[cfg(feature = "reactor")]
pub fn sync_targeting<R>(domain: &str, name: &str, f: impl FnOnce() -> R) -> R {
    TARGET.sync_scope(backend_key(domain, name), f)
}

#[cfg(all(test, feature = "reactor"))]
mod tests {
    use super::*;

    fn verb() -> CoreVerb {
        CoreVerb {
            caller: VerbCaller {
                user_id: "u".into(),
                username: "u".into(),
                role: "admin".into(),
                can_mutate: true,
            },
            role: "admin",
            execute_gated: true,
            dry_run: false,
            local: false,
            second_factor: false,
        }
    }

    #[test]
    fn only_the_declared_target_is_targeted() {
        assert!(capture("unit", "a").is_none());
        sync_core_verb(verb(), || {
            assert!(!capture("unit", "a").unwrap().targeted());
            sync_targeting("unit", "b", || {
                assert!(!capture("unit", "a").unwrap().targeted());
                assert!(capture("unit", "b").unwrap().targeted());
                assert!(!capture("storage", "b").unwrap().targeted());
            });
        });
        let daemon = sync_daemon_task(|| capture("unit", "a"));
        assert!(matches!(daemon, Some(BackendCall::Daemon)));
    }
}
