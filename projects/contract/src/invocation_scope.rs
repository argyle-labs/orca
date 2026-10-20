use std::future::Future;
use std::sync::{Arc, OnceLock};

use crate::CallerIdentity;

#[derive(Debug, Clone)]
pub struct CoreVerb {
    pub caller: CallerIdentity,
    pub role: &'static str,
    pub execute_gated: bool,
    pub dry_run: bool,
    pub local: bool,
    pub second_factor: bool,
    target: Arc<OnceLock<String>>,
}

impl CoreVerb {
    pub fn new(
        caller: CallerIdentity,
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
            target: Arc::new(OnceLock::new()),
        }
    }

    pub fn targets(&self, backend: &str) -> bool {
        self.target.get_or_init(|| backend.to_string()) == backend
    }
}

tokio::task_local! {
    static CORE_VERB: CoreVerb;
}

pub async fn core_verb<F: Future>(verb: CoreVerb, f: F) -> F::Output {
    CORE_VERB.scope(verb, f).await
}

pub fn current_core_verb() -> Option<CoreVerb> {
    CORE_VERB.try_with(Clone::clone).ok()
}

pub fn sync_core_verb<R>(verb: CoreVerb, f: impl FnOnce() -> R) -> R {
    CORE_VERB.sync_scope(verb, f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verb() -> CoreVerb {
        let caller = CallerIdentity {
            user_id: "u".into(),
            username: "u".into(),
            role: "admin".into(),
            can_mutate: true,
        };
        CoreVerb::new(caller, "admin", true, false, false, false)
    }

    #[test]
    fn a_core_verb_binds_to_its_first_backend() {
        let v = verb();
        let copy = v.clone();
        assert!(v.targets("p/unit/a"));
        assert!(copy.targets("p/unit/a"));
        assert!(!copy.targets("p/unit/b"));
    }

    #[test]
    fn scopes_are_only_visible_inside() {
        assert!(current_core_verb().is_none());
        assert!(sync_core_verb(verb(), || current_core_verb().is_some()));
    }
}
