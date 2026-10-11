//! Actual eager native callback fixture; callback completes before ABI init returns.
use serde::Deserialize;
use tinybus::{Connection, Error, Result};

#[derive(Deserialize)]
struct Config {
    host: tinybus::BusName,
    #[serde(default)]
    fail: bool,
}
static COMPLETED: (std::sync::Mutex<bool>, std::sync::Condvar) =
    (std::sync::Mutex::new(false), std::sync::Condvar::new());

async fn setup(connection: Connection, config: Config) -> Result<()> {
    let result = async {
        if config.fail { return Err(Error::failed("fixture initialization rejected")); }
        let _: tinybus::BusName = connection.proxy(config.host.as_str(), "/callback", "org.example.Callback")?
            .call_confidential("Observe", ()).await?;
        connection.request_name("org.example.EagerCallback").await
    }.await;
    *COMPLETED.0.lock().unwrap() = true;
    COMPLETED.1.notify_one();
    result
}

unsafe fn eager_start<C, F, Fut>(
    host: *const tinybus::module::abi::TbHostVtable,
    out: *mut tinybus::module::abi::TbModuleVtable,
    workers: usize, detach: bool, setup: F,
) -> i32
where
    C: serde::de::DeserializeOwned + Send + 'static,
    F: Fn(Connection, C) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    let code = unsafe { tinybus_module::start_reconfigurable_module::<C, F, Fut>(host, out, workers, detach, setup) };
    if code == tinybus::module::abi::TB_OK {
        let completed = COMPLETED.0.lock().unwrap();
        let (completed, deadline) = COMPLETED.1.wait_timeout_while(completed, std::time::Duration::from_secs(10), |done| !*done).unwrap();
        assert!(*completed && !deadline.timed_out(), "callback must finish before init returns");
    }
    code
}

tinybus_module::module_export! {
    @configured export = {#[unsafe(no_mangle)]},
    start = eager_start,
    setup = setup, config = Config,
    worker_threads = 1,
    provides = ["org.example.EagerCallback"],
    methods = ["Observe"], signals = [], requires = [], optional = [], lazy = false,
}
