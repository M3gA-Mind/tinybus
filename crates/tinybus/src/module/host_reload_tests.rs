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

// The native entrypoint and broker are real; only its ABI logging callback is
// observed by the test, retaining the SDK's own immutable host context pointer.
static LOGS: OnceLock<Mutex<HashMap<usize, Vec<String>>>> = OnceLock::new();
unsafe extern "C" fn observe_log(ctx: *mut std::ffi::c_void, _: u32, ptr: *const u8, len: usize) {
    let line =
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, len) }).into_owned();
    LOGS.get()
        .unwrap()
        .lock()
        .unwrap()
        .entry(ctx as usize)
        .or_default()
        .push(line);
}
fn observed_native(host: &ModuleHost, path: &Path, prefix: &str) -> usize {
    let artifact = loader::load(path, false).unwrap();
    let manifest = artifact.manifest.clone();
    let admitted = host
        .validate(path, &artifact.descriptor, &manifest)
        .unwrap();
    let config = serde_json::to_vec(&serde_json::json!({"prefix":prefix})).unwrap();
    let (transport, mut vtable) = ModuleTransport::new(admitted.name.clone(), config);
    let key = vtable.host_ctx as usize;
    LOGS.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(key, Vec::new());
    vtable.log = observe_log;
    transport.defer_initialize(artifact.init, vtable);
    host.attach_transport(
        path,
        admitted,
        manifest,
        transport,
        Activation {
            lazy_init: true,
            lazy_load: false,
            descriptor_info: None,
            pinned_sha256: None,
        },
    )
    .unwrap();
    key
}
#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires TINYBUS_TEST_MODULE to point at the built cdylib"]
async fn native_replacement_logs_and_sanitized_panics_use_only_its_original_host_callback() {
    let artifact = PathBuf::from(std::env::var_os("TINYBUS_TEST_MODULE").unwrap());
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
    let first_key = observed_native(&first, &artifact, "first:");
    assert!(
        proxy
            .call::<String>("Now", ())
            .await
            .unwrap()
            .starts_with("first:")
    );
    assert!(
        LOGS.get().unwrap().lock().unwrap()[&first_key]
            .iter()
            .any(|line| line.contains("clock fixture served"))
    );
    first.shutdown(Duration::from_secs(3)).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while caller
            .list_names()
            .await
            .unwrap()
            .iter()
            .any(|name| name.as_str() == "ai.tinyhumans.openhuman.Clock")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(first);
    let first_count = LOGS.get().unwrap().lock().unwrap()[&first_key].len();
    let second = ModuleHost::new(broker);
    let second_key = observed_native(&second, &artifact, "second:");
    assert_ne!(first_key, second_key);
    assert!(
        proxy
            .call::<String>("Now", ())
            .await
            .unwrap()
            .starts_with("second:")
    );
    assert!(proxy.call::<serde_json::Value>("Panic", ()).await.is_err());
    {
        let logs = LOGS.get().unwrap().lock().unwrap();
        assert_eq!(
            logs[&first_key].len(),
            first_count,
            "B must not call A's original logging callback"
        );
        assert!(
            logs[&second_key]
                .iter()
                .any(|line| line.contains("clock fixture served"))
        );
        assert!(
            logs[&second_key]
                .iter()
                .any(|line| line.contains("module panicked at"))
        );
        assert!(
            logs[&second_key]
                .iter()
                .all(|line| !line.contains("secret-token"))
        );
    }
    second.shutdown(Duration::from_secs(3)).await;
    broker_task.abort();
}
