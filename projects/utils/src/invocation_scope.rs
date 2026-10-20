#[cfg(feature = "reactor")]
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerbCaller {
    pub user_id: String,
    pub username: String,
    pub role: String,
    pub can_mutate: bool,
}

#[derive(Debug, Clone)]
pub struct CoreVerb {
    caller: VerbCaller,
    role: &'static str,
    execute_gated: bool,
    dry_run: bool,
    local: bool,
    second_factor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyStolen(pub &'static str);

impl std::fmt::Display for KeyStolen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the {} invocation key was already claimed by another component; refusing to start",
            self.0
        )
    }
}

impl std::error::Error for KeyStolen {}

#[derive(Debug)]
pub struct CoreVerbKey(());

#[derive(Debug)]
pub struct DaemonKey(());

static CORE_VERB_KEY_CLAIMED: AtomicBool = AtomicBool::new(false);
static DAEMON_KEY_CLAIMED: AtomicBool = AtomicBool::new(false);

impl CoreVerbKey {
    pub fn claim() -> Option<Self> {
        (!CORE_VERB_KEY_CLAIMED.swap(true, Ordering::SeqCst)).then_some(Self(()))
    }

    pub fn verb(
        &self,
        caller: VerbCaller,
        role: &'static str,
        execute_gated: bool,
        dry_run: bool,
        local: bool,
        second_factor: bool,
    ) -> CoreVerb {
        CoreVerb::new(caller, role, execute_gated, dry_run, local, second_factor)
    }

    #[cfg(feature = "reactor")]
    pub async fn scope<F: Future>(&self, verb: CoreVerb, f: F) -> F::Output {
        CORE_VERB.scope(verb, f).await
    }
}

impl DaemonKey {
    pub fn claim() -> Option<Self> {
        (!DAEMON_KEY_CLAIMED.swap(true, Ordering::SeqCst)).then_some(Self(()))
    }

    #[cfg(feature = "reactor")]
    pub async fn scope<F: Future>(&self, f: F) -> F::Output {
        daemon_task(f).await
    }
}

impl CoreVerb {
    fn new(
        caller: VerbCaller,
        role: &'static str,
        execute_gated: bool,
        dry_run: bool,
        local: bool,
        second_factor: bool,
    ) -> Self {
        Self {
            caller,
            role,
            execute_gated,
            dry_run,
            local,
            second_factor,
        }
    }

    pub fn caller(&self) -> &VerbCaller {
        &self.caller
    }

    pub fn role(&self) -> &'static str {
        self.role
    }

    pub fn execute_gated(&self) -> bool {
        self.execute_gated
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn local(&self) -> bool {
        self.local
    }

    pub fn second_factor(&self) -> bool {
        self.second_factor
    }
}

#[derive(Debug, Clone)]
pub struct BackendCall(Call);

#[derive(Debug, Clone)]
enum Call {
    Verb { verb: CoreVerb, targeted: bool },
    Daemon,
}

impl BackendCall {
    pub fn targeted(&self) -> bool {
        matches!(self.0, Call::Verb { targeted: true, .. })
    }

    pub fn verb(&self) -> Option<&CoreVerb> {
        match &self.0 {
            Call::Verb { verb, .. } => Some(verb),
            Call::Daemon => None,
        }
    }

    pub fn is_daemon(&self) -> bool {
        matches!(self.0, Call::Daemon)
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
        return in_daemon_task().then_some(BackendCall(Call::Daemon));
    };
    let key = backend_key(domain, name);
    let targeted = TARGET.try_with(|t| *t == key).unwrap_or(false);
    Some(BackendCall(Call::Verb { verb, targeted }))
}

#[cfg(feature = "reactor")]
pub(crate) async fn daemon_task<F: Future>(f: F) -> F::Output {
    DAEMON_TASK.scope((), f).await
}

#[cfg(feature = "reactor")]
pub fn in_daemon_task() -> bool {
    DAEMON_TASK.try_with(|_| ()).is_ok()
}

#[cfg(feature = "reactor")]
pub(crate) fn sync_daemon_task<R>(f: impl FnOnce() -> R) -> R {
    DAEMON_TASK.sync_scope((), f)
}

#[cfg(not(feature = "reactor"))]
pub fn capture(_domain: &str, _name: &str) -> Option<BackendCall> {
    None
}

#[cfg(all(feature = "reactor", any(test, feature = "test-scope")))]
pub mod testing {
    use super::{CORE_VERB, CoreVerb, TARGET, VerbCaller, backend_key};
    use std::future::Future;

    pub fn verb(
        caller: VerbCaller,
        role: &'static str,
        execute_gated: bool,
        dry_run: bool,
        local: bool,
        second_factor: bool,
    ) -> CoreVerb {
        CoreVerb::new(caller, role, execute_gated, dry_run, local, second_factor)
    }

    pub async fn core_verb<F: Future>(verb: CoreVerb, f: F) -> F::Output {
        CORE_VERB.scope(verb, f).await
    }

    pub fn sync_core_verb<R>(verb: CoreVerb, f: impl FnOnce() -> R) -> R {
        CORE_VERB.sync_scope(verb, f)
    }

    pub fn sync_targeting<R>(domain: &str, name: &str, f: impl FnOnce() -> R) -> R {
        TARGET.sync_scope(backend_key(domain, name), f)
    }

    pub fn sync_daemon_task<R>(f: impl FnOnce() -> R) -> R {
        super::sync_daemon_task(f)
    }
}

#[cfg(all(test, feature = "reactor"))]
mod tests {
    use super::testing::{sync_core_verb, sync_targeting};
    use super::*;

    fn verb() -> CoreVerb {
        CoreVerb::new(
            VerbCaller {
                user_id: "u".into(),
                username: "u".into(),
                role: "admin".into(),
                can_mutate: true,
            },
            "admin",
            true,
            false,
            false,
            false,
        )
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
        assert!(daemon.unwrap().is_daemon());
    }

    #[test]
    fn keys_are_claimable_exactly_once() {
        assert!(CoreVerbKey::claim().is_some());
        assert!(CoreVerbKey::claim().is_none());
        assert!(DaemonKey::claim().is_some());
        assert!(DaemonKey::claim().is_none());
    }

    #[tokio::test]
    async fn keys_enter_their_scopes() {
        let core = CoreVerbKey(());
        let v = core.verb(verb().caller().clone(), "admin", true, false, false, false);
        let call = core.scope(v, async { capture("unit", "a") }).await;
        assert!(call.unwrap().verb().is_some());
        let daemon = DaemonKey(());
        assert!(
            daemon
                .scope(async { capture("unit", "a") })
                .await
                .unwrap()
                .is_daemon()
        );
    }
}
