//! Native SDK reload lifecycle regressions using the real compiled clock fixture.
use super::*;
use crate::{Connection, transport::memory::MemoryBus};

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires TINYBUS_TEST_MODULE to point at the built cdylib"]
async fn a_stopped_native_sdk_instance_can_be_replaced_after_all_names_are_released() {
    let artifact = std::env::var_os("TINYBUS_TEST_MODULE").expect("TINYBUS_TEST_MODULE");
    let bus = MemoryBus::new();
    let broker = Broker::new();
    let broker_task = broker.clone().spawn(bus.clone());
    let caller = Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    let proxy = caller
        .proxy(
            "ai.tinyhumans.openhuman.Clock",
            "/ai/tinyhumans/openhuman/Clock",
            "ai.tinyhumans.openhuman.Clock",
        )
        .unwrap();
    let first = ModuleHost::new(broker.clone());
    let info = first
        .load_file_with_config(&artifact, serde_json::json!({"prefix":"first:"}))
        .unwrap();
    let value: String = proxy.call("Now", ()).await.unwrap();
    assert!(value.starts_with("first:"));
    first.shutdown(Duration::from_secs(3)).await;
    assert!(matches!(first.list()[0].state, ModuleState::Stopped));
    // Prove broker detach has released BOTH the manifest name and setup alias.
    // A replacement cannot confuse this test with an intentional live collision.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let names = caller.list_names().await.unwrap();
            if !names.iter().any(|name| {
                name == &info.manifest.bus_name || name.as_str() == "ai.tinyhumans.openhuman.Clock"
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stopped module releases all names");
    drop(first);
    let second = ModuleHost::new(broker);
    second
        .load_file_with_config(&artifact, serde_json::json!({"prefix":"second:"}))
        .unwrap();
    let result = proxy.call::<String>("Now", ()).await;
    assert!(
        result
            .as_ref()
            .is_ok_and(|value| value.starts_with("second:")),
        "same native artifact must initialize a fresh instance after complete detach: {result:?}; state={:?}",
        second.list()
    );
    second.shutdown(Duration::from_secs(3)).await;
    broker_task.abort();
}
