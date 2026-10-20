//! Actual eager native fixture exercising confidential callbacks and the
//! separately declared ordinary stream callback capability.
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tinybus::{
    BusName, Connection, Error, Interface, InterfaceName, MemberName, Result, StreamDescriptor,
    StreamRef,
};

#[derive(Deserialize)]
struct Config {
    host: BusName,
}

struct CallbackService(Connection, BusName);

#[async_trait]
impl Interface for CallbackService {
    fn name(&self) -> InterfaceName {
        "org.example.EagerCallback".parse().unwrap()
    }
    fn members(&self) -> Vec<MemberName> {
        vec!["RoundTrip".parse().unwrap()]
    }
    async fn call(&self, member: &MemberName, args: Value) -> Result<Value> {
        if member.as_str() != "RoundTrip" || args != serde_json::json!([]) {
            return Err(Error::failed("unknown callback fixture request"));
        }
        let callback = self
            .0
            .proxy(self.1.as_str(), "/callback", "org.example.Callback")?;

        // Ordinary traffic cannot enter a confidential-only member.
        if callback.call::<Value>("Observe", ()).await.is_ok() {
            return Err(Error::failed("ordinary confidential callback was accepted"));
        }
        if callback.call::<Value>("Undeclared", ()).await.is_ok() {
            return Err(Error::failed("undeclared callback member was accepted"));
        }

        let mut outgoing = self
            .0
            .open_stream(&self.1, StreamDescriptor::with_len(12))
            .await?;
        let outgoing_ref = outgoing.stream_ref();
        let write_outgoing = tokio::spawn(async move {
            outgoing.write(b"module->host").await?;
            outgoing.finish().await
        });
        let incoming_ref: StreamRef = callback.call("Transfer", (outgoing_ref,)).await?;
        write_outgoing
            .await
            .map_err(|_| Error::failed("module stream writer task failed"))??;
        let mut incoming = self.0.accept_stream_from(&incoming_ref, &self.1)?;
        if incoming.read_to_end_capped(12).await? != b"host->module" {
            return Err(Error::failed("host callback stream payload did not match"));
        }

        callback.call_confidential::<Value>("Observe", ()).await?;
        if callback
            .call::<Value>(
                "Transfer",
                (StreamRef {
                    id: "undeclared-test-handle".into(),
                    content_type: None,
                    len: Some(0),
                },),
            )
            .await
            .is_ok()
        {
            return Err(Error::failed("revoked stream callback was accepted"));
        }
        Ok(Value::Null)
    }
}

async fn setup(connection: Connection, config: Config) -> Result<()> {
    connection
        .serve_at(
            "/module".parse()?,
            CallbackService(connection.clone(), config.host),
        )
        .await?;
    connection.request_name("org.example.EagerCallback").await
}

tinybus_module::module_export! {
    setup = setup, config = Config,
    worker_threads = 1,
    provides = ["org.example.EagerCallback"],
    methods = ["RoundTrip"], signals = [], requires = [], optional = [], lazy = false,
}
