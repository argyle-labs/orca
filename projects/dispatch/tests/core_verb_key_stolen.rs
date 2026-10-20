#![cfg(feature = "in-process")]

use contract::invocation_scope::{CoreVerbKey, KeyStolen};

#[test]
fn init_refuses_when_the_core_verb_key_was_stolen() {
    let stolen = CoreVerbKey::claim();
    assert!(stolen.is_some());
    assert_eq!(dispatch::init(), Err(KeyStolen("core-verb")));
    assert_eq!(dispatch::init(), Err(KeyStolen("core-verb")));
}
