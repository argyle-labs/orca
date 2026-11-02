use std::sync::{Arc, Mutex};

use plugin_toolkit::capsink::{SharedCapSink, with_process_cap_sink};
use plugin_toolkit::client::{Client, Request};

// The process-wide sink is global; tests in this binary install it one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn sink() -> SharedCapSink {
    Arc::new(|cap: &str, _op: &str| {
        assert_eq!(cap, "http.request");
        Ok(r#"{"status":200,"headers":[],"body":[111,107]}"#.to_string())
    })
}

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

#[test]
fn capability_call_from_spawn_blocking_reaches_the_sink() {
    let _serial = serial();
    let resp = with_process_cap_sink(sink(), || {
        plugin_toolkit::reactor::block_on(async {
            tokio::task::spawn_blocking(|| Client::new().get("http://h/"))
                .await
                .unwrap()
        })
    });
    assert_eq!(resp.unwrap().text(), "ok");
}

#[test]
fn capability_call_from_a_spawned_task_reaches_the_sink() {
    let _serial = serial();
    let resp = with_process_cap_sink(sink(), || {
        plugin_toolkit::reactor::block_on(async {
            tokio::spawn(async { Client::new().get("http://h/") })
                .await
                .unwrap()
        })
    });
    assert_eq!(resp.unwrap().text(), "ok");
}

#[test]
fn async_capability_call_from_a_spawned_task_reaches_the_sink() {
    let _serial = serial();
    let resp = with_process_cap_sink(sink(), || {
        plugin_toolkit::reactor::block_on(async {
            tokio::spawn(async {
                Client::new()
                    .send_async(Request::new("GET", "http://h/"))
                    .await
            })
            .await
            .unwrap()
        })
    });
    assert_eq!(resp.unwrap().text(), "ok");
}

#[test]
fn process_sink_is_gone_after_its_scope() {
    let _serial = serial();
    with_process_cap_sink(sink(), || ());
    let err = std::thread::spawn(|| Client::new().get("http://h/"))
        .join()
        .unwrap()
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("not running as an orca subprocess"),
        "got: {err}"
    );
}
