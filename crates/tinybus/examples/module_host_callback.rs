//! Actual eager native fixture calling its fixed host during setup before READY.
use serde::Deserialize;
use tinybus::{Connection, Error, Result};

#[derive(Deserialize)]
struct Config {
    host: tinybus::BusName,
    #[serde(default)]
    fail: bool,
}
async fn setup(connection: Connection, config: Config) -> Result<()> {
    let result = async {
        if config.fail {
            return Err(Error::failed("fixture initialization rejected"));
        }
        let _: tinybus::BusName = connection
            .proxy(config.host.as_str(), "/callback", "org.example.Callback")?
            .call_confidential("Observe", ())
            .await?;
        connection.request_name("org.example.EagerCallback").await
    }
    .await;
    result
}

tinybus_module::module_export! {
    setup = setup, config = Config,
    worker_threads = 1,
    provides = ["org.example.EagerCallback"],
    methods = ["Observe"], signals = [], requires = [], optional = [], lazy = false,
}
