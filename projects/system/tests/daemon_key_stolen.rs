use contract::invocation_scope::{DaemonKey, KeyStolen};

#[test]
fn init_refuses_when_the_daemon_key_was_stolen() {
    let _stolen = DaemonKey::claim();
    assert_eq!(system::init(), Err(KeyStolen("daemon")));
}

#[tokio::test]
async fn daemon_tasks_refuse_to_start_without_the_key() {
    let _stolen = DaemonKey::claim();
    system::system_info::spawn_refresher();
    let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = ran.clone();
    let handle = system::periodic::spawn(
        system::periodic::PeriodicSpec {
            name: "stolen-key-test",
            initial_delay: std::time::Duration::ZERO,
            interval: std::time::Duration::from_secs(3600),
        },
        system::periodic::boxed(move || {
            let flag = flag.clone();
            async move {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        }),
    );
    handle.await.unwrap();
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
}
